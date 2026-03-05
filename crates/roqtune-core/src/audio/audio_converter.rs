//! Batch audio conversion: decodes a source file with Symphonia and re-encodes
//! it to WAV, FLAC, Opus, or MP3.
//!
//! [`convert_audio_file`] is the single public entry point. It runs
//! synchronously on the caller's thread (the batch-file-op worker thread).

use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};
use log::warn;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::conversion_config::{
    ConversionConfig, ConversionFormat, FlacBitDepth, Mp3Mode, WavBitDepth,
};

// Opus always encodes at 48 kHz.
const OPUS_SAMPLE_RATE: u32 = 48_000;
// Opus frame size: 20 ms at 48 kHz.
const OPUS_FRAME_SAMPLES: usize = 960;
// Resample chunk used when converting to 48 kHz for Opus.
const RESAMPLE_CHUNK_FRAMES: usize = 4096;

/// Decoded raw PCM from Symphonia before any format-specific processing.
struct DecodedAudio {
    /// Interleaved f32 samples in the range [-1.0, 1.0].
    samples: Vec<f32>,
    sample_rate: u32,
    channels: usize,
}

// ─── Public entry point ──────────────────────────────────────────────────────

/// Decode `source_path` and encode the result to `dest_path` according to
/// `config`.  Tags are copied from the source file to the destination after
/// encoding using lofty.
///
/// Returns `Ok(())` on success, `Err(human-readable message)` on failure.
pub fn convert_audio_file(
    source_path: &Path,
    dest_path: &Path,
    config: &ConversionConfig,
) -> Result<(), String> {
    let decoded = decode_file(source_path)?;

    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create output directory: {e}"))?;
    }

    match config.format {
        ConversionFormat::Wav => encode_wav(&decoded, &config.wav, dest_path)?,
        ConversionFormat::Flac => encode_flac(&decoded, &config.flac, dest_path)?,
        ConversionFormat::Opus => {
            let resampled = resample_to_48k(decoded)?;
            encode_opus(&resampled, &config.opus, dest_path, source_path)?;
        }
        ConversionFormat::Mp3 => encode_mp3(&decoded, &config.mp3, dest_path)?,
    }

    // Transfer tags with lofty for all non-Opus formats.
    // For Opus the tags are written into the OGG header inside encode_opus.
    if config.format != ConversionFormat::Opus {
        if let Err(e) = transfer_tags(source_path, dest_path) {
            warn!("Tag transfer failed for {:?}: {e}", dest_path);
        }
    }

    Ok(())
}

// ─── Decode ──────────────────────────────────────────────────────────────────

fn decode_file(path: &Path) -> Result<DecodedAudio, String> {
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let file = std::fs::File::open(path).map_err(|e| format!("Cannot open {:?}: {e}", path))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut format_reader = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Failed to probe {:?}: {e}", path))?
        .format;

    // Select the first decodable audio track.
    let mut selected = None;
    if let Some(t) = format_reader.default_track() {
        let id = t.id;
        let params = t.codec_params.clone();
        if let Ok(dec) = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())
        {
            selected = Some((id, params, dec));
        }
    }
    if selected.is_none() {
        for t in format_reader.tracks() {
            let id = t.id;
            let params = t.codec_params.clone();
            if let Ok(dec) =
                symphonia::default::get_codecs().make(&params, &DecoderOptions::default())
            {
                selected = Some((id, params, dec));
                break;
            }
        }
    }
    let (track_id, codec_params, mut decoder) =
        selected.ok_or_else(|| format!("No decodable audio track in {:?}", path))?;

    let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
    let channels = codec_params.channels.map(|c| c.count()).unwrap_or(2).max(1);

    let mut all_samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format_reader.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                let _ = symphonia::default::get_codecs()
                    .make(&codec_params, &DecoderOptions::default())
                    .map(|d| decoder = d);
                continue;
            }
            Err(e) => {
                warn!("Packet read error in {:?}: {e}", path);
                break;
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = decoded.spec();
                let cap = decoded.capacity() as u64;
                let mut buf = SampleBuffer::<f32>::new(cap, *spec);
                buf.copy_interleaved_ref(decoded);
                all_samples.extend_from_slice(buf.samples());
            }
            Err(SymphoniaError::DecodeError(msg)) => {
                warn!("Decode error in {:?}: {msg}", path);
            }
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        }
    }

    Ok(DecodedAudio {
        samples: all_samples,
        sample_rate,
        channels,
    })
}

// ─── Resample to 48 kHz (required for Opus) ──────────────────────────────────

fn resample_to_48k(audio: DecodedAudio) -> Result<DecodedAudio, String> {
    if audio.sample_rate == OPUS_SAMPLE_RATE {
        return Ok(audio);
    }

    let channels = audio.channels;
    let ratio = OPUS_SAMPLE_RATE as f64 / audio.sample_rate as f64;
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler =
        SincFixedIn::<f32>::new(ratio, 2.0, params, RESAMPLE_CHUNK_FRAMES, channels)
            .map_err(|e| format!("Failed to create resampler: {e}"))?;

    let deinterleaved = deinterleave(&audio.samples, channels);
    let total_frames = audio.samples.len() / channels;
    let mut out_channels: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut offset = 0usize;

    while offset < total_frames {
        let chunk_len = RESAMPLE_CHUNK_FRAMES.min(total_frames - offset);
        let input: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                let mut ch = deinterleaved[c][offset..offset + chunk_len].to_vec();
                if ch.len() < RESAMPLE_CHUNK_FRAMES {
                    ch.resize(RESAMPLE_CHUNK_FRAMES, 0.0);
                }
                ch
            })
            .collect();
        offset += chunk_len;

        let output = resampler
            .process(&input, None)
            .map_err(|e| format!("Resample error: {e}"))?;
        for (c, ch_out) in output.iter().enumerate() {
            out_channels[c].extend_from_slice(ch_out);
        }
    }

    // Flush remaining samples.
    let silence: Vec<Vec<f32>> = vec![vec![0.0f32; RESAMPLE_CHUNK_FRAMES]; channels];
    if let Ok(flushed) = resampler.process(&silence, None) {
        for (c, ch_out) in flushed.iter().enumerate() {
            out_channels[c].extend_from_slice(ch_out);
        }
    }

    let resampled_samples = interleave(&out_channels);
    Ok(DecodedAudio {
        samples: resampled_samples,
        sample_rate: OPUS_SAMPLE_RATE,
        channels,
    })
}

// ─── WAV encoding ────────────────────────────────────────────────────────────

fn encode_wav(
    audio: &DecodedAudio,
    settings: &crate::conversion_config::WavSettings,
    dest: &Path,
) -> Result<(), String> {
    let (sample_format, bits_per_sample) = match settings.bit_depth {
        WavBitDepth::Bits16 => (SampleFormat::Int, 16u16),
        WavBitDepth::Bits24 => (SampleFormat::Int, 24u16),
        WavBitDepth::Float32 => (SampleFormat::Float, 32u16),
    };
    let spec = WavSpec {
        channels: audio.channels as u16,
        sample_rate: audio.sample_rate,
        bits_per_sample,
        sample_format,
    };
    let mut writer =
        WavWriter::create(dest, spec).map_err(|e| format!("Failed to create WAV file: {e}"))?;

    match settings.bit_depth {
        WavBitDepth::Bits16 => {
            for &s in &audio.samples {
                let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
                writer
                    .write_sample(v)
                    .map_err(|e| format!("WAV write error: {e}"))?;
            }
        }
        WavBitDepth::Bits24 => {
            let scale = (1i32 << 23) as f32 - 1.0;
            for &s in &audio.samples {
                let v = (s.clamp(-1.0, 1.0) * scale).round() as i32;
                writer
                    .write_sample(v)
                    .map_err(|e| format!("WAV write error: {e}"))?;
            }
        }
        WavBitDepth::Float32 => {
            for &s in &audio.samples {
                writer
                    .write_sample(s)
                    .map_err(|e| format!("WAV write error: {e}"))?;
            }
        }
    }

    writer
        .finalize()
        .map_err(|e| format!("WAV finalize error: {e}"))
}

// ─── FLAC encoding ───────────────────────────────────────────────────────────

fn encode_flac(
    audio: &DecodedAudio,
    settings: &crate::conversion_config::FlacSettings,
    dest: &Path,
) -> Result<(), String> {
    use flac_encoder::{BpsLevel, FlacBuilder};

    let bps = match settings.bit_depth {
        FlacBitDepth::Bits16 => BpsLevel::Bps16,
        FlacBitDepth::Bits24 => BpsLevel::Bps24,
    };

    // flac_encoder accepts f32 via its IntoSample trait. Feed interleaved samples
    // converted to planar (one Vec<f32> per channel).
    let channels = audio.channels;
    let deinterleaved = deinterleave(&audio.samples, channels);

    let builder = FlacBuilder::from_planar(&deinterleaved, audio.sample_rate)
        .compression_level(settings.compression as u32)
        .bps(bps);

    builder
        .write_file(dest)
        .map_err(|e| format!("FLAC encode error: {e:?}"))
}

// ─── Opus + OGG encoding ─────────────────────────────────────────────────────

fn encode_opus(
    audio: &DecodedAudio,
    settings: &crate::conversion_config::OpusSettings,
    dest: &Path,
    source_path: &Path,
) -> Result<(), String> {
    debug_assert_eq!(
        audio.sample_rate, OPUS_SAMPLE_RATE,
        "Opus encoder requires 48 kHz input"
    );

    let channels = audio.channels.min(2); // libopus: 1 or 2
    let opus_channels = match channels {
        1 => opus::Channels::Mono,
        _ => opus::Channels::Stereo,
    };

    let mut encoder = opus::Encoder::new(OPUS_SAMPLE_RATE, opus_channels, opus::Application::Audio)
        .map_err(|e| format!("Failed to create Opus encoder: {e}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(settings.bitrate.bits_per_second()))
        .map_err(|e| format!("Failed to set Opus bitrate: {e}"))?;

    // Read source tags for embedding in the OpusTags OGG page.
    let source_meta = read_basic_tags(source_path);

    // Write OGG Opus file.
    let file = std::fs::File::create(dest)
        .map_err(|e| format!("Failed to create Opus output file: {e}"))?;
    let mut ogg_writer = ogg::PacketWriter::new(std::io::BufWriter::new(file));

    // Generate a random stream serial.
    let mut serial_bytes = [0u8; 4];
    let _ = getrandom::fill(&mut serial_bytes);
    let serial = u32::from_le_bytes(serial_bytes);

    // Pre-skip: number of samples to trim from the start (standard = 312).
    let pre_skip: u16 = 312;

    // ── OpusHead (identification header) ──────────────────────────────────────
    let mut opus_head = Vec::with_capacity(19);
    opus_head.extend_from_slice(b"OpusHead");
    opus_head.push(1); // version
    opus_head.push(channels as u8);
    opus_head.extend_from_slice(&pre_skip.to_le_bytes());
    // Original sample rate (informational).
    opus_head.extend_from_slice(&OPUS_SAMPLE_RATE.to_le_bytes());
    opus_head.extend_from_slice(&0u16.to_le_bytes()); // output gain = 0
    opus_head.push(0); // channel mapping family: RTP (mono/stereo)

    ogg_writer
        .write_packet::<Vec<u8>>(opus_head, serial, ogg::PacketWriteEndInfo::EndPage, 0)
        .map_err(|e| format!("Failed to write OpusHead: {e}"))?;

    // ── OpusTags (comment header) ─────────────────────────────────────────────
    let vendor = "roqtune";
    let comments = source_meta_to_vorbis_comments(&source_meta);
    let mut opus_tags = Vec::new();
    opus_tags.extend_from_slice(b"OpusTags");
    write_vorbis_string(&mut opus_tags, vendor);
    let comment_count = comments.len() as u32;
    opus_tags.extend_from_slice(&comment_count.to_le_bytes());
    for comment in &comments {
        write_vorbis_string(&mut opus_tags, comment);
    }

    ogg_writer
        .write_packet::<Vec<u8>>(opus_tags, serial, ogg::PacketWriteEndInfo::EndPage, 0)
        .map_err(|e| format!("Failed to write OpusTags: {e}"))?;

    // ── Audio packets ─────────────────────────────────────────────────────────
    // Feed the encoder in 960-sample-per-channel frames.
    let frame_samples = OPUS_FRAME_SAMPLES * channels; // interleaved
    let mut encode_buf = vec![0u8; 4096];
    let mut granule_pos: u64 = 0;

    let mut offset = 0usize; // offset in interleaved samples
    loop {
        let remaining = audio.samples.len().saturating_sub(offset);
        if remaining == 0 {
            break;
        }

        // Build a padded frame if we're at the tail.
        let mut frame = vec![0.0f32; frame_samples];
        let copy = remaining.min(frame_samples);
        frame[..copy].copy_from_slice(&audio.samples[offset..offset + copy]);
        let is_last = remaining <= frame_samples;

        let encoded_len = encoder
            .encode_float(&frame, &mut encode_buf)
            .map_err(|e| format!("Opus encode error: {e}"))?;

        offset += copy;
        granule_pos += OPUS_FRAME_SAMPLES as u64;

        let end_info = if is_last {
            ogg::PacketWriteEndInfo::EndStream
        } else {
            ogg::PacketWriteEndInfo::NormalPacket
        };

        ogg_writer
            .write_packet::<Vec<u8>>(
                encode_buf[..encoded_len].to_vec(),
                serial,
                end_info,
                granule_pos,
            )
            .map_err(|e| format!("Failed to write Opus audio packet: {e}"))?;

        if is_last {
            break;
        }
    }

    Ok(())
}

fn write_vorbis_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn source_meta_to_vorbis_comments(meta: &BasicTags) -> Vec<String> {
    let mut comments = Vec::new();
    if !meta.title.is_empty() {
        comments.push(format!("TITLE={}", meta.title));
    }
    if !meta.artist.is_empty() {
        comments.push(format!("ARTIST={}", meta.artist));
    }
    if !meta.album.is_empty() {
        comments.push(format!("ALBUM={}", meta.album));
    }
    if !meta.album_artist.is_empty() {
        comments.push(format!("ALBUMARTIST={}", meta.album_artist));
    }
    if !meta.date.is_empty() {
        comments.push(format!("DATE={}", meta.date));
    }
    if !meta.track_number.is_empty() {
        comments.push(format!("TRACKNUMBER={}", meta.track_number));
    }
    if !meta.genre.is_empty() {
        comments.push(format!("GENRE={}", meta.genre));
    }
    comments
}

// ─── MP3 encoding ────────────────────────────────────────────────────────────

fn encode_mp3(
    audio: &DecodedAudio,
    settings: &crate::conversion_config::Mp3Settings,
    dest: &Path,
) -> Result<(), String> {
    use mp3lame_encoder::{Builder, DualPcm, FlushNoGap, MonoPcm};

    let mut builder = Builder::new().ok_or("Failed to create LAME MP3 encoder")?;

    builder
        .set_num_channels(audio.channels.min(2) as u8)
        .map_err(|e| format!("MP3 set channels error: {e:?}"))?;
    builder
        .set_sample_rate(audio.sample_rate)
        .map_err(|e| format!("MP3 set sample rate error: {e:?}"))?;

    match settings.mode {
        Mp3Mode::Cbr => {
            let kbps = match settings.cbr_bitrate {
                crate::conversion_config::Mp3CbrBitrate::Kbps128 => {
                    mp3lame_encoder::Bitrate::Kbps128
                }
                crate::conversion_config::Mp3CbrBitrate::Kbps192 => {
                    mp3lame_encoder::Bitrate::Kbps192
                }
                crate::conversion_config::Mp3CbrBitrate::Kbps256 => {
                    mp3lame_encoder::Bitrate::Kbps256
                }
                crate::conversion_config::Mp3CbrBitrate::Kbps320 => {
                    mp3lame_encoder::Bitrate::Kbps320
                }
            };
            builder
                .set_brate(kbps)
                .map_err(|e| format!("MP3 set bitrate error: {e:?}"))?;
        }
        Mp3Mode::Vbr => {
            builder
                .set_vbr_mode(mp3lame_encoder::VbrMode::Mtrh)
                .map_err(|e| format!("MP3 set VBR mode error: {e:?}"))?;
            let quality = match settings.vbr_quality {
                crate::conversion_config::Mp3VbrQuality::V0 => mp3lame_encoder::Quality::Best,
                crate::conversion_config::Mp3VbrQuality::V2 => mp3lame_encoder::Quality::NearBest,
                crate::conversion_config::Mp3VbrQuality::V4 => mp3lame_encoder::Quality::Nice,
            };
            builder
                .set_vbr_quality(quality)
                .map_err(|e| format!("MP3 set VBR quality error: {e:?}"))?;
        }
    }

    let mut encoder = builder
        .build()
        .map_err(|e| format!("MP3 build error: {e:?}"))?;

    // Encode all samples — LAME needs a pre-allocated output buffer.
    // max_required_buffer_size gives a safe upper bound.
    use std::mem::MaybeUninit;
    let num_samples = audio.samples.len() / audio.channels.clamp(1, 2);
    let buf_size = mp3lame_encoder::max_required_buffer_size(num_samples);
    let mut out_buf = vec![MaybeUninit::<u8>::uninit(); buf_size];

    let mut encoded: Vec<u8> = Vec::with_capacity(buf_size);
    let channels = audio.channels.min(2);

    let n = if channels == 1 {
        let mono = MonoPcm(audio.samples.as_slice());
        encoder
            .encode(mono, &mut out_buf)
            .map_err(|e| format!("MP3 encode error: {e:?}"))?
    } else {
        let (left, right) = deinterleave_stereo(&audio.samples, channels);
        let dual = DualPcm {
            left: left.as_slice(),
            right: right.as_slice(),
        };
        encoder
            .encode(dual, &mut out_buf)
            .map_err(|e| format!("MP3 encode error: {e:?}"))?
    };
    // SAFETY: `encode` guarantees the first `n` bytes are initialized.
    encoded.extend(out_buf[..n].iter().map(|b| unsafe { b.assume_init() }));

    let mut flush_buf = vec![MaybeUninit::<u8>::uninit(); 7200];
    let nf = encoder
        .flush::<FlushNoGap>(&mut flush_buf)
        .map_err(|e| format!("MP3 flush error: {e:?}"))?;
    encoded.extend(flush_buf[..nf].iter().map(|b| unsafe { b.assume_init() }));

    std::fs::write(dest, &encoded).map_err(|e| format!("Failed to write MP3 file: {e}"))
}

// ─── Tag transfer via lofty ───────────────────────────────────────────────────

#[derive(Default)]
struct BasicTags {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    date: String,
    track_number: String,
    genre: String,
}

fn read_basic_tags(path: &Path) -> BasicTags {
    use lofty::config::ParseOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::ItemKey;

    let mut meta = BasicTags::default();

    let Ok(tagged) = Probe::open(path).and_then(|p| p.options(ParseOptions::new()).read()) else {
        return meta;
    };

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let Some(tag) = tag else {
        return meta;
    };

    macro_rules! get {
        ($field:ident, $key:ident) => {
            if let Some(v) = tag.get_string(ItemKey::$key) {
                meta.$field = v.to_string();
            }
        };
    }
    get!(title, TrackTitle);
    get!(artist, TrackArtist);
    get!(album, AlbumTitle);
    get!(track_number, TrackNumber);
    get!(genre, Genre);

    if let Some(v) = tag
        .get_string(ItemKey::AlbumArtist)
        .or_else(|| tag.get_string(ItemKey::TrackArtist))
    {
        meta.album_artist = v.to_string();
    }
    if let Some(v) = tag
        .get_string(ItemKey::RecordingDate)
        .or_else(|| tag.get_string(ItemKey::OriginalReleaseDate))
        .or_else(|| tag.get_string(ItemKey::Year))
    {
        meta.date = v.to_string();
    }

    meta
}

fn transfer_tags(source: &Path, dest: &Path) -> Result<(), String> {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::picture::PictureType;
    use lofty::probe::Probe;
    use lofty::tag::{ItemKey, Tag};

    // Collect source tags into owned data to avoid borrow conflicts.
    let (text_tags, cover_pic) = {
        let Ok(src_tagged) =
            Probe::open(source).and_then(|p| p.options(ParseOptions::new()).read())
        else {
            return Ok(());
        };
        let src_tag = match src_tagged.primary_tag().or_else(|| src_tagged.first_tag()) {
            Some(t) => t,
            None => return Ok(()),
        };

        let keys = [
            ItemKey::TrackTitle,
            ItemKey::TrackArtist,
            ItemKey::AlbumTitle,
            ItemKey::AlbumArtist,
            ItemKey::Year,
            ItemKey::RecordingDate,
            ItemKey::TrackNumber,
            ItemKey::Genre,
            ItemKey::Comment,
        ];
        let text: Vec<(ItemKey, String)> = keys
            .iter()
            .filter_map(|k| src_tag.get_string(*k).map(|v| (*k, v.to_string())))
            .collect();

        let pic = src_tag
            .pictures()
            .iter()
            .find(|p| p.pic_type() == PictureType::CoverFront)
            .cloned();

        (text, pic)
    };

    if text_tags.is_empty() && cover_pic.is_none() {
        return Ok(());
    }

    let Ok(mut dst_tagged) = Probe::open(dest).and_then(|p| p.options(ParseOptions::new()).read())
    else {
        return Ok(());
    };

    let dst_tag_type = dst_tagged.primary_tag_type();
    if dst_tagged.tag(dst_tag_type).is_none() {
        dst_tagged.insert_tag(Tag::new(dst_tag_type));
    }

    {
        let dst_tag = dst_tagged
            .tag_mut(dst_tag_type)
            .ok_or("Failed to get destination tag for writing")?;
        for (key, value) in text_tags {
            dst_tag.insert_text(key, value);
        }
        if let Some(pic) = cover_pic {
            dst_tag.push_picture(pic);
        }
    }

    dst_tagged
        .save_to_path(dest, WriteOptions::default())
        .map_err(|e| format!("Failed to write tags to {:?}: {e}", dest))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); channels];
    for (i, &s) in samples.iter().enumerate() {
        out[i % channels].push(s);
    }
    out
}

/// Returns (left, right) — for stereo. For mono source, duplicates the channel.
fn deinterleave_stereo(samples: &[f32], channels: usize) -> (Vec<f32>, Vec<f32>) {
    if channels == 1 {
        return (samples.to_vec(), samples.to_vec());
    }
    let n = samples.len() / 2;
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for chunk in samples.chunks_exact(2) {
        left.push(chunk[0]);
        right.push(chunk[1]);
    }
    (left, right)
}

fn interleave(channels: &[Vec<f32>]) -> Vec<f32> {
    if channels.is_empty() {
        return Vec::new();
    }
    let len = channels[0].len();
    let mut out = Vec::with_capacity(len * channels.len());
    for i in 0..len {
        for ch in channels {
            if i < ch.len() {
                out.push(ch[i]);
            }
        }
    }
    out
}
