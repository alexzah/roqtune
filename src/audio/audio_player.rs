//! Audio output engine.
//!
//! Consumes decoded packets, manages queue/cursor state, drives the CPAL output
//! stream, and emits playback progress/track lifecycle notifications.

use crate::protocol::{
    AudioMessage, AudioPacket, ChannelTransformKind, ConfigMessage, Message, OutputPathInfo,
    OutputSampleFormat, OutputStreamInfo, PlaybackMessage, TrackStarted,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, error, warn};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tokio::sync::broadcast::{Receiver, Sender};

/// Queue marker used to announce track start inside the audio stream.
#[derive(Debug, Clone)]
pub struct TrackHeader {
    /// Stable track id.
    pub id: String,
    /// Start offset applied when this track entered playback.
    pub start_offset_ms: u64,
}

/// Queue item variants consumed by the audio callback.
#[derive(Debug, Clone)]
enum AudioQueueEntry {
    Samples(Vec<f32>),
    TrackHeader(TrackHeader),
    TrackFooter(String),
}

/// Cached queue index span for one decoded track.
#[derive(Debug, Clone)]
struct TrackIndex {
    start: usize,
    end: Option<usize>,
    start_offset_ms: u64,
    technical_metadata: crate::protocol::TechnicalMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputConfigSignature {
    device_name: Option<String>,
    sample_rate_hz: u32,
    channel_count: u16,
    bits_per_sample: u16,
    dither_on_bitdepth_reduce: bool,
}

/// Runtime audio output controller and packet queue owner.
pub struct AudioPlayer {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    // Audio state
    target_sample_rate: Arc<AtomicUsize>,
    target_channels: Arc<AtomicUsize>,
    target_bits_per_sample: u16,
    dither_on_bitdepth_reduce: bool,
    downmix_higher_channel_tracks: bool,
    target_output_device_name: Arc<Mutex<Option<String>>>,
    output_stream_info: Arc<Mutex<Option<OutputStreamInfo>>>,
    sample_queue: Arc<Mutex<VecDeque<AudioQueueEntry>>>,
    queue_start_position: Arc<AtomicUsize>,
    queue_end_position: Arc<AtomicUsize>,
    is_playing: Arc<AtomicBool>,
    current_track_id: Arc<Mutex<String>>,
    current_track_position: Arc<AtomicUsize>,
    current_track_offset_ms: Arc<AtomicUsize>,
    current_metadata: Arc<Mutex<Option<crate::protocol::TechnicalMetadata>>>,
    decode_bootstrap_pending: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    buffer_low_watermark_ms: Arc<AtomicUsize>,
    buffer_target_ms: Arc<AtomicUsize>,
    buffer_request_interval_ms: Arc<AtomicUsize>,

    // Setup cache
    cached_track_indices: Arc<Mutex<HashMap<String, TrackIndex>>>,
    pending_immediate_start_track_id: Option<String>,

    // Audio stream
    config: Option<cpal::StreamConfig>,
    sample_format: Option<cpal::SampleFormat>,
    device: Option<cpal::Device>,
    cached_requested_device_name: Option<String>,
    cached_supported_output_configs: Vec<cpal::SupportedStreamConfigRange>,
    stream: Option<cpal::Stream>,
    last_output_signature: Option<OutputConfigSignature>,
}

impl AudioPlayer {
    fn canonicalize_requested_device_name(device_name: &str) -> Option<String> {
        let trimmed = device_name.trim();
        if trimmed.is_empty() {
            return None;
        }
        let normalized = trimmed.to_ascii_lowercase();
        if normalized == "default"
            || normalized == "sysdefault"
            || normalized.starts_with("sysdefault:")
        {
            return None;
        }
        Some(trimmed.to_string())
    }

    fn output_signature_from_config(config: &crate::config::Config) -> OutputConfigSignature {
        OutputConfigSignature {
            device_name: Self::canonicalize_requested_device_name(
                &config.output.output_device_name,
            ),
            sample_rate_hz: config.output.sample_rate_khz.max(8_000),
            channel_count: config.output.channel_count.max(1),
            bits_per_sample: config.output.bits_per_sample.max(8),
            dither_on_bitdepth_reduce: config.output.dither_on_bitdepth_reduce,
        }
    }

    fn current_output_signature(&self) -> OutputConfigSignature {
        OutputConfigSignature {
            device_name: self.target_output_device_name.lock().unwrap().clone(),
            sample_rate_hz: self.target_sample_rate.load(Ordering::Relaxed) as u32,
            channel_count: self.target_channels.load(Ordering::Relaxed) as u16,
            bits_per_sample: self.target_bits_per_sample.max(8),
            dither_on_bitdepth_reduce: self.dither_on_bitdepth_reduce,
        }
    }

    fn output_sample_format_from_cpal(sample_format: cpal::SampleFormat) -> OutputSampleFormat {
        match sample_format {
            cpal::SampleFormat::F32 => OutputSampleFormat::F32,
            cpal::SampleFormat::I16 => OutputSampleFormat::I16,
            cpal::SampleFormat::U16 => OutputSampleFormat::U16,
            _ => OutputSampleFormat::Unknown,
        }
    }

    fn score_sample_format(sample_format: cpal::SampleFormat, requested_bits: u16) -> u64 {
        let bits = (sample_format.sample_size() * 8) as u16;
        match sample_format {
            cpal::SampleFormat::F32 => 0,
            cpal::SampleFormat::I16 => 20,
            cpal::SampleFormat::U16 => 30,
            _ => 200 + u64::from(bits.abs_diff(requested_bits)),
        }
    }

    fn choose_sample_rate_for_range(
        range: &cpal::SupportedStreamConfigRange,
        requested_sample_rate: u32,
    ) -> u32 {
        const COMMON_SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        let min_rate = range.min_sample_rate().0;
        let max_rate = range.max_sample_rate().0;
        if requested_sample_rate >= min_rate && requested_sample_rate <= max_rate {
            return requested_sample_rate;
        }
        COMMON_SAMPLE_RATES
            .iter()
            .copied()
            .filter(|rate| *rate >= min_rate && *rate <= max_rate)
            .min_by_key(|rate| rate.abs_diff(requested_sample_rate))
            .unwrap_or_else(|| requested_sample_rate.clamp(min_rate, max_rate))
    }

    fn choose_best_stream_config(
        supported_configs: &[cpal::SupportedStreamConfigRange],
        requested_sample_rate: u32,
        requested_channels: u16,
        requested_bits: u16,
    ) -> Option<cpal::SupportedStreamConfig> {
        let mut best: Option<(u64, cpal::SupportedStreamConfig)> = None;
        for range in supported_configs {
            let candidate_sample_rate =
                Self::choose_sample_rate_for_range(range, requested_sample_rate.max(8_000));
            let candidate = range.with_sample_rate(cpal::SampleRate(candidate_sample_rate));
            let channel_penalty =
                u64::from(candidate.channels().abs_diff(requested_channels)) * 1_000;
            let sample_rate_penalty = u64::from(
                candidate
                    .sample_rate()
                    .0
                    .abs_diff(requested_sample_rate.max(8_000)),
            );
            let sample_format_penalty =
                Self::score_sample_format(candidate.sample_format(), requested_bits);
            let score = channel_penalty + sample_rate_penalty + sample_format_penalty;
            match &best {
                Some((best_score, _)) if *best_score <= score => {}
                _ => best = Some((score, candidate)),
            }
        }
        best.map(|(_, candidate)| candidate)
    }

    fn build_output_stream_info(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        sample_format: cpal::SampleFormat,
    ) -> OutputStreamInfo {
        let device_name = device
            .name()
            .unwrap_or_else(|_| "Unknown Device".to_string());
        OutputStreamInfo {
            device_name,
            sample_rate_hz: config.sample_rate.0,
            channel_count: config.channels,
            bits_per_sample: (sample_format.sample_size() * 8) as u16,
            sample_format: Self::output_sample_format_from_cpal(sample_format),
        }
    }

    fn output_path_info_for_metadata(
        metadata: &crate::protocol::TechnicalMetadata,
        stream_info: &OutputStreamInfo,
        dither_enabled: bool,
        downmix_higher_channel_tracks: bool,
    ) -> OutputPathInfo {
        let output_float = matches!(stream_info.sample_format, OutputSampleFormat::F32);
        let channel_transform = if metadata.channel_count != stream_info.channel_count {
            if metadata.channel_count > stream_info.channel_count && downmix_higher_channel_tracks {
                Some(ChannelTransformKind::Downmix)
            } else {
                Some(ChannelTransformKind::ChannelMap)
            }
        } else {
            None
        };
        OutputPathInfo {
            source_sample_rate_hz: metadata.sample_rate_hz,
            source_channel_count: metadata.channel_count,
            output_stream: stream_info.clone(),
            resampled: metadata.sample_rate_hz != stream_info.sample_rate_hz,
            channel_transform,
            dithered: dither_enabled && !output_float,
        }
    }

    fn lcg_next(state: &mut u64) -> f32 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((*state >> 32) as u32) as f32 / u32::MAX as f32
    }

    fn tpdf_noise(state: &mut u64) -> f32 {
        Self::lcg_next(state) + Self::lcg_next(state) - 1.0
    }

    fn quantize_i16(sample: f32, dither: bool, dither_state: &mut u64) -> i16 {
        let mut clamped = sample.clamp(-1.0, 1.0);
        if dither {
            clamped += Self::tpdf_noise(dither_state) / i16::MAX as f32;
        }
        (clamped * i16::MAX as f32)
            .round()
            .clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }

    fn quantize_u16(sample: f32, dither: bool, dither_state: &mut u64) -> u16 {
        let mut clamped = sample.clamp(-1.0, 1.0);
        if dither {
            clamped += Self::tpdf_noise(dither_state) / u16::MAX as f32;
        }
        ((clamped * 0.5 + 0.5) * u16::MAX as f32)
            .round()
            .clamp(0.0, u16::MAX as f32) as u16
    }

    fn queue_entry_len(entry: &AudioQueueEntry) -> usize {
        match entry {
            AudioQueueEntry::Samples(samples) => samples.len(),
            AudioQueueEntry::TrackHeader(_) | AudioQueueEntry::TrackFooter(_) => 1,
        }
    }

    fn locate_position_in_queue(
        queue: &VecDeque<AudioQueueEntry>,
        queue_start_position: usize,
        target_position: usize,
    ) -> Option<(usize, usize)> {
        if target_position < queue_start_position {
            return None;
        }

        let mut cursor = queue_start_position;
        for (entry_index, entry) in queue.iter().enumerate() {
            let len = Self::queue_entry_len(entry);
            if target_position < cursor.saturating_add(len) {
                return Some((entry_index, target_position - cursor));
            }
            cursor = cursor.saturating_add(len);
        }
        None
    }

    fn truncate_queue_after_position(
        queue: &mut VecDeque<AudioQueueEntry>,
        queue_start_position: usize,
        keep_inclusive_position: usize,
    ) -> usize {
        let keep_exclusive_position = keep_inclusive_position.saturating_add(1);
        let mut cursor = queue_start_position;
        let mut truncated = VecDeque::new();

        while let Some(entry) = queue.pop_front() {
            if cursor >= keep_exclusive_position {
                break;
            }

            let len = Self::queue_entry_len(&entry);
            if cursor.saturating_add(len) <= keep_exclusive_position {
                cursor = cursor.saturating_add(len);
                truncated.push_back(entry);
                continue;
            }

            let keep_len = keep_exclusive_position.saturating_sub(cursor);
            match entry {
                AudioQueueEntry::Samples(mut samples) => {
                    samples.truncate(keep_len);
                    if !samples.is_empty() {
                        truncated.push_back(AudioQueueEntry::Samples(samples));
                        cursor = cursor.saturating_add(keep_len);
                    }
                }
                AudioQueueEntry::TrackHeader(header) => {
                    if keep_len > 0 {
                        truncated.push_back(AudioQueueEntry::TrackHeader(header));
                        cursor = cursor.saturating_add(1);
                    }
                }
                AudioQueueEntry::TrackFooter(id) => {
                    if keep_len > 0 {
                        truncated.push_back(AudioQueueEntry::TrackFooter(id));
                        cursor = cursor.saturating_add(1);
                    }
                }
            }
            break;
        }

        *queue = truncated;
        cursor
    }

    /// Creates an audio player, initializes output device, and spawns helper threads.
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        let is_playing = Arc::new(AtomicBool::new(false));
        let current_track_position = Arc::new(AtomicUsize::new(0));
        let cached_track_indices = Arc::new(Mutex::new(HashMap::new()));
        let current_track_id = Arc::new(Mutex::new(String::new()));
        let current_metadata = Arc::new(Mutex::new(None));
        let decode_bootstrap_pending = Arc::new(AtomicBool::new(false));
        let current_track_offset_ms = Arc::new(AtomicUsize::new(0));
        let queue_start_position = Arc::new(AtomicUsize::new(0));
        let queue_end_position = Arc::new(AtomicUsize::new(0));
        let target_sample_rate = Arc::new(AtomicUsize::new(44100));
        let target_channels = Arc::new(AtomicUsize::new(2));
        let target_output_device_name = Arc::new(Mutex::new(None));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let buffer_low_watermark_ms = Arc::new(AtomicUsize::new(12_000));
        let buffer_target_ms = Arc::new(AtomicUsize::new(24_000));
        let buffer_request_interval_ms = Arc::new(AtomicUsize::new(120));

        let mut player = Self {
            bus_receiver,
            bus_sender: bus_sender.clone(),
            sample_queue: Arc::new(Mutex::new(VecDeque::new())),
            queue_start_position: queue_start_position.clone(),
            queue_end_position: queue_end_position.clone(),
            cached_track_indices: cached_track_indices.clone(),
            pending_immediate_start_track_id: None,
            is_playing: is_playing.clone(),
            device: None,
            config: None,
            stream: None,
            sample_format: None,
            cached_requested_device_name: None,
            cached_supported_output_configs: Vec::new(),
            target_sample_rate: target_sample_rate.clone(),
            target_channels: target_channels.clone(),
            target_bits_per_sample: 24,
            dither_on_bitdepth_reduce: true,
            downmix_higher_channel_tracks: true,
            target_output_device_name,
            output_stream_info: Arc::new(Mutex::new(None)),
            current_track_id: current_track_id.clone(),
            current_track_position: current_track_position.clone(),
            current_track_offset_ms: current_track_offset_ms.clone(),
            current_metadata: current_metadata.clone(),
            decode_bootstrap_pending: decode_bootstrap_pending.clone(),
            volume: volume.clone(),
            buffer_low_watermark_ms: buffer_low_watermark_ms.clone(),
            buffer_target_ms: buffer_target_ms.clone(),
            buffer_request_interval_ms: buffer_request_interval_ms.clone(),
            last_output_signature: None,
        };

        if player.setup_audio_device() {
            player.last_output_signature = Some(player.current_output_signature());
        }

        // Spawn progress reporter thread
        let bus_sender_clone = bus_sender.clone();
        let is_playing_clone = is_playing.clone();
        let current_track_position_clone = current_track_position.clone();
        let cached_track_indices_clone = cached_track_indices.clone();
        let current_track_id_clone = current_track_id.clone();
        let current_metadata_clone = current_metadata.clone();
        let current_track_offset_ms_clone = current_track_offset_ms.clone();
        let target_sample_rate_clone = target_sample_rate.clone();
        let target_channels_clone = target_channels.clone();

        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(50));
            if is_playing_clone.load(Ordering::Relaxed) {
                let metadata = current_metadata_clone.lock().unwrap().clone();
                let track_id = current_track_id_clone.lock().unwrap().clone();
                let sample_rate = target_sample_rate_clone.load(Ordering::Relaxed);
                let channels = target_channels_clone.load(Ordering::Relaxed);
                let offset_ms = current_track_offset_ms_clone.load(Ordering::Relaxed) as u64;

                if let Some(meta) = metadata {
                    let current_pos = current_track_position_clone.load(Ordering::Relaxed);
                    let start_pos = cached_track_indices_clone
                        .lock()
                        .unwrap()
                        .get(&track_id)
                        .map(|i| i.start)
                        .unwrap_or(0);

                    let elapsed_samples = current_pos.saturating_sub(start_pos);
                    if sample_rate > 0 && channels > 0 {
                        let elapsed_ms = offset_ms
                            + (elapsed_samples as f64 * 1000.0
                                / (sample_rate as f64 * channels as f64))
                                as u64;

                        // debug!("Track id {} current_pos: {}, start_pos: {}, elapsed_samples: {}, offset_ms: {} elapsed_ms: {}", track_id, current_pos, start_pos, elapsed_samples, offset_ms, elapsed_ms);

                        let _ = bus_sender_clone.send(Message::Playback(
                            PlaybackMessage::PlaybackProgress {
                                elapsed_ms,
                                total_ms: meta.duration_ms,
                            },
                        ));
                    }
                }
            }
        });

        // Spawn decode prefetch thread. It requests more decoded audio when
        // buffered samples ahead of playback fall below a configurable threshold.
        let bus_sender_clone = bus_sender.clone();
        let current_track_position_clone = current_track_position.clone();
        let queue_end_position_clone = queue_end_position.clone();
        let target_sample_rate_clone = target_sample_rate.clone();
        let target_channels_clone = target_channels.clone();
        let current_metadata_clone = current_metadata.clone();
        let decode_bootstrap_pending_clone = decode_bootstrap_pending.clone();
        let buffer_low_watermark_ms_clone = buffer_low_watermark_ms.clone();
        let buffer_target_ms_clone = buffer_target_ms.clone();
        let buffer_request_interval_ms_clone = buffer_request_interval_ms.clone();
        thread::spawn(move || loop {
            let interval_ms = buffer_request_interval_ms_clone
                .load(Ordering::Relaxed)
                .max(20) as u64;
            thread::sleep(Duration::from_millis(interval_ms));

            let sample_rate = target_sample_rate_clone.load(Ordering::Relaxed);
            let channels = target_channels_clone.load(Ordering::Relaxed);
            let low_watermark_ms = buffer_low_watermark_ms_clone.load(Ordering::Relaxed);
            let target_buffer_ms = buffer_target_ms_clone
                .load(Ordering::Relaxed)
                .max(low_watermark_ms.saturating_add(500));

            let low_watermark_samples =
                Self::milliseconds_to_samples(low_watermark_ms, sample_rate, channels);
            let target_buffer_samples =
                Self::milliseconds_to_samples(target_buffer_ms, sample_rate, channels);

            let has_active_track = current_metadata_clone
                .lock()
                .map(|metadata| metadata.is_some())
                .unwrap_or(false);
            let has_bootstrap_pending = decode_bootstrap_pending_clone.load(Ordering::Relaxed);

            let current_position = current_track_position_clone.load(Ordering::Relaxed);
            let queue_end_position = queue_end_position_clone.load(Ordering::Relaxed);
            let buffered_samples = queue_end_position.saturating_sub(current_position);

            let requested_samples = Self::compute_decode_request_samples_for_state(
                has_active_track,
                has_bootstrap_pending,
                buffered_samples,
                low_watermark_samples,
                target_buffer_samples,
            );
            if requested_samples > 0 {
                let _ = bus_sender_clone.send(Message::Audio(AudioMessage::RequestDecodeChunk {
                    requested_samples,
                }));
            }
        });

        player
    }

    fn milliseconds_to_samples(milliseconds: usize, sample_rate: usize, channels: usize) -> usize {
        let sr = sample_rate.max(1) as u128;
        let ch = channels.max(1) as u128;
        let samples = milliseconds as u128 * sr * ch / 1000;
        samples.min(usize::MAX as u128) as usize
    }

    fn compute_decode_request_samples(
        buffered_samples: usize,
        low_watermark_samples: usize,
        target_buffer_samples: usize,
    ) -> usize {
        if buffered_samples >= low_watermark_samples {
            return 0;
        }
        target_buffer_samples.saturating_sub(buffered_samples)
    }

    fn compute_decode_request_samples_for_state(
        has_active_track: bool,
        has_bootstrap_pending: bool,
        buffered_samples: usize,
        low_watermark_samples: usize,
        target_buffer_samples: usize,
    ) -> usize {
        if !has_active_track && !has_bootstrap_pending {
            return 0;
        }
        Self::compute_decode_request_samples(
            buffered_samples,
            low_watermark_samples,
            target_buffer_samples,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_output_buffer<T, F>(
        output_buffer: &mut [T],
        is_playing: &Arc<AtomicBool>,
        sample_queue: &Arc<Mutex<VecDeque<AudioQueueEntry>>>,
        queue_start_position: &Arc<AtomicUsize>,
        queue_end_position: &Arc<AtomicUsize>,
        cached_track_indices: &Arc<Mutex<HashMap<String, TrackIndex>>>,
        current_track_id: &Arc<Mutex<String>>,
        bus_sender: &Sender<Message>,
        current_track_position: &Arc<AtomicUsize>,
        volume: &Arc<AtomicU32>,
        mut convert_sample: F,
        silence_value: T,
    ) where
        T: Copy,
        F: FnMut(f32) -> T,
    {
        if !is_playing.load(Ordering::Relaxed) {
            output_buffer.fill(silence_value);
            return;
        }

        let mut sample_queue_unlocked = sample_queue.lock().unwrap();
        let mut queue_start = queue_start_position.load(Ordering::Relaxed);
        let mut input_current_position = current_track_position.load(Ordering::Relaxed);
        if input_current_position < queue_start {
            input_current_position = queue_start;
        }
        let mut output_current_position = 0;
        let gain = f32::from_bits(volume.load(Ordering::Relaxed)).clamp(0.0, 1.0);
        let mut queue_cursor = Self::locate_position_in_queue(
            &sample_queue_unlocked,
            queue_start,
            input_current_position,
        );

        while output_current_position < output_buffer.len() {
            let Some((entry_index, entry_offset)) = queue_cursor else {
                for sample in &mut output_buffer[output_current_position..] {
                    *sample = silence_value;
                }
                break;
            };

            let Some(entry) = sample_queue_unlocked.get(entry_index) else {
                for sample in &mut output_buffer[output_current_position..] {
                    *sample = silence_value;
                }
                break;
            };

            match entry {
                AudioQueueEntry::Samples(samples) => {
                    if entry_offset >= samples.len() {
                        queue_cursor = Some((entry_index + 1, 0));
                        continue;
                    }
                    let sample = samples[entry_offset] * gain;
                    output_buffer[output_current_position] = convert_sample(sample);
                    input_current_position = input_current_position.saturating_add(1);
                    output_current_position += 1;

                    if entry_offset + 1 < samples.len() {
                        queue_cursor = Some((entry_index, entry_offset + 1));
                    } else {
                        queue_cursor = Some((entry_index + 1, 0));
                    }
                }
                AudioQueueEntry::TrackHeader(TrackHeader {
                    id,
                    start_offset_ms,
                }) => {
                    let _ = bus_sender.send(Message::Playback(PlaybackMessage::TrackStarted(
                        TrackStarted {
                            id: id.clone(),
                            start_offset_ms: *start_offset_ms,
                        },
                    )));
                    input_current_position = input_current_position.saturating_add(1);
                    queue_cursor = Some((entry_index + 1, 0));
                }
                AudioQueueEntry::TrackFooter(id) => {
                    let _ = bus_sender.send(Message::Playback(PlaybackMessage::TrackFinished(
                        id.clone(),
                    )));
                    input_current_position = input_current_position.saturating_add(1);
                    is_playing.store(false, Ordering::Relaxed);
                    for sample in &mut output_buffer[output_current_position..] {
                        *sample = silence_value;
                    }
                    break;
                }
            }
        }

        let mut popped_any = false;
        while let Some(front) = sample_queue_unlocked.front() {
            let front_len = Self::queue_entry_len(front);
            if queue_start.saturating_add(front_len) <= input_current_position {
                sample_queue_unlocked.pop_front();
                queue_start = queue_start.saturating_add(front_len);
                popped_any = true;
            } else {
                break;
            }
        }
        current_track_position.store(input_current_position, Ordering::Relaxed);
        if popped_any {
            queue_start_position.store(queue_start, Ordering::Relaxed);
        }
        let queue_end = queue_end_position.load(Ordering::Relaxed).max(queue_start);
        queue_end_position.store(queue_end, Ordering::Relaxed);

        drop(sample_queue_unlocked);
        if popped_any {
            let active_track_id = current_track_id.lock().unwrap().clone();
            let mut indices = cached_track_indices.lock().unwrap();
            let evicted_ids: Vec<String> = indices
                .iter()
                .filter_map(|(id, info)| {
                    if id == &active_track_id {
                        return None;
                    }
                    match info.end {
                        Some(end) if end < queue_start => Some(id.clone()),
                        _ => None,
                    }
                })
                .collect();

            for evicted_id in &evicted_ids {
                indices.remove(evicted_id);
            }
            drop(indices);

            for evicted_id in evicted_ids {
                let _ = bus_sender.send(Message::Audio(AudioMessage::TrackEvicted(evicted_id)));
            }
        }
    }

    fn setup_audio_device(&mut self) -> bool {
        let requested_device_name = self
            .target_output_device_name
            .lock()
            .unwrap()
            .as_ref()
            .cloned();
        let reuse_cached_default_device = requested_device_name.is_none()
            && self.cached_requested_device_name.is_none()
            && !self.cached_supported_output_configs.is_empty()
            && self.device.is_some();
        let (device, configs) = if reuse_cached_default_device {
            debug!("AudioPlayer: Reusing cached default output device capabilities");
            (
                self.device
                    .as_ref()
                    .expect("cached default device should exist")
                    .clone(),
                self.cached_supported_output_configs.clone(),
            )
        } else {
            let host = cpal::default_host();
            let selected_device = requested_device_name.as_ref().and_then(|device_name| {
                host.output_devices().ok().and_then(|devices| {
                    devices
                        .filter_map(|device| {
                            let name = device.name().ok()?;
                            if name == *device_name {
                                Some(device)
                            } else {
                                None
                            }
                        })
                        .next()
                })
            });
            if requested_device_name.is_some() && selected_device.is_none() {
                warn!(
                    "AudioPlayer: requested output device not found. Falling back to system default"
                );
            }
            let Some(device) = selected_device.or_else(|| host.default_output_device()) else {
                error!("No output device available");
                return false;
            };
            let configs = match device.supported_output_configs() {
                Ok(configs) => configs.collect::<Vec<_>>(),
                Err(e) => {
                    error!("Error getting device configs: {}", e);
                    return false;
                }
            };
            if requested_device_name.is_none() {
                self.cached_requested_device_name = None;
                self.cached_supported_output_configs = configs.clone();
            } else {
                self.cached_requested_device_name = None;
                self.cached_supported_output_configs.clear();
            }
            (device, configs)
        };

        let requested_sample_rate = self.target_sample_rate.load(Ordering::Relaxed) as u32;
        let requested_channels = self.target_channels.load(Ordering::Relaxed) as u16;
        let requested_bits = self.target_bits_per_sample.max(8);

        if configs.is_empty() {
            error!("No output configs reported for selected device");
            return false;
        }

        let Some(selected_config) = Self::choose_best_stream_config(
            &configs,
            requested_sample_rate,
            requested_channels,
            requested_bits,
        ) else {
            error!("No matching device config found");
            return false;
        };

        self.target_channels
            .store(selected_config.channels() as usize, Ordering::Relaxed);
        self.target_sample_rate
            .store(selected_config.sample_rate().0 as usize, Ordering::Relaxed);

        let stream_config: cpal::StreamConfig = selected_config.config();
        let sample_format = selected_config.sample_format();
        let stream_info = Self::build_output_stream_info(&device, &stream_config, sample_format);

        self.config = Some(stream_config);
        self.sample_format = Some(sample_format);
        self.device = Some(device);
        *self.output_stream_info.lock().unwrap() = Some(stream_info.clone());
        debug!(
            "AudioPlayer: Audio device initialized: device='{}' sr={} channels={} bits={} format={:?}",
            stream_info.device_name,
            stream_info.sample_rate_hz,
            stream_info.channel_count,
            stream_info.bits_per_sample,
            stream_info.sample_format
        );

        let _ = self
            .bus_sender
            .send(Message::Config(ConfigMessage::AudioDeviceOpened {
                stream_info,
            }));
        true
    }

    fn apply_runtime_output_sample_rate_change(&mut self, sample_rate_hz: u32) {
        let requested_sample_rate_hz = sample_rate_hz.max(8_000);
        debug!(
            "AudioPlayer: Applying runtime output sample-rate change to {} Hz",
            requested_sample_rate_hz
        );
        let previous_sample_rate = self.target_sample_rate.load(Ordering::Relaxed);
        let previous_channels = self.target_channels.load(Ordering::Relaxed);
        let previous_bits_per_sample = self.target_bits_per_sample;
        let previous_dither = self.dither_on_bitdepth_reduce;
        let previous_device_name = self.target_output_device_name.lock().unwrap().clone();
        self.target_sample_rate
            .store(requested_sample_rate_hz as usize, Ordering::Relaxed);

        if self.setup_audio_device() {
            if self.stream.is_some() {
                self.stream = None;
                self.create_stream();
            }
            self.last_output_signature = Some(self.current_output_signature());
            if let Some(metadata) = self.current_metadata.lock().unwrap().clone() {
                self.emit_output_path_for_metadata(&metadata);
            }
        } else {
            self.target_sample_rate
                .store(previous_sample_rate, Ordering::Relaxed);
            self.target_channels
                .store(previous_channels, Ordering::Relaxed);
            self.target_bits_per_sample = previous_bits_per_sample;
            self.dither_on_bitdepth_reduce = previous_dither;
            *self.target_output_device_name.lock().unwrap() = previous_device_name;
        }
    }

    fn create_stream(&mut self) {
        if self.stream.is_some() {
            return;
        }

        let Some(device) = self.device.as_ref() else {
            warn!("AudioPlayer: cannot create stream without an initialized output device");
            return;
        };
        let Some(config) = self.config.as_ref() else {
            warn!("AudioPlayer: cannot create stream without an initialized stream config");
            return;
        };
        let sample_format = self.sample_format.unwrap_or(cpal::SampleFormat::F32);

        let sample_queue = self.sample_queue.clone();
        let queue_start_position = self.queue_start_position.clone();
        let queue_end_position = self.queue_end_position.clone();
        let cached_track_indices = self.cached_track_indices.clone();
        let current_track_id = self.current_track_id.clone();
        let bus_sender_clone = self.bus_sender.clone();
        let is_playing = self.is_playing.clone();
        let current_track_position = self.current_track_position.clone();
        let volume = self.volume.clone();
        let dither_on_bitdepth_reduce = self.dither_on_bitdepth_reduce;

        let stream_result = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config,
                move |output_buffer: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    Self::render_output_buffer(
                        output_buffer,
                        &is_playing,
                        &sample_queue,
                        &queue_start_position,
                        &queue_end_position,
                        &cached_track_indices,
                        &current_track_id,
                        &bus_sender_clone,
                        &current_track_position,
                        &volume,
                        |sample| sample.clamp(-1.0, 1.0),
                        0.0,
                    );
                },
                |err| error!("Audio stream error: {}", err),
                None,
            ),
            cpal::SampleFormat::I16 => {
                let mut dither_state = 0x6d_75_73_69_63_5f_70_6c_u64;
                device.build_output_stream(
                    config,
                    move |output_buffer: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        Self::render_output_buffer(
                            output_buffer,
                            &is_playing,
                            &sample_queue,
                            &queue_start_position,
                            &queue_end_position,
                            &cached_track_indices,
                            &current_track_id,
                            &bus_sender_clone,
                            &current_track_position,
                            &volume,
                            |sample| {
                                Self::quantize_i16(
                                    sample,
                                    dither_on_bitdepth_reduce,
                                    &mut dither_state,
                                )
                            },
                            0,
                        );
                    },
                    |err| error!("Audio stream error: {}", err),
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut dither_state = 0x72_6f_71_74_75_6e_65_01_u64;
                device.build_output_stream(
                    config,
                    move |output_buffer: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        Self::render_output_buffer(
                            output_buffer,
                            &is_playing,
                            &sample_queue,
                            &queue_start_position,
                            &queue_end_position,
                            &cached_track_indices,
                            &current_track_id,
                            &bus_sender_clone,
                            &current_track_position,
                            &volume,
                            |sample| {
                                Self::quantize_u16(
                                    sample,
                                    dither_on_bitdepth_reduce,
                                    &mut dither_state,
                                )
                            },
                            u16::MAX / 2 + 1,
                        );
                    },
                    |err| error!("Audio stream error: {}", err),
                    None,
                )
            }
            other => {
                error!("Unsupported output sample format: {:?}", other);
                return;
            }
        };

        match stream_result {
            Ok(stream) => {
                let _ = stream.play();
                self.stream = Some(stream);
                debug!("Audio stream created");
            }
            Err(e) => error!("Failed to build audio stream: {}", e),
        }
    }

    fn emit_output_path_for_metadata(&self, metadata: &crate::protocol::TechnicalMetadata) {
        let stream_info = self.output_stream_info.lock().unwrap().clone();
        let Some(stream_info) = stream_info else {
            return;
        };
        let output_path = Self::output_path_info_for_metadata(
            metadata,
            &stream_info,
            self.dither_on_bitdepth_reduce,
            self.downmix_higher_channel_tracks,
        );
        let _ = self
            .bus_sender
            .send(Message::Playback(PlaybackMessage::OutputPathChanged(
                output_path,
            )));
    }

    fn load_samples(&mut self, samples: AudioPacket) {
        if self.stream.is_none() {
            self.create_stream();
        }

        match samples {
            AudioPacket::Samples { samples, .. } => {
                if samples.is_empty() {
                    return;
                }
                self.decode_bootstrap_pending
                    .store(false, Ordering::Relaxed);
                let sample_count = samples.len();
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioQueueEntry::Samples(samples));
                drop(queue);
                self.queue_end_position
                    .fetch_add(sample_count, Ordering::Relaxed);
                if let Some(pending_track_id) = self.pending_immediate_start_track_id.clone() {
                    let current_track_id = self.current_track_id.lock().unwrap().clone();
                    if current_track_id == pending_track_id {
                        self.is_playing.store(true, Ordering::Relaxed);
                        self.pending_immediate_start_track_id = None;
                        debug!(
                            "AudioPlayer: Playback started after first samples buffered for {}",
                            current_track_id
                        );
                    } else {
                        self.pending_immediate_start_track_id = None;
                    }
                }
            }
            AudioPacket::TrackHeader {
                id,
                play_immediately,
                technical_metadata,
                start_offset_ms,
            } => {
                self.decode_bootstrap_pending
                    .store(false, Ordering::Relaxed);
                let start_index = self.queue_end_position.load(Ordering::Relaxed);
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioQueueEntry::TrackHeader(TrackHeader {
                    id: id.clone(),
                    start_offset_ms,
                }));
                drop(queue);
                self.queue_end_position.fetch_add(1, Ordering::Relaxed);

                // debug!("AudioPlayer: Loaded track header: id {} technical metadata {:?}", id, technical_metadata);
                self.cached_track_indices.lock().unwrap().insert(
                    id.clone(),
                    TrackIndex {
                        start: start_index,
                        end: None,
                        start_offset_ms,
                        technical_metadata: technical_metadata.clone(),
                    },
                );

                if play_immediately {
                    // This is the case where some kind of user action caused immediate playback
                    *self.current_track_id.lock().unwrap() = id.clone();
                    *self.current_metadata.lock().unwrap() = Some(technical_metadata.clone());
                    self.current_track_offset_ms
                        .store(start_offset_ms as usize, Ordering::Relaxed);
                    self.current_track_position
                        .store(start_index, Ordering::Relaxed);
                    self.pending_immediate_start_track_id = Some(id.clone());
                    self.is_playing.store(false, Ordering::Relaxed);
                    let _ = self.bus_sender.send(Message::Playback(
                        PlaybackMessage::TechnicalMetadataChanged(technical_metadata),
                    ));
                    let metadata_for_path = self.current_metadata.lock().unwrap().clone();
                    if let Some(metadata_for_path) = metadata_for_path.as_ref() {
                        self.emit_output_path_for_metadata(metadata_for_path);
                    }
                }
            }
            AudioPacket::TrackFooter { id } => {
                self.decode_bootstrap_pending
                    .store(false, Ordering::Relaxed);
                let footer_index = self.queue_end_position.load(Ordering::Relaxed);
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioQueueEntry::TrackFooter(id.clone()));
                drop(queue);
                self.queue_end_position.fetch_add(1, Ordering::Relaxed);

                let start_offset_ms =
                    if let Some(info) = self.cached_track_indices.lock().unwrap().get_mut(&id) {
                        info.end = Some(footer_index);
                        info.start_offset_ms
                    } else {
                        0
                    };

                self.bus_sender
                    .send(Message::Audio(AudioMessage::TrackCached(
                        id.clone(),
                        start_offset_ms,
                    )))
                    .unwrap();

                let is_current = *self.current_track_id.lock().unwrap() == id;
                if self.pending_immediate_start_track_id.as_deref() == Some(id.as_str()) {
                    self.pending_immediate_start_track_id = None;
                }
                if !is_current || !self.is_playing.load(Ordering::Relaxed) {
                    self.bus_sender
                        .send(Message::Playback(PlaybackMessage::ReadyForPlayback(id)))
                        .unwrap();
                }
            }
        }
    }

    /// Starts the blocking event loop that reacts to bus messages.
    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    Message::Audio(AudioMessage::DecodeTracks(tracks)) => {
                        let needs_bootstrap = tracks.iter().any(|track| track.play_immediately);
                        self.decode_bootstrap_pending
                            .store(needs_bootstrap, Ordering::Relaxed);
                    }
                    Message::Audio(AudioMessage::AudioPacket(buffer)) => {
                        self.load_samples(buffer);
                    }
                    Message::Playback(PlaybackMessage::Play) => {
                        self.pending_immediate_start_track_id = None;
                        self.is_playing.store(true, Ordering::Relaxed);
                        debug!("AudioPlayer: Playback resumed");
                    }
                    Message::Playback(PlaybackMessage::Pause) => {
                        self.is_playing.store(false, Ordering::Relaxed);
                        debug!("AudioPlayer: Playback paused");
                    }
                    Message::Playback(PlaybackMessage::Stop) => {
                        self.pending_immediate_start_track_id = None;
                        self.is_playing.store(false, Ordering::Relaxed);
                        self.decode_bootstrap_pending
                            .store(false, Ordering::Relaxed);
                        debug!("AudioPlayer: Playback stopped");
                    }
                    Message::Playback(PlaybackMessage::PlayTrackById(id)) => {
                        self.pending_immediate_start_track_id = None;
                        let queue_start = self.queue_start_position.load(Ordering::Relaxed);
                        let mut indices = self.cached_track_indices.lock().unwrap();
                        let track_info = indices.get(&id).cloned();
                        if let Some(info) = track_info {
                            if info.start < queue_start {
                                indices.remove(&id);
                                let _ = self
                                    .bus_sender
                                    .send(Message::Audio(AudioMessage::TrackEvicted(id)));
                                continue;
                            }
                            *self.current_track_id.lock().unwrap() = id;
                            *self.current_metadata.lock().unwrap() =
                                Some(info.technical_metadata.clone());
                            self.current_track_position
                                .store(info.start, Ordering::Relaxed);
                            self.current_track_offset_ms.store(0, Ordering::Relaxed);
                            self.is_playing.store(true, Ordering::Relaxed);
                            debug!("AudioPlayer: Playback started (manual)");
                            let _ = self.bus_sender.send(Message::Playback(
                                PlaybackMessage::TechnicalMetadataChanged(info.technical_metadata),
                            ));
                            let metadata_for_path = self.current_metadata.lock().unwrap().clone();
                            if let Some(metadata_for_path) = metadata_for_path.as_ref() {
                                self.emit_output_path_for_metadata(metadata_for_path);
                            }
                        }
                    }
                    Message::Playback(PlaybackMessage::ClearNextTracks) => {
                        let current_id = self.current_track_id.lock().unwrap().clone();
                        let mut indices = self.cached_track_indices.lock().unwrap();
                        let mut queue = self.sample_queue.lock().unwrap();
                        let queue_start = self.queue_start_position.load(Ordering::Relaxed);

                        if let Some(info) = indices.get(&current_id) {
                            if let Some(footer_pos) = info.end {
                                // Truncate queue after current track's footer.
                                let new_queue_end = Self::truncate_queue_after_position(
                                    &mut queue,
                                    queue_start,
                                    footer_pos,
                                );
                                self.queue_end_position
                                    .store(new_queue_end, Ordering::Relaxed);
                                let current_position =
                                    self.current_track_position.load(Ordering::Relaxed);
                                if current_position > new_queue_end {
                                    self.current_track_position
                                        .store(new_queue_end, Ordering::Relaxed);
                                }

                                // Remove all other tracks from indices
                                indices.retain(|id, _| id == &current_id);
                                debug!("AudioPlayer: Cleared next tracks after {}", current_id);
                            }
                        }
                    }
                    Message::Playback(PlaybackMessage::ClearPlayerCache) => {
                        self.pending_immediate_start_track_id = None;
                        self.is_playing.store(false, Ordering::Relaxed);
                        self.decode_bootstrap_pending
                            .store(false, Ordering::Relaxed);
                        self.sample_queue.lock().unwrap().clear();
                        self.cached_track_indices.lock().unwrap().clear();
                        self.queue_start_position.store(0, Ordering::Relaxed);
                        self.queue_end_position.store(0, Ordering::Relaxed);
                        self.current_track_position.store(0, Ordering::Relaxed);
                        *self.current_metadata.lock().unwrap() = None;
                        debug!("AudioPlayer: Cache cleared");
                    }
                    Message::Config(ConfigMessage::ConfigLoaded(config)) => {
                        let low_watermark_ms = config.buffering.player_low_watermark_ms as usize;
                        let target_buffer_ms = (config.buffering.player_target_buffer_ms as usize)
                            .max(low_watermark_ms.saturating_add(500));
                        self.buffer_low_watermark_ms
                            .store(low_watermark_ms, Ordering::Relaxed);
                        self.buffer_target_ms
                            .store(target_buffer_ms, Ordering::Relaxed);
                        self.buffer_request_interval_ms.store(
                            config.buffering.player_request_interval_ms.max(20) as usize,
                            Ordering::Relaxed,
                        );
                        self.downmix_higher_channel_tracks =
                            config.output.downmix_higher_channel_tracks;
                        let next_output_signature = Self::output_signature_from_config(&config);
                        if self.last_output_signature.as_ref() != Some(&next_output_signature) {
                            let previous_sample_rate =
                                self.target_sample_rate.load(Ordering::Relaxed);
                            let previous_channels = self.target_channels.load(Ordering::Relaxed);
                            let previous_bits_per_sample = self.target_bits_per_sample;
                            let previous_dither = self.dither_on_bitdepth_reduce;
                            let previous_device_name =
                                self.target_output_device_name.lock().unwrap().clone();

                            self.target_sample_rate.store(
                                next_output_signature.sample_rate_hz as usize,
                                Ordering::Relaxed,
                            );
                            self.target_channels.store(
                                next_output_signature.channel_count as usize,
                                Ordering::Relaxed,
                            );
                            self.target_bits_per_sample = next_output_signature.bits_per_sample;
                            self.dither_on_bitdepth_reduce =
                                next_output_signature.dither_on_bitdepth_reduce;
                            *self.target_output_device_name.lock().unwrap() =
                                next_output_signature.device_name.clone();

                            if self.setup_audio_device() {
                                if self.stream.is_some() {
                                    self.stream = None;
                                    self.create_stream();
                                }
                                self.last_output_signature = Some(next_output_signature);
                                if let Some(metadata) =
                                    self.current_metadata.lock().unwrap().clone()
                                {
                                    self.emit_output_path_for_metadata(&metadata);
                                }
                            } else {
                                self.target_sample_rate
                                    .store(previous_sample_rate, Ordering::Relaxed);
                                self.target_channels
                                    .store(previous_channels, Ordering::Relaxed);
                                self.target_bits_per_sample = previous_bits_per_sample;
                                self.dither_on_bitdepth_reduce = previous_dither;
                                *self.target_output_device_name.lock().unwrap() =
                                    previous_device_name;
                            }
                        } else {
                            debug!(
                                "AudioPlayer: Skipping audio device reinit because output signature is unchanged"
                            );
                        }
                    }
                    Message::Audio(AudioMessage::StopDecoding) => {
                        self.decode_bootstrap_pending
                            .store(false, Ordering::Relaxed);
                    }
                    Message::Playback(PlaybackMessage::TrackStarted(track_started)) => {
                        debug!("AudioPlayer: Track started: {}", track_started.id);
                        // Handle the case when a track automatically starts playing (not caused by a user action)
                        self.current_track_id
                            .lock()
                            .unwrap()
                            .clone_from(&track_started.id);
                        self.current_track_offset_ms
                            .store(track_started.start_offset_ms as usize, Ordering::Relaxed);
                        let track_info = self
                            .cached_track_indices
                            .lock()
                            .unwrap()
                            .get(&track_started.id)
                            .cloned();
                        if let Some(info) = track_info {
                            let _ = self.bus_sender.send(Message::Playback(
                                PlaybackMessage::TechnicalMetadataChanged(
                                    info.technical_metadata.clone(),
                                ),
                            ));
                            *self.current_metadata.lock().unwrap() = Some(info.technical_metadata);
                            let metadata_for_path = self.current_metadata.lock().unwrap().clone();
                            if let Some(metadata_for_path) = metadata_for_path.as_ref() {
                                self.emit_output_path_for_metadata(metadata_for_path);
                            }
                        }
                    }
                    Message::Config(ConfigMessage::ConfigChanged(changes)) => {
                        let mut latest_output: Option<crate::config::OutputConfig> = None;
                        let mut latest_buffering: Option<crate::config::BufferingConfig> = None;
                        for change in changes {
                            match change {
                                crate::protocol::ConfigDeltaEntry::Output(output) => {
                                    latest_output = Some(output);
                                }
                                crate::protocol::ConfigDeltaEntry::Buffering(buffering) => {
                                    latest_buffering = Some(buffering);
                                }
                                crate::protocol::ConfigDeltaEntry::Cast(_)
                                | crate::protocol::ConfigDeltaEntry::Ui(_)
                                | crate::protocol::ConfigDeltaEntry::Library(_)
                                | crate::protocol::ConfigDeltaEntry::Integrations(_) => {}
                            }
                        }
                        if let Some(buffering) = latest_buffering {
                            let low_watermark_ms = buffering.player_low_watermark_ms as usize;
                            let target_buffer_ms = (buffering.player_target_buffer_ms as usize)
                                .max(low_watermark_ms.saturating_add(500));
                            self.buffer_low_watermark_ms
                                .store(low_watermark_ms, Ordering::Relaxed);
                            self.buffer_target_ms
                                .store(target_buffer_ms, Ordering::Relaxed);
                            self.buffer_request_interval_ms.store(
                                buffering.player_request_interval_ms.max(20) as usize,
                                Ordering::Relaxed,
                            );
                        }
                        let Some(output) = latest_output else {
                            continue;
                        };
                        self.downmix_higher_channel_tracks = output.downmix_higher_channel_tracks;
                        let next_output_signature = OutputConfigSignature {
                            sample_rate_hz: output.sample_rate_khz,
                            channel_count: output.channel_count,
                            bits_per_sample: output.bits_per_sample,
                            dither_on_bitdepth_reduce: output.dither_on_bitdepth_reduce,
                            device_name: Some(output.output_device_name),
                        };
                        if self.last_output_signature.as_ref() != Some(&next_output_signature) {
                            let previous_sample_rate =
                                self.target_sample_rate.load(Ordering::Relaxed);
                            let previous_channels = self.target_channels.load(Ordering::Relaxed);
                            let previous_bits_per_sample = self.target_bits_per_sample;
                            let previous_dither = self.dither_on_bitdepth_reduce;
                            let previous_device_name =
                                self.target_output_device_name.lock().unwrap().clone();

                            self.target_sample_rate.store(
                                next_output_signature.sample_rate_hz as usize,
                                Ordering::Relaxed,
                            );
                            self.target_channels.store(
                                next_output_signature.channel_count as usize,
                                Ordering::Relaxed,
                            );
                            self.target_bits_per_sample = next_output_signature.bits_per_sample;
                            self.dither_on_bitdepth_reduce =
                                next_output_signature.dither_on_bitdepth_reduce;
                            *self.target_output_device_name.lock().unwrap() =
                                next_output_signature.device_name.clone();

                            if self.setup_audio_device() {
                                if self.stream.is_some() {
                                    self.stream = None;
                                    self.create_stream();
                                }
                                self.last_output_signature = Some(next_output_signature);
                                if let Some(metadata) =
                                    self.current_metadata.lock().unwrap().clone()
                                {
                                    self.emit_output_path_for_metadata(&metadata);
                                }
                            } else {
                                self.target_sample_rate
                                    .store(previous_sample_rate, Ordering::Relaxed);
                                self.target_channels
                                    .store(previous_channels, Ordering::Relaxed);
                                self.target_bits_per_sample = previous_bits_per_sample;
                                self.dither_on_bitdepth_reduce = previous_dither;
                                *self.target_output_device_name.lock().unwrap() =
                                    previous_device_name;
                            }
                        }
                    }
                    Message::Config(ConfigMessage::RuntimeOutputSampleRateChanged {
                        sample_rate_hz,
                    }) => {
                        self.apply_runtime_output_sample_rate_change(sample_rate_hz);
                    }
                    Message::Playback(PlaybackMessage::SetVolume(volume)) => {
                        let clamped = volume.clamp(0.0, 1.0);
                        self.volume.store(clamped.to_bits(), Ordering::Relaxed);
                        debug!("AudioPlayer: Volume set to {:.2}", clamped);
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("AudioPlayer: bus lagged by {} messages", skipped);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioPlayer, AudioQueueEntry, TrackHeader};
    use crate::config::Config;
    use crate::protocol::{AudioPacket, Message, PlaybackMessage, TechnicalMetadata};
    use std::collections::{HashMap, VecDeque};
    use std::sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::sync::broadcast;
    use tokio::sync::broadcast::error::TryRecvError;

    #[test]
    fn test_milliseconds_to_samples_stereo() {
        let samples = AudioPlayer::milliseconds_to_samples(1000, 44_100, 2);
        assert_eq!(samples, 88_200);
    }

    #[test]
    fn test_milliseconds_to_samples_clamps_zero_inputs() {
        let samples = AudioPlayer::milliseconds_to_samples(1000, 0, 0);
        assert_eq!(samples, 1);
    }

    #[test]
    fn test_compute_decode_request_samples_when_above_watermark() {
        let request = AudioPlayer::compute_decode_request_samples(50_000, 20_000, 40_000);
        assert_eq!(request, 0);
    }

    #[test]
    fn test_compute_decode_request_samples_when_below_watermark() {
        let request = AudioPlayer::compute_decode_request_samples(10_000, 20_000, 40_000);
        assert_eq!(request, 30_000);
    }

    #[test]
    fn test_compute_decode_request_samples_for_state_without_active_track() {
        let request =
            AudioPlayer::compute_decode_request_samples_for_state(false, false, 0, 20_000, 40_000);
        assert_eq!(request, 0);
    }

    #[test]
    fn test_compute_decode_request_samples_for_state_with_active_track() {
        let request = AudioPlayer::compute_decode_request_samples_for_state(
            true, false, 10_000, 20_000, 40_000,
        );
        assert_eq!(request, 30_000);
    }

    #[test]
    fn test_compute_decode_request_samples_for_state_with_bootstrap_pending() {
        let request = AudioPlayer::compute_decode_request_samples_for_state(
            false, true, 10_000, 20_000, 40_000,
        );
        assert_eq!(request, 30_000);
    }

    #[test]
    fn test_output_signature_normalizes_empty_device_name() {
        let mut config = Config::default();
        config.output.output_device_name = "   ".to_string();

        let signature = AudioPlayer::output_signature_from_config(&config);
        assert_eq!(signature.device_name, None);
    }

    #[test]
    fn test_output_signature_clamps_output_values() {
        let mut config = Config::default();
        config.output.output_device_name = "Speakers".to_string();
        config.output.sample_rate_khz = 2_000;
        config.output.channel_count = 0;
        config.output.bits_per_sample = 4;

        let signature = AudioPlayer::output_signature_from_config(&config);
        assert_eq!(signature.device_name.as_deref(), Some("Speakers"));
        assert_eq!(signature.sample_rate_hz, 8_000);
        assert_eq!(signature.channel_count, 1);
        assert_eq!(signature.bits_per_sample, 8);
    }

    #[test]
    fn test_output_signature_treats_default_alias_as_system_default() {
        let mut config = Config::default();
        config.output.output_device_name = "default".to_string();

        let signature = AudioPlayer::output_signature_from_config(&config);
        assert_eq!(signature.device_name, None);
    }

    #[test]
    fn test_render_stops_after_footer_and_does_not_leak_next_track_audio() {
        let is_playing = Arc::new(AtomicBool::new(true));
        let sample_queue = Arc::new(Mutex::new(VecDeque::from(vec![
            AudioQueueEntry::TrackHeader(TrackHeader {
                id: "t1".to_string(),
                start_offset_ms: 0,
            }),
            AudioQueueEntry::Samples(vec![0.5, 0.25]),
            AudioQueueEntry::TrackFooter("t1".to_string()),
            AudioQueueEntry::TrackHeader(TrackHeader {
                id: "t2".to_string(),
                start_offset_ms: 0,
            }),
            AudioQueueEntry::Samples(vec![0.9, 0.8]),
        ])));
        let queue_start_position = Arc::new(AtomicUsize::new(0));
        let queue_end_position = Arc::new(AtomicUsize::new(7));
        let cached_track_indices = Arc::new(Mutex::new(HashMap::new()));
        let current_track_id = Arc::new(Mutex::new("t1".to_string()));
        let current_track_position = Arc::new(AtomicUsize::new(0));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let (bus_sender, mut bus_receiver) = broadcast::channel(32);
        let mut output = [0.0f32; 8];

        AudioPlayer::render_output_buffer(
            &mut output,
            &is_playing,
            &sample_queue,
            &queue_start_position,
            &queue_end_position,
            &cached_track_indices,
            &current_track_id,
            &bus_sender,
            &current_track_position,
            &volume,
            |sample| sample,
            0.0f32,
        );

        assert!(!is_playing.load(Ordering::Relaxed));
        assert_eq!(current_track_position.load(Ordering::Relaxed), 4);
        assert_eq!(queue_start_position.load(Ordering::Relaxed), 4);
        assert_eq!(output[0], 0.5);
        assert_eq!(output[1], 0.25);
        assert!(output[2..].iter().all(|sample| *sample == 0.0));
        let queue_guard = sample_queue.lock().expect("sample queue lock poisoned");
        assert!(
            matches!(queue_guard.front(), Some(AudioQueueEntry::TrackHeader(header)) if header.id == "t2")
        );
        drop(queue_guard);

        let mut saw_started_t1 = false;
        let mut saw_finished_t1 = false;
        let mut saw_started_t2 = false;
        loop {
            match bus_receiver.try_recv() {
                Ok(Message::Playback(PlaybackMessage::TrackStarted(track_started))) => {
                    if track_started.id == "t1" {
                        saw_started_t1 = true;
                    } else if track_started.id == "t2" {
                        saw_started_t2 = true;
                    }
                }
                Ok(Message::Playback(PlaybackMessage::TrackFinished(id))) => {
                    if id == "t1" {
                        saw_finished_t1 = true;
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => break,
            }
        }

        assert!(saw_started_t1);
        assert!(saw_finished_t1);
        assert!(!saw_started_t2);
    }

    #[test]
    fn test_play_immediately_waits_for_first_samples_before_starting() {
        let (bus_sender, bus_receiver) = broadcast::channel(32);
        let mut player = AudioPlayer::new(bus_receiver, bus_sender);
        let metadata = TechnicalMetadata {
            format: "FLAC".to_string(),
            bitrate_kbps: 900,
            sample_rate_hz: 44_100,
            channel_count: 2,
            duration_ms: 100_000,
            bits_per_sample: 16,
        };

        player.load_samples(AudioPacket::TrackHeader {
            id: "cold_start_track".to_string(),
            play_immediately: true,
            technical_metadata: metadata,
            start_offset_ms: 0,
        });

        assert_eq!(
            player.pending_immediate_start_track_id.as_deref(),
            Some("cold_start_track")
        );
        assert!(!player.is_playing.load(Ordering::Relaxed));

        player.load_samples(AudioPacket::Samples {
            samples: vec![0.5, 0.25, -0.25, -0.5],
        });

        assert_eq!(player.pending_immediate_start_track_id, None);
        assert!(player.is_playing.load(Ordering::Relaxed));
    }

    #[test]
    fn test_footer_clears_pending_immediate_start_without_samples() {
        let (bus_sender, bus_receiver) = broadcast::channel(32);
        let mut player = AudioPlayer::new(bus_receiver, bus_sender);
        let metadata = TechnicalMetadata {
            format: "MP3".to_string(),
            bitrate_kbps: 320,
            sample_rate_hz: 44_100,
            channel_count: 2,
            duration_ms: 25_000,
            bits_per_sample: 16,
        };

        player.load_samples(AudioPacket::TrackHeader {
            id: "empty_track".to_string(),
            play_immediately: true,
            technical_metadata: metadata,
            start_offset_ms: 0,
        });
        assert_eq!(
            player.pending_immediate_start_track_id.as_deref(),
            Some("empty_track")
        );

        player.load_samples(AudioPacket::TrackFooter {
            id: "empty_track".to_string(),
        });

        assert_eq!(player.pending_immediate_start_track_id, None);
        assert!(!player.is_playing.load(Ordering::Relaxed));
    }
}
