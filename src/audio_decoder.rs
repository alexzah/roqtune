//! Audio decode and resample pipeline.
//!
//! The `AudioDecoder` listens to bus commands and forwards work to a dedicated
//! decode worker thread that performs file decode, optional seek, resampling,
//! and packet emission.

use crate::config::{BufferingConfig, Config, ResamplerQuality};
use crate::integration_keyring::get_opensubsonic_password;
use crate::integration_uri::{parse_opensubsonic_track_uri, OpenSubsonicTrackLocator};
use crate::protocol::{self, AudioMessage, AudioPacket, ConfigMessage, Message, TrackIdentifier};
use audio_mixer::{Channel as MixChannel, Mixer};
use log::{debug, error, warn};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::cmp::min;
use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender};

const OPENSUBSONIC_API_VERSION: &str = "1.16.1";
const OPENSUBSONIC_CLIENT_ID: &str = "roqtune";

/// Work items consumed by the decode worker thread.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
enum DecodeWorkItem {
    DecodeTracks {
        tracks: Vec<TrackIdentifier>,
        generation: u64,
    },
    RequestDecodeChunk {
        requested_samples: usize,
        generation: u64,
    },
    Stop {
        generation: u64,
    },
    ConfigChanged(Config),
    AudioDeviceOpened {
        stream_info: protocol::OutputStreamInfo,
    },
}

/// Decoder state for the track currently being produced.
struct ActiveDecodeTrack {
    track_identifier: TrackIdentifier,
    source_track_id: u32,
    codec_params: CodecParameters,
    format_reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    source_sample_rate: u32,
    source_channels: u16,
    input_exhausted: bool,
    consecutive_decode_errors: u32,
}

/// Single-threaded decode worker that owns decoder/resampler mutable state.
struct DecodeWorker {
    bus_sender: Sender<Message>,
    work_receiver: MpscReceiver<DecodeWorkItem>,
    decode_request_inflight: Arc<AtomicBool>,
    work_queue: VecDeque<DecodeWorkItem>,
    pending_tracks: VecDeque<TrackIdentifier>,
    active_track: Option<ActiveDecodeTrack>,
    resampler: Option<SincFixedIn<f32>>,
    resampler_flushed: bool,
    resample_buffer: VecDeque<f32>,
    downmix_mixers: HashMap<(usize, usize), Mixer<f32>>,
    target_sample_rate: u32,
    target_channels: u16,
    target_bits_per_sample: u16,
    resampler_quality: ResamplerQuality,
    dither_on_bitdepth_reduce: bool,
    downmix_higher_channel_tracks: bool,
    decoder_request_chunk_ms: u32,
    decode_generation: u64,
}

impl DecodeWorker {
    /// Creates a decode worker instance backed by a work queue receiver.
    pub fn new(
        bus_sender: Sender<Message>,
        work_receiver: MpscReceiver<DecodeWorkItem>,
        decode_request_inflight: Arc<AtomicBool>,
    ) -> Self {
        Self {
            bus_sender,
            work_receiver,
            decode_request_inflight,
            work_queue: VecDeque::new(),
            pending_tracks: VecDeque::new(),
            active_track: None,
            resampler: None,
            resampler_flushed: false,
            resample_buffer: VecDeque::new(),
            downmix_mixers: HashMap::new(),
            target_sample_rate: 44100,
            target_channels: 2,
            target_bits_per_sample: 16,
            resampler_quality: ResamplerQuality::High,
            dither_on_bitdepth_reduce: true,
            downmix_higher_channel_tracks: true,
            decoder_request_chunk_ms: BufferingConfig::default().decoder_request_chunk_ms,
            decode_generation: 0,
        }
    }

    fn should_bootstrap_decode(tracks: &[TrackIdentifier]) -> bool {
        tracks.iter().any(|track| track.play_immediately)
    }

    fn make_opensubsonic_salt() -> String {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        bytes.iter().map(|value| format!("{value:02x}")).collect()
    }

    fn opensubsonic_stream_url(locator: &OpenSubsonicTrackLocator, password: &str) -> String {
        let salt = Self::make_opensubsonic_salt();
        let token = format!("{:x}", md5::compute(format!("{}{}", password, salt)));
        format!(
            "{}/rest/stream.view?u={}&t={}&s={}&v={}&c={}&id={}",
            locator.endpoint.trim().trim_end_matches('/'),
            urlencoding::encode(locator.username.trim()),
            token,
            salt,
            OPENSUBSONIC_API_VERSION,
            OPENSUBSONIC_CLIENT_ID,
            urlencoding::encode(locator.song_id.as_str()),
        )
    }

    fn extension_from_content_type(content_type: &str) -> Option<&'static str> {
        let mime = content_type
            .split(';')
            .next()
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase();
        match mime.as_str() {
            "audio/mpeg" | "audio/mp3" => Some("mp3"),
            "audio/flac" | "audio/x-flac" => Some("flac"),
            "audio/ogg" | "audio/vorbis" | "audio/opus" => Some("ogg"),
            "audio/aac" | "audio/x-aac" => Some("aac"),
            "audio/mp4" | "audio/x-m4a" => Some("m4a"),
            "audio/wav" | "audio/x-wav" => Some("wav"),
            _ => None,
        }
    }

    fn fetch_opensubsonic_stream_bytes_with_hint(
        locator: &OpenSubsonicTrackLocator,
    ) -> Result<(Vec<u8>, Option<String>), String> {
        let Some(password) =
            get_opensubsonic_password(locator.profile_id.as_str()).map_err(|error| {
                format!("Failed to read OpenSubsonic password from keyring: {error}")
            })?
        else {
            return Err(format!(
                "OpenSubsonic profile '{}' has no saved password in keyring",
                locator.profile_id
            ));
        };
        let url = Self::opensubsonic_stream_url(locator, password.as_str());
        let client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(45))
            .timeout_write(Duration::from_secs(45))
            .build();
        let response = client
            .get(url.as_str())
            .call()
            .map_err(|error| format!("OpenSubsonic stream request failed: {error}"))?;
        let hint_extension = response
            .header("Content-Type")
            .and_then(Self::extension_from_content_type)
            .map(ToOwned::to_owned);
        let mut body = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut body)
            .map_err(|error| format!("OpenSubsonic stream body read failed: {error}"))?;
        if body.is_empty() {
            return Err("OpenSubsonic stream response was empty".to_string());
        }
        let preview = String::from_utf8_lossy(&body[..body.len().min(512)]).to_ascii_lowercase();
        if preview.contains("subsonic-response")
            || preview.contains("<error")
            || preview.contains("\"error\"")
        {
            return Err("OpenSubsonic stream request returned an error payload".to_string());
        }
        Ok((body, hint_extension))
    }

    fn open_media_source_stream(
        &self,
        track: &TrackIdentifier,
        hint: &mut Hint,
    ) -> Result<MediaSourceStream, String> {
        if let Some(locator) = parse_opensubsonic_track_uri(track.path.as_path()) {
            let (body, hint_extension) = Self::fetch_opensubsonic_stream_bytes_with_hint(&locator)?;
            if let Some(extension) = hint_extension {
                hint.with_extension(extension.as_str());
            }
            return Ok(MediaSourceStream::new(
                Box::new(Cursor::new(body)),
                Default::default(),
            ));
        }

        if let Some(extension) = track.path.extension().and_then(|ext| ext.to_str()) {
            hint.with_extension(extension);
        }
        let file = std::fs::File::open(track.path.clone())
            .map_err(|error| format!("Failed to open file: {error}"))?;
        Ok(MediaSourceStream::new(Box::new(file), Default::default()))
    }

    /// Runs the decode worker loop, draining queued work items forever.
    pub fn run(&mut self) {
        loop {
            while let Some(item) = self.work_queue.pop_front() {
                self.handle_work_item(item);
            }

            if let Some(item) = self.work_receiver.blocking_recv() {
                self.work_queue.push_back(item);
            }
        }
    }

    fn handle_work_item(&mut self, item: DecodeWorkItem) {
        match item {
            DecodeWorkItem::Stop { generation } => {
                if generation < self.decode_generation {
                    debug!(
                        "DecodeWorker: Ignoring stale stop. stale_generation={} current_generation={}",
                        generation, self.decode_generation
                    );
                    return;
                }
                debug!(
                    "DecodeWorker: Received stop signal for generation={}",
                    generation
                );
                self.clear_state();
                self.decode_generation = generation;
            }
            DecodeWorkItem::DecodeTracks { tracks, generation } => {
                if generation < self.decode_generation {
                    debug!(
                        "DecodeWorker: Ignoring stale decode tracks. stale_generation={} current_generation={}",
                        generation, self.decode_generation
                    );
                    return;
                }
                if generation > self.decode_generation {
                    self.clear_state();
                    self.decode_generation = generation;
                }
                let should_bootstrap = Self::should_bootstrap_decode(&tracks);
                for track in tracks {
                    self.pending_tracks.push_back(track);
                }
                if should_bootstrap {
                    let bootstrap_samples =
                        self.ms_to_samples(self.decoder_request_chunk_ms.max(1));
                    self.decode_requested_samples(bootstrap_samples.max(1));
                }
            }
            DecodeWorkItem::RequestDecodeChunk {
                requested_samples,
                generation,
            } => {
                self.decode_request_inflight.store(false, Ordering::Relaxed);
                if generation != self.decode_generation {
                    debug!(
                        "DecodeWorker: Ignoring decode chunk request for non-active generation. requested_generation={} current_generation={}",
                        generation, self.decode_generation
                    );
                    return;
                }
                self.decode_requested_samples(requested_samples);
            }
            DecodeWorkItem::ConfigChanged(config) => {
                let next_target_sample_rate = config.output.sample_rate_khz;
                let next_target_channels = config.output.channel_count;
                let next_target_bits_per_sample = config.output.bits_per_sample;
                let next_resampler_quality = config.output.resampler_quality;
                let next_dither_on_bitdepth_reduce = config.output.dither_on_bitdepth_reduce;
                let next_downmix_higher_channel_tracks =
                    config.output.downmix_higher_channel_tracks;
                let next_decoder_request_chunk_ms = config.buffering.decoder_request_chunk_ms;

                let audio_processing_changed = self.target_sample_rate != next_target_sample_rate
                    || self.target_channels != next_target_channels
                    || self.target_bits_per_sample != next_target_bits_per_sample
                    || self.resampler_quality != next_resampler_quality
                    || self.dither_on_bitdepth_reduce != next_dither_on_bitdepth_reduce
                    || self.downmix_higher_channel_tracks != next_downmix_higher_channel_tracks;

                self.target_sample_rate = next_target_sample_rate;
                self.target_channels = next_target_channels;
                self.target_bits_per_sample = next_target_bits_per_sample;
                self.resampler_quality = next_resampler_quality;
                self.dither_on_bitdepth_reduce = next_dither_on_bitdepth_reduce;
                self.downmix_higher_channel_tracks = next_downmix_higher_channel_tracks;
                self.decoder_request_chunk_ms = next_decoder_request_chunk_ms;

                if audio_processing_changed {
                    self.resampler = None;
                    self.resampler_flushed = false;
                    self.resample_buffer.clear();
                }
            }
            DecodeWorkItem::AudioDeviceOpened { stream_info } => {
                self.target_sample_rate = stream_info.sample_rate_hz;
                self.target_channels = stream_info.channel_count;
                self.target_bits_per_sample = stream_info.bits_per_sample;
                self.resampler = None;
                self.resampler_flushed = false;
                self.resample_buffer.clear();
            }
        }
    }

    fn clear_state(&mut self) {
        self.pending_tracks.clear();
        self.active_track = None;
        self.resample_buffer.clear();
        self.resampler = None;
        self.resampler_flushed = false;
    }

    fn create_resampler(
        &self,
        source_sample_rate: u32,
        chunk_size: usize,
    ) -> Result<SincFixedIn<f32>, String> {
        let params = match self.resampler_quality {
            ResamplerQuality::High => SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            },
            ResamplerQuality::Highest => SincInterpolationParameters {
                sinc_len: 512,
                f_cutoff: 0.97,
                interpolation: SincInterpolationType::Cubic,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            },
        };
        SincFixedIn::<f32>::new(
            self.target_sample_rate as f64 / source_sample_rate as f64,
            2.0,
            params,
            chunk_size,
            self.target_channels.max(1) as usize,
        )
        .map_err(|err| format!("Failed to create resampler: {err}"))
    }

    fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
        let mut deinterleaved = vec![vec![]; channels];
        for (i, sample) in samples.iter().enumerate() {
            deinterleaved[i % channels].push(*sample);
        }
        deinterleaved
    }

    fn interleave(samples: &[Vec<f32>]) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        let mut interleaved = Vec::new();
        for i in 0..samples[0].len() {
            for sample in samples {
                interleaved.push(sample[i]);
            }
        }
        interleaved
    }

    fn resampler_input_chunk_size(&mut self, source_sample_rate: u32) -> usize {
        if source_sample_rate == self.target_sample_rate {
            return 2048 * self.target_channels.max(1) as usize;
        }
        if self.resampler.is_none() {
            match self.create_resampler(source_sample_rate, 2048) {
                Ok(resampler) => self.resampler = Some(resampler),
                Err(err) => {
                    error!("{err}");
                    return 2048 * self.target_channels.max(1) as usize;
                }
            }
            self.resampler_flushed = false;
        }

        let channels = self.target_channels.max(1) as usize;
        self.resampler
            .as_ref()
            .map(|resampler| resampler.input_frames_next() * channels)
            .unwrap_or(2048 * channels)
    }

    fn pop_passthrough_chunk(&mut self, max_samples: usize) -> Vec<f32> {
        let take = max_samples.min(self.resample_buffer.len());
        if take == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(sample) = self.resample_buffer.pop_front() {
                out.push(sample);
            }
        }
        out
    }

    fn resample_next_frame(&mut self, source_sample_rate: u32, input_exhausted: bool) -> Vec<f32> {
        if source_sample_rate == self.target_sample_rate {
            return self.pop_passthrough_chunk(2048 * self.target_channels.max(1) as usize);
        }

        let mut samples = Vec::new();
        if self.resampler.is_none() {
            match self.create_resampler(source_sample_rate, 2048) {
                Ok(resampler) => {
                    self.resampler = Some(resampler);
                    self.resampler_flushed = false;
                }
                Err(err) => {
                    error!("{err}");
                    return self.pop_passthrough_chunk(2048 * self.target_channels.max(1) as usize);
                }
            }
        }

        let channels: usize = self.target_channels.max(1) as usize;
        if let Some(resampler) = &mut self.resampler {
            let input_frames_next = resampler.input_frames_next();
            for _ in 0..min(input_frames_next * channels, self.resample_buffer.len()) {
                if let Some(sample) = self.resample_buffer.pop_front() {
                    samples.push(sample);
                }
            }

            if samples.is_empty() {
                if input_exhausted && !self.resampler_flushed {
                    match resampler.process_partial::<&[f32]>(None, None) {
                        Ok(flush_result) => {
                            self.resampler_flushed = true;
                            return Self::interleave(&flush_result);
                        }
                        Err(err) => {
                            warn!("DecodeWorker: resampler flush failed: {}", err);
                            self.resampler_flushed = true;
                        }
                    }
                }
                return Vec::new();
            }

            let deinterleaved = Self::deinterleave(&samples, channels);
            let mut waves_out = if deinterleaved[0].len() == input_frames_next {
                match resampler.process(&deinterleaved, None) {
                    Ok(waves_out) => waves_out,
                    Err(err) => {
                        warn!("DecodeWorker: resample failed: {}", err);
                        return samples;
                    }
                }
            } else {
                match resampler.process_partial(Some(&deinterleaved), None) {
                    Ok(waves_out) => waves_out,
                    Err(err) => {
                        warn!("DecodeWorker: partial resample failed: {}", err);
                        return samples;
                    }
                }
            };

            if input_exhausted && self.resample_buffer.is_empty() && !self.resampler_flushed {
                match resampler.process_partial::<&[f32]>(None, None) {
                    Ok(flush_result) => {
                        for i in 0..channels {
                            waves_out[i].extend(flush_result[i].iter().copied());
                        }
                        self.resampler_flushed = true;
                    }
                    Err(err) => {
                        warn!("DecodeWorker: trailing resampler flush failed: {}", err);
                        self.resampler_flushed = true;
                    }
                }
            }
            return Self::interleave(&waves_out);
        }

        Vec::new()
    }

    fn ms_to_samples(&self, milliseconds: u32) -> usize {
        let sample_rate = self.target_sample_rate.max(1) as u128;
        let channels = self.target_channels.max(1) as u128;
        let samples = milliseconds as u128 * sample_rate * channels / 1000;
        samples.min(usize::MAX as u128) as usize
    }

    fn decode_requested_samples(&mut self, requested_samples: usize) {
        if requested_samples == 0 {
            return;
        }

        let max_request_samples = self.ms_to_samples(self.decoder_request_chunk_ms.max(1));
        let target_samples = requested_samples.min(max_request_samples.max(1));
        let mut emitted_samples = 0usize;

        while emitted_samples < target_samples {
            if self.active_track.is_none() && !self.start_next_track() {
                break;
            }

            let source_sample_rate = self
                .active_track
                .as_ref()
                .map(|track| track.source_sample_rate)
                .unwrap_or(self.target_sample_rate);

            let chunk_size = self.resampler_input_chunk_size(source_sample_rate);
            self.fill_resample_buffer(chunk_size);

            let input_exhausted = self
                .active_track
                .as_ref()
                .map(|track| track.input_exhausted)
                .unwrap_or(false);

            if self.resample_buffer.is_empty() && !input_exhausted {
                if self.finish_active_track_if_complete() {
                    continue;
                }
                break;
            }

            let resampled_samples = self.resample_next_frame(source_sample_rate, input_exhausted);
            if resampled_samples.is_empty() {
                if self.finish_active_track_if_complete() {
                    continue;
                }
                break;
            }

            emitted_samples += resampled_samples.len();
            let _ = self
                .bus_sender
                .send(Message::Audio(AudioMessage::AudioPacket(
                    AudioPacket::Samples {
                        samples: resampled_samples,
                    },
                )));

            self.finish_active_track_if_complete();
        }
    }

    fn fill_resample_buffer(&mut self, desired_samples: usize) {
        while self.resample_buffer.len() < desired_samples {
            if let Some(active) = &self.active_track {
                if active.input_exhausted {
                    break;
                }
            } else {
                break;
            }

            if !self.decode_one_packet_into_buffer() {
                break;
            }
        }
    }

    fn channel_map_channels(
        samples: &[f32],
        source_channels: usize,
        target_channels: usize,
    ) -> Vec<f32> {
        if source_channels == 0 || target_channels == 0 {
            return Vec::new();
        }
        if source_channels == target_channels {
            return samples.to_vec();
        }

        let frame_count = samples.len() / source_channels;
        let mut remapped = Vec::with_capacity(frame_count * target_channels);

        for frame_index in 0..frame_count {
            let frame_start = frame_index * source_channels;
            let frame = &samples[frame_start..frame_start + source_channels];
            for out_channel in 0..target_channels {
                let sample = if target_channels == 1 {
                    let sum = frame.iter().copied().sum::<f32>();
                    sum / source_channels as f32
                } else if source_channels == 1 {
                    frame[0]
                } else if out_channel < source_channels {
                    frame[out_channel]
                } else {
                    let index = out_channel % source_channels;
                    frame[index]
                };
                remapped.push(sample);
            }
        }

        remapped
    }

    fn channel_layout_for_count(channel_count: usize) -> Vec<MixChannel> {
        match channel_count {
            0 => Vec::new(),
            1 => vec![MixChannel::FrontCenter],
            2 => vec![MixChannel::FrontLeft, MixChannel::FrontRight],
            3 => vec![
                MixChannel::FrontLeft,
                MixChannel::FrontRight,
                MixChannel::FrontCenter,
            ],
            4 => vec![
                MixChannel::FrontLeft,
                MixChannel::FrontRight,
                MixChannel::BackLeft,
                MixChannel::BackRight,
            ],
            5 => vec![
                MixChannel::FrontLeft,
                MixChannel::FrontRight,
                MixChannel::FrontCenter,
                MixChannel::BackLeft,
                MixChannel::BackRight,
            ],
            6 => vec![
                MixChannel::FrontLeft,
                MixChannel::FrontRight,
                MixChannel::FrontCenter,
                MixChannel::LowFrequency,
                MixChannel::BackLeft,
                MixChannel::BackRight,
            ],
            7 => vec![
                MixChannel::FrontLeft,
                MixChannel::FrontRight,
                MixChannel::FrontCenter,
                MixChannel::LowFrequency,
                MixChannel::BackLeft,
                MixChannel::BackRight,
                MixChannel::BackCenter,
            ],
            8 => vec![
                MixChannel::FrontLeft,
                MixChannel::FrontRight,
                MixChannel::FrontCenter,
                MixChannel::LowFrequency,
                MixChannel::BackLeft,
                MixChannel::BackRight,
                MixChannel::SideLeft,
                MixChannel::SideRight,
            ],
            _ => {
                let mut layout = Self::channel_layout_for_count(8);
                layout.resize(channel_count, MixChannel::Discrete);
                layout
            }
        }
    }

    fn downmix_mixer_for(&mut self, source_channels: usize, target_channels: usize) -> &Mixer<f32> {
        self.downmix_mixers
            .entry((source_channels, target_channels))
            .or_insert_with(|| {
                let input_layout = Self::channel_layout_for_count(source_channels);
                let output_layout = Self::channel_layout_for_count(target_channels);
                Mixer::<f32>::new(&input_layout, &output_layout)
            })
    }

    fn downmix_channels(
        &mut self,
        samples: &[f32],
        source_channels: usize,
        target_channels: usize,
    ) -> Vec<f32> {
        if source_channels == 0 || target_channels == 0 {
            return Vec::new();
        }
        if source_channels <= target_channels {
            return Self::channel_map_channels(samples, source_channels, target_channels);
        }

        let frame_count = samples.len() / source_channels;
        let mut downmixed = Vec::with_capacity(frame_count * target_channels);
        let mixer = self.downmix_mixer_for(source_channels, target_channels);
        let mut output_frame = vec![0.0f32; target_channels];

        for input_frame in samples.chunks_exact(source_channels) {
            mixer.mix(input_frame, &mut output_frame);
            downmixed.extend_from_slice(&output_frame);
        }

        downmixed
    }

    fn transform_channels(
        &mut self,
        samples: &[f32],
        source_channels: usize,
        target_channels: usize,
    ) -> Vec<f32> {
        if source_channels > target_channels && self.downmix_higher_channel_tracks {
            self.downmix_channels(samples, source_channels, target_channels)
        } else {
            Self::channel_map_channels(samples, source_channels, target_channels)
        }
    }

    fn decode_one_packet_into_buffer(&mut self) -> bool {
        let mut decoded_samples: Option<(Vec<f32>, usize)> = None;
        let mut exhausted_input = false;
        let target_channels = self.target_channels.max(1) as usize;

        {
            let active = match self.active_track.as_mut() {
                Some(track) => track,
                None => return false,
            };

            match active.format_reader.next_packet() {
                Ok(packet) => {
                    if packet.track_id() != active.source_track_id {
                        return true;
                    }

                    match active.decoder.decode(&packet) {
                        Ok(decoded) => {
                            active.consecutive_decode_errors = 0;
                            let spec = decoded.spec();
                            let duration = decoded.capacity() as u64;
                            let mut sample_buffer = SampleBuffer::<f32>::new(duration, *spec);
                            sample_buffer.copy_interleaved_ref(decoded);
                            decoded_samples = Some((
                                sample_buffer.samples().to_vec(),
                                active.source_channels.max(1) as usize,
                            ));
                        }
                        Err(Error::DecodeError(msg)) => {
                            warn!("Decode error (skipping frame): {}", msg);
                            active.consecutive_decode_errors += 1;
                            if active.consecutive_decode_errors > 50 {
                                error!("Too many consecutive decode errors. Giving up on track.");
                                exhausted_input = true;
                            }
                        }
                        Err(Error::ResetRequired) => {
                            debug!("DecodeWorker: Reset required. Re-creating decoder.");
                            match symphonia::default::get_codecs()
                                .make(&active.codec_params, &DecoderOptions::default())
                            {
                                Ok(decoder) => {
                                    active.decoder = decoder;
                                    active.consecutive_decode_errors = 0;
                                }
                                Err(e) => {
                                    error!("Failed to re-create decoder: {}", e);
                                    exhausted_input = true;
                                }
                            }
                        }
                        Err(e) => {
                            error!("Fatal decode error: {}", e);
                            exhausted_input = true;
                        }
                    }
                }
                Err(Error::IoError(_)) => {
                    exhausted_input = true;
                }
                Err(e) => {
                    debug!(
                        "DecodeWorker: Reached end of stream or failed to read packet: {}",
                        e
                    );
                    exhausted_input = true;
                }
            }

            if exhausted_input {
                active.input_exhausted = true;
            }
        }

        if let Some((samples, source_channels)) = decoded_samples {
            let transformed = self.transform_channels(&samples, source_channels, target_channels);
            self.resample_buffer.extend(transformed);
            true
        } else {
            !exhausted_input
        }
    }

    fn finish_active_track_if_complete(&mut self) -> bool {
        let requires_resampling = self
            .active_track
            .as_ref()
            .map(|active| active.source_sample_rate != self.target_sample_rate)
            .unwrap_or(false);
        let should_finish = self
            .active_track
            .as_ref()
            .map(|active| {
                active.input_exhausted
                    && self.resample_buffer.is_empty()
                    && (!requires_resampling || self.resampler_flushed)
            })
            .unwrap_or(false);

        if !should_finish {
            return false;
        }

        let track = self
            .active_track
            .take()
            .expect("track exists when finishing");
        let _ = self
            .bus_sender
            .send(Message::Audio(AudioMessage::AudioPacket(
                AudioPacket::TrackFooter {
                    id: track.track_identifier.id.clone(),
                },
            )));
        let _ = self
            .bus_sender
            .send(Message::Audio(AudioMessage::TrackCached(
                track.track_identifier.id.clone(),
                track.track_identifier.start_offset_ms,
            )));

        self.resampler = None;
        self.resampler_flushed = false;
        true
    }

    fn start_next_track(&mut self) -> bool {
        while let Some(next_track) = self.pending_tracks.pop_front() {
            match self.open_track(next_track) {
                Some(active_track) => {
                    self.active_track = Some(active_track);
                    self.resampler = None;
                    self.resampler_flushed = false;
                    self.resample_buffer.clear();
                    return true;
                }
                None => {
                    warn!("DecodeWorker: Failed to initialize track, skipping");
                }
            }
        }

        false
    }

    fn open_track(&mut self, input_track: TrackIdentifier) -> Option<ActiveDecodeTrack> {
        let mut hint = Hint::new();
        let media_source = match self.open_media_source_stream(&input_track, &mut hint) {
            Ok(source) => source,
            Err(error_text) => {
                error!("{error_text}");
                return None;
            }
        };
        let mut format_reader = match symphonia::default::get_probe().format(
            &hint,
            media_source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(probed) => probed.format,
            Err(e) => {
                error!("Failed to probe media source: {}", e);
                return None;
            }
        };

        let (source_track_id, codec_params) = {
            let track = match format_reader.default_track() {
                Some(track) => track,
                None => {
                    error!("No default track found");
                    return None;
                }
            };
            (track.id, track.codec_params.clone())
        };

        let source_sample_rate = codec_params.sample_rate.unwrap_or(44100);
        let source_channels = codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);
        if source_channels == 0 {
            error!("Unsupported channel count 0 in {:?}", input_track.path);
            return None;
        }

        if input_track.start_offset_ms > 0 {
            debug!("DecodeWorker: Seeking to {}ms", input_track.start_offset_ms);
            let seconds = input_track.start_offset_ms / 1000;
            let frac = (input_track.start_offset_ms % 1000) as f64 / 1000.0;
            if let Err(e) = format_reader.seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: symphonia::core::units::Time { seconds, frac },
                    track_id: Some(source_track_id),
                },
            ) {
                error!("Seek failed: {}", e);
            }
        }

        let decoder = match symphonia::default::get_codecs()
            .make(&codec_params, &DecoderOptions::default())
        {
            Ok(decoder) => decoder,
            Err(e) => {
                error!("Failed to create decoder: {}", e);
                return None;
            }
        };

        let technical_metadata = self.build_technical_metadata(&input_track.path, &codec_params);
        debug!(
            "DecodeWorker: Track ready id={} sr={} channels={} play_immediately={}",
            input_track.id, source_sample_rate, source_channels, input_track.play_immediately
        );

        let _ = self
            .bus_sender
            .send(Message::Audio(AudioMessage::AudioPacket(
                AudioPacket::TrackHeader {
                    id: input_track.id.clone(),
                    play_immediately: input_track.play_immediately,
                    technical_metadata: technical_metadata.clone(),
                    start_offset_ms: input_track.start_offset_ms,
                },
            )));

        Some(ActiveDecodeTrack {
            track_identifier: input_track,
            source_track_id,
            codec_params,
            format_reader,
            decoder,
            source_sample_rate,
            source_channels,
            input_exhausted: false,
            consecutive_decode_errors: 0,
        })
    }

    fn build_technical_metadata(
        &self,
        path: &PathBuf,
        codec_params: &CodecParameters,
    ) -> protocol::TechnicalMetadata {
        let sample_rate = codec_params.sample_rate.unwrap_or(44100);
        let format_name = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("AUDIO")
            .to_uppercase();

        let n_frames = codec_params.n_frames.unwrap_or(0);
        let duration_ms = if n_frames > 0 {
            (n_frames as f64 * 1000.0) / sample_rate as f64
        } else {
            0.0
        };

        let mut bitrate = 0;
        if let Ok(file_metadata) = std::fs::metadata(path) {
            let file_size = file_metadata.len();
            let metadata_size = get_metadata_size(path);
            let audio_data_size = if file_size > metadata_size {
                file_size - metadata_size
            } else {
                file_size
            };

            if duration_ms > 0.0 {
                bitrate = ((audio_data_size as f64 * 8.0) / (duration_ms / 1000.0)) as u32;
            }
        }

        protocol::TechnicalMetadata {
            format: format_name,
            bitrate_kbps: (bitrate as f32 / 1000.0).round() as u32,
            sample_rate_hz: sample_rate,
            channel_count: codec_params
                .channels
                .map(|channels| channels.count() as u16)
                .unwrap_or(2),
            duration_ms: duration_ms as u64,
            bits_per_sample: codec_params.bits_per_sample.unwrap_or(16) as u16,
        }
    }
}

fn get_metadata_size(path: &PathBuf) -> u64 {
    let mut total_size = 0;
    if let Ok(mut file) = std::fs::File::open(path) {
        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_size == 0 {
            return 0;
        }

        let mut header = [0u8; 10];
        use std::io::{Read, Seek, SeekFrom};
        if file.read_exact(&mut header).is_ok() && &header[0..3] == b"ID3" {
            let size = ((header[6] as u32 & 0x7F) << 21)
                | ((header[7] as u32 & 0x7F) << 14)
                | ((header[8] as u32 & 0x7F) << 7)
                | (header[9] as u32 & 0x7F);
            total_size += (size + 10) as u64;
        }

        if file_size > 128 {
            let _ = file.seek(SeekFrom::End(-128));
            let mut id3v1 = [0u8; 3];
            if file.read_exact(&mut id3v1).is_ok() && &id3v1 == b"TAG" {
                total_size += 128;
            }
        }
    }
    total_size
}

/// Bus-facing decoder orchestrator that manages decode generations.
pub struct AudioDecoder {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    worker_sender: MpscSender<DecodeWorkItem>,
    decode_request_inflight: Arc<AtomicBool>,
    decode_generation: u64,
}

impl AudioDecoder {
    /// Creates the decoder and spawns its dedicated decode worker thread.
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        let (worker_sender, worker_receiver) = mpsc::channel(128);
        let mut audio_decoder = Self {
            bus_receiver,
            bus_sender,
            worker_sender,
            decode_request_inflight: Arc::new(AtomicBool::new(false)),
            decode_generation: 0,
        };
        audio_decoder.spawn_decode_worker(worker_receiver);
        audio_decoder
    }

    /// Starts the blocking event loop that translates bus commands into decode work.
    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    Message::Audio(AudioMessage::DecodeTracks(paths)) => {
                        debug!("AudioDecoder: Queueing tracks for decode {:?}", paths);
                        if paths.iter().any(|track| track.play_immediately) {
                            self.decode_generation = self.decode_generation.saturating_add(1);
                        }
                        let generation = self.decode_generation;
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::DecodeTracks {
                                tracks: paths,
                                generation,
                            })
                            .unwrap();
                    }
                    Message::Audio(AudioMessage::RequestDecodeChunk { requested_samples }) => {
                        self.enqueue_decode_chunk_request(
                            self.decode_generation,
                            requested_samples,
                        );
                    }
                    Message::Audio(AudioMessage::StopDecoding) => {
                        debug!("AudioDecoder: Clearing decode state");
                        let generation = self.decode_generation;
                        let _ = self
                            .worker_sender
                            .blocking_send(DecodeWorkItem::Stop { generation });
                    }
                    Message::Config(ConfigMessage::ConfigChanged(config)) => {
                        debug!(
                            "AudioDecoder: Received config changed command: {:?}",
                            config
                        );
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::ConfigChanged(config))
                            .unwrap();
                    }
                    Message::Config(ConfigMessage::AudioDeviceOpened { stream_info }) => {
                        debug!(
                            "AudioDecoder: Syncing with actual device config: sr={}, channels={}, bits={}",
                            stream_info.sample_rate_hz,
                            stream_info.channel_count,
                            stream_info.bits_per_sample
                        );
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::AudioDeviceOpened { stream_info })
                            .unwrap();
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(
                        "AudioDecoder lagged on control bus, skipped {} message(s)",
                        skipped
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!("AudioDecoder: bus closed");
                    break;
                }
            }
        }
    }

    fn enqueue_decode_chunk_request(&self, generation: u64, requested_samples: usize) {
        if requested_samples == 0 {
            return;
        }
        if self
            .decode_request_inflight
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        match self
            .worker_sender
            .try_send(DecodeWorkItem::RequestDecodeChunk {
                requested_samples,
                generation,
            }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.decode_request_inflight.store(false, Ordering::Relaxed);
                // Decode chunk requests are advisory; if a request is already queued, let it drain.
                debug!("AudioDecoder: dropping decode request because worker queue is full");
            }
            Err(TrySendError::Closed(_)) => {
                self.decode_request_inflight.store(false, Ordering::Relaxed);
                error!("AudioDecoder: decode worker channel is closed");
            }
        }
    }

    fn spawn_decode_worker(&mut self, worker_receiver: MpscReceiver<DecodeWorkItem>) {
        let bus_sender = self.bus_sender.clone();
        let decode_request_inflight = self.decode_request_inflight.clone();
        thread::spawn(move || {
            let mut worker =
                DecodeWorker::new(bus_sender.clone(), worker_receiver, decode_request_inflight);
            worker.run();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioDecoder, DecodeWorkItem, DecodeWorker};
    use crate::integration_uri::OpenSubsonicTrackLocator;
    use crate::protocol::TrackIdentifier;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tokio::sync::mpsc;

    #[test]
    fn test_enqueue_decode_chunk_request_drops_when_worker_queue_full() {
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, mut worker_rx) = mpsc::channel(1);
        let decoder = AudioDecoder {
            bus_receiver: bus_sender.subscribe(),
            bus_sender: bus_sender.clone(),
            worker_sender: _worker_tx,
            decode_request_inflight: Arc::new(AtomicBool::new(false)),
            decode_generation: 0,
        };

        decoder
            .worker_sender
            .try_send(DecodeWorkItem::Stop { generation: 0 })
            .expect("seed worker queue");
        decoder.enqueue_decode_chunk_request(0, 10_000);

        let queued = worker_rx.try_recv().expect("expected first queued item");
        assert!(matches!(queued, DecodeWorkItem::Stop { generation: 0 }));
        assert!(
            worker_rx.try_recv().is_err(),
            "decode request should have been dropped while queue was full"
        );
    }

    fn make_track(id: &str, play_immediately: bool) -> TrackIdentifier {
        TrackIdentifier {
            id: id.to_string(),
            path: PathBuf::from(format!("/tmp/{}.flac", id)),
            play_immediately,
            start_offset_ms: 0,
        }
    }

    #[test]
    fn test_should_bootstrap_decode_detects_immediate_track() {
        let tracks = vec![
            TrackIdentifier {
                id: "a".to_string(),
                path: PathBuf::from("/tmp/a.flac"),
                play_immediately: false,
                start_offset_ms: 0,
            },
            TrackIdentifier {
                id: "b".to_string(),
                path: PathBuf::from("/tmp/b.flac"),
                play_immediately: true,
                start_offset_ms: 0,
            },
        ];
        assert!(DecodeWorker::should_bootstrap_decode(&tracks));
    }

    #[test]
    fn test_should_bootstrap_decode_false_when_no_immediate_track() {
        let tracks = vec![TrackIdentifier {
            id: "a".to_string(),
            path: PathBuf::from("/tmp/a.flac"),
            play_immediately: false,
            start_offset_ms: 0,
        }];
        assert!(!DecodeWorker::should_bootstrap_decode(&tracks));
    }

    #[test]
    fn test_stale_stop_does_not_clear_active_generation_queue() {
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, Arc::new(AtomicBool::new(false)));

        worker.handle_work_item(DecodeWorkItem::DecodeTracks {
            tracks: vec![make_track("g2", false)],
            generation: 2,
        });
        assert_eq!(worker.decode_generation, 2);
        assert_eq!(worker.pending_tracks.len(), 1);

        worker.handle_work_item(DecodeWorkItem::Stop { generation: 1 });
        assert_eq!(worker.decode_generation, 2);
        assert_eq!(worker.pending_tracks.len(), 1);
    }

    #[test]
    fn test_stale_decode_request_is_ignored() {
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, Arc::new(AtomicBool::new(false)));

        worker.handle_work_item(DecodeWorkItem::DecodeTracks {
            tracks: vec![make_track("g3", false)],
            generation: 3,
        });
        assert_eq!(worker.decode_generation, 3);
        assert_eq!(worker.pending_tracks.len(), 1);

        worker.handle_work_item(DecodeWorkItem::RequestDecodeChunk {
            requested_samples: 10_000,
            generation: 2,
        });
        assert_eq!(worker.pending_tracks.len(), 1);
    }

    #[test]
    fn test_new_generation_replaces_previous_pending_tracks() {
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, Arc::new(AtomicBool::new(false)));

        worker.handle_work_item(DecodeWorkItem::DecodeTracks {
            tracks: vec![make_track("old", false)],
            generation: 1,
        });
        assert_eq!(worker.pending_tracks.len(), 1);

        worker.handle_work_item(DecodeWorkItem::DecodeTracks {
            tracks: vec![make_track("new", false)],
            generation: 2,
        });
        assert_eq!(worker.decode_generation, 2);
        assert_eq!(worker.pending_tracks.len(), 1);
        assert_eq!(
            worker.pending_tracks.front().map(|track| track.id.as_str()),
            Some("new")
        );
    }

    #[test]
    fn test_stop_clears_current_generation_state() {
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, Arc::new(AtomicBool::new(false)));

        worker.handle_work_item(DecodeWorkItem::DecodeTracks {
            tracks: vec![make_track("active", false)],
            generation: 4,
        });
        assert_eq!(worker.pending_tracks.len(), 1);

        worker.handle_work_item(DecodeWorkItem::Stop { generation: 4 });
        assert_eq!(worker.decode_generation, 4);
        assert!(worker.pending_tracks.is_empty());
        assert!(worker.active_track.is_none());
    }

    #[test]
    fn test_enqueue_decode_chunk_request_coalesces_while_inflight() {
        let (bus_sender, _) = broadcast::channel(8);
        let (worker_tx, mut worker_rx) = mpsc::channel(8);
        let inflight = Arc::new(AtomicBool::new(false));
        let decoder = AudioDecoder {
            bus_receiver: bus_sender.subscribe(),
            bus_sender: bus_sender.clone(),
            worker_sender: worker_tx,
            decode_request_inflight: inflight.clone(),
            decode_generation: 0,
        };

        decoder.enqueue_decode_chunk_request(0, 10_000);
        decoder.enqueue_decode_chunk_request(0, 20_000);

        let queued = worker_rx
            .try_recv()
            .expect("expected queued decode request");
        assert!(matches!(
            queued,
            DecodeWorkItem::RequestDecodeChunk {
                requested_samples: 10_000,
                generation: 0
            }
        ));
        assert!(
            worker_rx.try_recv().is_err(),
            "second request should be coalesced while inflight"
        );
        assert!(inflight.load(Ordering::Relaxed));
    }

    #[test]
    fn test_worker_clears_inflight_flag_when_handling_decode_request() {
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let inflight = Arc::new(AtomicBool::new(true));
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, inflight.clone());
        worker.decode_generation = 2;

        worker.handle_work_item(DecodeWorkItem::RequestDecodeChunk {
            requested_samples: 5_000,
            generation: 1,
        });

        assert!(
            !inflight.load(Ordering::Relaxed),
            "worker should clear inflight even for stale requests"
        );
    }

    #[test]
    fn test_transform_channels_downmixes_when_enabled_for_higher_channel_source() {
        let samples = vec![0.3, 0.2, 0.1, 0.0, 0.2, 0.1];
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, Arc::new(AtomicBool::new(false)));
        worker.downmix_higher_channel_tracks = true;
        let transformed = worker.transform_channels(&samples, 6, 2);

        assert_eq!(transformed.len(), 2);
        assert_ne!(transformed, vec![0.3, 0.2]);
        assert!(transformed.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn test_transform_channels_uses_channel_map_when_downmix_disabled() {
        let samples = vec![0.3, 0.2, 0.1, 0.0, 0.2, 0.1];
        let (bus_sender, _) = broadcast::channel(8);
        let (_worker_tx, worker_rx) = mpsc::channel(8);
        let mut worker = DecodeWorker::new(bus_sender, worker_rx, Arc::new(AtomicBool::new(false)));
        worker.downmix_higher_channel_tracks = false;
        let transformed = worker.transform_channels(&samples, 6, 2);

        assert_eq!(transformed, vec![0.3, 0.2]);
    }

    #[test]
    fn test_opensubsonic_stream_url_contains_required_query_parts() {
        let locator = OpenSubsonicTrackLocator {
            profile_id: "home".to_string(),
            song_id: "song-42".to_string(),
            endpoint: "https://music.example.com/".to_string(),
            username: "alice@example.com".to_string(),
        };
        let url = DecodeWorker::opensubsonic_stream_url(&locator, "secret");
        assert!(url.starts_with("https://music.example.com/rest/stream.view?"));
        assert!(url.contains("u=alice%40example.com"));
        assert!(url.contains("id=song-42"));
        assert!(url.contains("t="));
        assert!(url.contains("s="));
        assert!(url.contains("v=1.16.1"));
        assert!(url.contains("c=roqtune"));
    }
}
