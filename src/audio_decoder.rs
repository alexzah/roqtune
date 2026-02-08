use crate::protocol::{
    self, AudioMessage, AudioPacket, Config, ConfigMessage, Message, TrackIdentifier,
};
use log::{debug, error, warn};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::cmp::min;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::thread;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender};

#[derive(Debug, Clone)]
enum DecodeWorkItem {
    DecodeTracks(Vec<TrackIdentifier>),
    RequestDecodeChunk { requested_samples: usize },
    Stop,
    ConfigChanged(Config),
}

struct ActiveDecodeTrack {
    track_identifier: TrackIdentifier,
    source_track_id: u32,
    codec_params: CodecParameters,
    format_reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    source_sample_rate: u32,
    input_exhausted: bool,
    consecutive_decode_errors: u32,
}

// Worker to decode in a separate thread
struct DecodeWorker {
    bus_sender: Sender<Message>,
    work_receiver: MpscReceiver<DecodeWorkItem>,
    work_queue: VecDeque<DecodeWorkItem>,
    pending_tracks: VecDeque<TrackIdentifier>,
    active_track: Option<ActiveDecodeTrack>,
    resampler: Option<SincFixedIn<f32>>,
    resample_buffer: VecDeque<f32>,
    target_sample_rate: u32,
    target_channels: u16,
    target_bits_per_sample: u16,
    decoder_request_chunk_ms: u32,
}

impl DecodeWorker {
    pub fn new(bus_sender: Sender<Message>, work_receiver: MpscReceiver<DecodeWorkItem>) -> Self {
        Self {
            bus_sender,
            work_receiver,
            work_queue: VecDeque::new(),
            pending_tracks: VecDeque::new(),
            active_track: None,
            resampler: None,
            resample_buffer: VecDeque::new(),
            target_sample_rate: 44100,
            target_channels: 2,
            target_bits_per_sample: 16,
            decoder_request_chunk_ms: protocol::BufferingConfig::default().decoder_request_chunk_ms,
        }
    }

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
            DecodeWorkItem::Stop => {
                debug!("DecodeWorker: Received stop signal");
                self.clear_state();
            }
            DecodeWorkItem::DecodeTracks(tracks) => {
                for track in tracks {
                    self.pending_tracks.push_back(track);
                }
            }
            DecodeWorkItem::RequestDecodeChunk { requested_samples } => {
                self.decode_requested_samples(requested_samples);
            }
            DecodeWorkItem::ConfigChanged(config) => {
                self.target_sample_rate = config.output.sample_rate_khz;
                self.target_channels = config.output.channel_count;
                self.target_bits_per_sample = config.output.bits_per_sample;
                self.decoder_request_chunk_ms = config.buffering.decoder_request_chunk_ms;
                self.resampler = None;
                self.resample_buffer.clear();
            }
        }
    }

    fn clear_state(&mut self) {
        self.pending_tracks.clear();
        self.active_track = None;
        self.resample_buffer.clear();
        self.resampler = None;
    }

    fn create_resampler(&mut self, source_sample_rate: u32, chunk_size: usize) -> SincFixedIn<f32> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        SincFixedIn::<f32>::new(
            self.target_sample_rate as f64 / source_sample_rate as f64,
            2.0,
            params,
            chunk_size,
            self.target_channels as usize,
        )
        .unwrap()
    }

    fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
        let mut deinterleaved = vec![vec![]; channels];
        for (i, sample) in samples.iter().enumerate() {
            deinterleaved[i % channels].push(*sample);
        }
        deinterleaved
    }

    fn interleave(samples: &[Vec<f32>]) -> Vec<f32> {
        let mut interleaved = Vec::new();
        for i in 0..samples[0].len() {
            for sample in samples {
                interleaved.push(sample[i]);
            }
        }
        interleaved
    }

    fn resampler_input_chunk_size(&mut self, source_sample_rate: u32) -> usize {
        if self.resampler.is_none() {
            self.resampler = Some(self.create_resampler(source_sample_rate, 2048));
        }

        let channels = self.target_channels.max(1) as usize;
        self.resampler
            .as_ref()
            .expect("resampler initialized")
            .input_frames_next()
            * channels
    }

    fn resample_next_frame(&mut self, sample_rate: u32) -> Vec<f32> {
        let mut samples = Vec::new();
        if self.resampler.is_none() {
            self.resampler = Some(self.create_resampler(sample_rate, 2048));
        }

        let channels: usize = self.target_channels.max(1) as usize;
        let mut result: Vec<f32> = Vec::new();
        if let Some(resampler) = &mut self.resampler {
            for _ in 0..min(
                resampler.input_frames_next() * channels,
                self.resample_buffer.len(),
            ) {
                if let Some(sample) = self.resample_buffer.pop_front() {
                    samples.push(sample);
                }
            }

            // No need to resample
            if sample_rate == self.target_sample_rate {
                return samples;
            }

            let deinterleaved = Self::deinterleave(&samples, channels);
            let mut waves_out = vec![vec![]; channels];
            if deinterleaved[0].len() == resampler.input_frames_next() {
                waves_out = resampler.process(&deinterleaved, None).unwrap();
            } else {
                waves_out = resampler
                    .process_partial(Some(&deinterleaved), None)
                    .unwrap();
                if let Ok(flush_result) = resampler.process_partial::<&[f32]>(None, None) {
                    for i in 0..channels {
                        waves_out[i].extend(flush_result[i].iter());
                    }
                }
            }
            result = Self::interleave(&waves_out);
        }

        result
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

            if self.resample_buffer.is_empty() {
                if self.finish_active_track_if_complete() {
                    continue;
                }
                break;
            }

            let resampled_samples = self.resample_next_frame(source_sample_rate);
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

    fn decode_one_packet_into_buffer(&mut self) -> bool {
        let mut decoded_samples: Option<Vec<f32>> = None;
        let mut exhausted_input = false;

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
                            decoded_samples = Some(sample_buffer.samples().to_vec());
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

        if let Some(samples) = decoded_samples {
            self.resample_buffer.extend(samples);
            true
        } else {
            !exhausted_input
        }
    }

    fn finish_active_track_if_complete(&mut self) -> bool {
        let should_finish = self
            .active_track
            .as_ref()
            .map(|active| active.input_exhausted && self.resample_buffer.is_empty())
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
        true
    }

    fn start_next_track(&mut self) -> bool {
        while let Some(next_track) = self.pending_tracks.pop_front() {
            match self.open_track(next_track) {
                Some(active_track) => {
                    self.active_track = Some(active_track);
                    self.resampler = None;
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
        let file = match std::fs::File::open(input_track.path.clone()) {
            Ok(file) => file,
            Err(e) => {
                error!("Failed to open file: {}", e);
                return None;
            }
        };

        let media_source = MediaSourceStream::new(Box::new(file), Default::default());
        let hint = Hint::new();
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
        let source_channels = codec_params.channels.map(|c| c.count()).unwrap_or(2);
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
            duration_ms: duration_ms as u64,
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

pub struct AudioDecoder {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    worker_sender: MpscSender<DecodeWorkItem>,
}

impl AudioDecoder {
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        let (worker_sender, worker_receiver) = mpsc::channel(128);
        let mut audio_decoder = Self {
            bus_receiver,
            bus_sender,
            worker_sender,
        };
        audio_decoder.spawn_decode_worker(worker_receiver);
        audio_decoder
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    Message::Audio(AudioMessage::DecodeTracks(paths)) => {
                        debug!("AudioDecoder: Queueing tracks for decode {:?}", paths);
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::DecodeTracks(paths))
                            .unwrap();
                    }
                    Message::Audio(AudioMessage::RequestDecodeChunk { requested_samples }) => {
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::RequestDecodeChunk { requested_samples })
                            .unwrap();
                    }
                    Message::Audio(AudioMessage::StopDecoding) => {
                        debug!("AudioDecoder: Clearing decode state");
                        let _ = self.worker_sender.blocking_send(DecodeWorkItem::Stop);
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
                    Message::Config(ConfigMessage::AudioDeviceOpened {
                        sample_rate,
                        channels,
                    }) => {
                        debug!(
                            "AudioDecoder: Syncing with actual device config: sr={}, channels={}",
                            sample_rate, channels
                        );
                        let dummy_config = protocol::Config {
                            output: protocol::OutputConfig {
                                channel_count: channels,
                                sample_rate_khz: sample_rate,
                                bits_per_sample: 32,
                            },
                            ui: protocol::UiConfig::default(),
                            buffering: protocol::BufferingConfig::default(),
                        };
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::ConfigChanged(dummy_config))
                            .unwrap();
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!("AudioDecoder: bus closed");
                    break;
                }
            }
        }
    }

    fn spawn_decode_worker(&mut self, worker_receiver: MpscReceiver<DecodeWorkItem>) {
        let bus_sender = self.bus_sender.clone();
        thread::spawn(move || {
            let mut worker = DecodeWorker::new(bus_sender.clone(), worker_receiver);
            worker.run();
        });
    }
}
