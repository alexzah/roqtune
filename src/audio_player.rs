use crate::protocol::{AudioMessage, AudioPacket, ConfigMessage, Message, PlaybackMessage};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, error, trace};
use rubato::{
    FftFixedIn, Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use symphonia::core::{codecs::Decoder, formats::Packet};
use tokio::sync::broadcast::{Receiver, Sender};

#[derive(Debug, Clone)]
enum AudioSample {
    Sample(f32),
    TrackHeader(String),
    TrackFooter(String),
}

#[derive(Debug, Clone)]
struct TrackIndex {
    start: usize,
    end: Option<usize>,
}

pub struct AudioPlayer {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    // Audio state
    target_sample_rate: u32,
    target_channels: u16,
    target_bits_per_sample: u16,
    sample_queue: Arc<Mutex<VecDeque<AudioSample>>>,
    is_playing: Arc<AtomicBool>,
    current_track_id: String,
    current_track_position: Arc<AtomicUsize>,

    // Setup cache
    cached_track_indices: Arc<Mutex<HashMap<String, TrackIndex>>>,
    max_cached_tracks: usize,

    // Audio stream
    config: Option<cpal::StreamConfig>,
    device: Option<cpal::Device>,
    stream: Option<cpal::Stream>,
    resampler: Option<FftFixedIn<f32>>,
}

impl AudioPlayer {
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        let mut player = Self {
            bus_receiver,
            bus_sender,
            sample_queue: Arc::new(Mutex::new(VecDeque::new())),
            cached_track_indices: Arc::new(Mutex::new(HashMap::new())),
            is_playing: Arc::new(AtomicBool::new(false)),
            device: None,
            config: None,
            stream: None,
            target_sample_rate: 0,
            target_channels: 0,
            target_bits_per_sample: 0,
            resampler: None,
            current_track_id: String::new(),
            current_track_position: Arc::new(AtomicUsize::new(0)),
            max_cached_tracks: 6,
        };

        // Initialize audio device once during construction
        player.setup_audio_device();
        player
    }

    fn setup_audio_device(&mut self) {
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(device) => device,
            None => {
                error!("No output device available");
                return;
            }
        };

        let sample_rate = self.target_sample_rate;
        let channels = self.target_channels;
        // Setup stream config
        let config = match device.supported_output_configs() {
            Ok(mut configs) => {
                match configs.find(|config| {
                    config.channels() == channels as u16
                        && config.min_sample_rate().0 <= sample_rate
                        && config.max_sample_rate().0 >= sample_rate
                        && match config.sample_format() {
                            cpal::SampleFormat::I16 => 16,
                            cpal::SampleFormat::U16 => 16,
                            cpal::SampleFormat::F32 => 32,
                            cpal::SampleFormat::I32 => 32,
                            cpal::SampleFormat::U32 => 32,
                            cpal::SampleFormat::I8 => 8,
                            cpal::SampleFormat::U8 => 8,
                            cpal::SampleFormat::I64 => 64,
                            cpal::SampleFormat::U64 => 64,
                            cpal::SampleFormat::F64 => 64,
                            _ => 0,
                        } >= self.target_bits_per_sample
                }) {
                    Some(config) => config.with_sample_rate(cpal::SampleRate(sample_rate)),
                    None => {
                        error!("No matching device config found");
                        return;
                    }
                }
            }
            Err(e) => {
                error!("Error getting device configs: {}", e);
                return;
            }
        };

        let matched_config_channels = config.channels(); // config.channels();
        let matched_config_sample_rate = config.sample_rate().0; // config.sample_rate().0;
        let matched_config_sample_format = config.sample_format();
        self.config = Some(config.into());
        self.device = Some(device);
        debug!(
            "AudioPlayer: Audio device initialized with target sample rate: {}, channels: {}, sample format: {}",
            matched_config_sample_rate, matched_config_channels, matched_config_sample_format
        );
    }

    fn load_samples(&mut self, samples: AudioPacket) {
        // Create new stream if none exists or if config changed
        if self.stream.is_none() {
            self.create_stream();
        }
        trace!("AudioPlayer: Loading {:?} samples", samples);
        match samples {
            AudioPacket::Samples {
                samples,
                sample_rate: _,
                channels: _,
            } => {
                let mut queue = self.sample_queue.lock().unwrap();
                for sample in samples {
                    queue.push_back(AudioSample::Sample(sample));
                }
            }
            AudioPacket::TrackHeader {
                id,
                play_immediately,
            } => {
                debug!("AudioPlayer: Received track header: {}", id);
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioSample::TrackHeader(id.clone()));

                let start_index = queue.len() - 1;
                drop(queue);

                self.cached_track_indices.lock().unwrap().insert(
                    id.clone(),
                    TrackIndex {
                        start: start_index,
                        end: None,
                    },
                );

                // Remove the oldest track from the cache if we have too many
                while self.cached_track_indices.lock().unwrap().len() > self.max_cached_tracks {
                    let mut sample_queue_unlocked = self.sample_queue.lock().unwrap();
                    let oldest_track_header = sample_queue_unlocked.front().cloned();

                    if let Some(AudioSample::TrackHeader(oldest_track_id)) = oldest_track_header {
                        let track_indices = self.cached_track_indices.lock().unwrap();
                        let oldest_track_info = track_indices.get(&oldest_track_id);

                        if let Some(info) = oldest_track_info {
                            if let Some(old_end_index) = info.end {
                                let current_player_position =
                                    self.current_track_position.load(Ordering::Relaxed);

                                // Evict oldest track from cache if it's not currently playing
                                if current_player_position > old_end_index {
                                    debug!(
                                        "AudioPlayer: Evicting oldest track from cache: {}",
                                        oldest_track_id
                                    );

                                    // Remove all samples from the queue until we hit the end of the track
                                    while let Some(sample) = sample_queue_unlocked.pop_front() {
                                        if let AudioSample::TrackFooter(_) = sample {
                                            break;
                                        }
                                    }
                                    drop(sample_queue_unlocked);
                                    drop(track_indices);

                                    // Remove track from index
                                    self.cached_track_indices
                                        .lock()
                                        .unwrap()
                                        .remove(&oldest_track_id);

                                    // Decrement all cached indices now that we've evicted an item
                                    for (_, track_index) in
                                        self.cached_track_indices.lock().unwrap().iter_mut()
                                    {
                                        if track_index.start > old_end_index {
                                            track_index.start =
                                                track_index.start - old_end_index - 1;
                                        }
                                        if track_index.end.is_some()
                                            && track_index.end.unwrap() > old_end_index
                                        {
                                            track_index.end =
                                                Some(track_index.end.unwrap() - old_end_index - 1);
                                        }
                                    }

                                    // Decrement the current track position, if needed
                                    if current_player_position > old_end_index {
                                        self.current_track_position
                                            .fetch_sub(old_end_index + 1, Ordering::Relaxed);
                                    }

                                    self.bus_sender
                                        .send(Message::Audio(AudioMessage::TrackEvictedFromCache(
                                            oldest_track_id.clone(),
                                        )))
                                        .unwrap();
                                } else {
                                    debug!("AudioPlayer: Unable to evict oldest track from cache because it's currently playing or ahead in queue. track_id={}", oldest_track_id);
                                    break;
                                }
                            } else {
                                debug!("AudioPlayer: Oldest track hasn't finished decoding yet, skipping eviction.");
                                break;
                            }
                        } else {
                            // Track not in index, but in queue? Just pop it to keep things moving.
                            sample_queue_unlocked.pop_front();
                        }
                    } else {
                        break;
                    }
                }

                if play_immediately {
                    self.current_track_id = id.clone();
                    // Use the index from the map in case evictions changed it
                    let start_index = self
                        .cached_track_indices
                        .lock()
                        .unwrap()
                        .get(&id)
                        .map(|idx| idx.start)
                        .unwrap_or(0);

                    self.current_track_position
                        .store(start_index, Ordering::Relaxed);
                    if let Some(stream) = &self.stream {
                        if let Err(e) = stream.play() {
                            error!("AudioPlayer: Failed to start playback: {}", e);
                        } else {
                            self.is_playing.store(true, Ordering::Relaxed);
                            debug!("AudioPlayer: Playback started");
                        }
                    } else {
                        debug!("No audio stream available to play");
                    }
                }
            }
            AudioPacket::TrackFooter { id } => {
                debug!("AudioPlayer: Received track footer: {}", id);
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioSample::TrackFooter(id.clone()));
                let footer_index = queue.len() - 1;
                drop(queue);

                let mut track_indices = self
                    .cached_track_indices
                    .lock()
                    .expect("Failed to lock track indices");
                if let Some(track_index) = track_indices.get_mut(&id) {
                    track_index.end = Some(footer_index);
                } else {
                    debug!(
                        "AudioPlayer: Received footer for unknown track {}, skipping index update",
                        id
                    );
                }
                self.bus_sender
                    .send(Message::Audio(AudioMessage::TrackCached(id.clone())))
                    .unwrap();

                // Only send ReadyForPlayback if we're NOT already playing this track
                // This prevents redundant PlayTrackById messages from the manager
                let already_playing =
                    self.current_track_id == id && self.is_playing.load(Ordering::Relaxed);
                if !already_playing {
                    self.bus_sender
                        .send(Message::Playback(PlaybackMessage::ReadyForPlayback(id)))
                        .unwrap();
                }
            }
        }
    }

    fn create_stream(&mut self) {
        let device = match &self.device {
            Some(device) => device,
            None => {
                error!("Cannot create stream: no audio device initialized");
                return;
            }
        };

        let config = match &self.config {
            Some(config) => config,
            None => {
                error!("Cannot create stream: no stream config set");
                return;
            }
        };

        // Clone our handles for the audio callback
        let sample_queue = self.sample_queue.clone();
        let bus_sender_clone = self.bus_sender.clone();
        let is_playing = self.is_playing.clone();
        let current_track_position = self.current_track_position.clone();
        // Build the output stream
        match device.build_output_stream(
            config,
            move |output_buffer: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if !is_playing.load(Ordering::Relaxed) {
                    output_buffer.fill(0.0);
                    return;
                }

                let mut sample_queue_unlocked = sample_queue.lock().unwrap();

                let mut input_current_position = current_track_position.load(Ordering::Relaxed);
                let mut output_current_position = 0;
                // Copy samples to output
                while output_current_position < output_buffer.len() {
                    let sample = &mut output_buffer[output_current_position];
                    *sample = if input_current_position < sample_queue_unlocked.len() {
                        let next_sample = &mut sample_queue_unlocked[input_current_position];
                        match next_sample {
                            AudioSample::Sample(buffered_sample) => {
                                input_current_position += 1;
                                *buffered_sample
                            }
                            AudioSample::TrackHeader(id) => {
                                bus_sender_clone
                                    .send(Message::Playback(PlaybackMessage::TrackStarted(
                                        id.clone(),
                                    )))
                                    .unwrap();
                                input_current_position += 1;
                                continue;
                            }
                            AudioSample::TrackFooter(id) => {
                                // Detected track footer, notify that track has finished and skip this sample
                                debug!("AudioPlayer: Sending track finished message: {}", id);
                                bus_sender_clone
                                    .send(Message::Playback(PlaybackMessage::TrackFinished(
                                        id.clone(),
                                    )))
                                    .unwrap();
                                input_current_position += 1;
                                continue;
                            }
                        }
                    } else {
                        0.0
                    };
                    output_current_position += 1;
                }

                current_track_position.store(input_current_position, Ordering::Relaxed);

                trace!(
                    "AudioPlayer: Copied {} samples to output",
                    output_buffer.len()
                );
            },
            |err| error!("Audio stream error: {}", err),
            None,
        ) {
            Ok(stream) => {
                self.stream = Some(stream);
                debug!("Audio stream created");
            }
            Err(e) => error!("Failed to build audio stream: {}", e),
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    Message::Audio(AudioMessage::AudioPacket(buffer)) => {
                        trace!("AudioPlayer: Received {:?} samples", buffer);
                        self.load_samples(buffer);
                    }
                    Message::Playback(PlaybackMessage::Play) => {
                        if let Some(stream) = &self.stream {
                            if let Err(e) = stream.play() {
                                error!("AudioPlayer: Failed to start playback: {}", e);
                            } else {
                                self.is_playing.store(true, Ordering::Relaxed);
                                debug!("AudioPlayer: Playback started");
                            }
                        } else {
                            debug!("No audio stream available to play");
                        }
                    }
                    Message::Playback(PlaybackMessage::Pause) => {
                        if let Some(stream) = &self.stream {
                            let _ = stream.pause();
                            self.is_playing.store(false, Ordering::Relaxed);
                            debug!("AudioPlayer: Playback paused");
                        }
                    }
                    Message::Playback(PlaybackMessage::Stop) => {
                        if let Some(stream) = &self.stream {
                            let _ = stream.pause();
                            self.is_playing.store(false, Ordering::Relaxed);
                            debug!("AudioPlayer: Playback stopped");
                        }
                    }
                    Message::Playback(PlaybackMessage::PlayTrackByIndex(_)) => {
                        // Handled by PlaylistManager which sends PlayTrackById
                    }
                    Message::Playback(PlaybackMessage::PlayTrackById(id)) => {
                        self.current_track_id = id.clone();
                        let start_index = self
                            .cached_track_indices
                            .lock()
                            .unwrap()
                            .get(&id)
                            .map(|idx| idx.start);

                        if let Some(current_track_index) = start_index {
                            self.current_track_position
                                .store(current_track_index, Ordering::Relaxed);
                            if let Some(stream) = &self.stream {
                                if let Err(e) = stream.play() {
                                    error!("AudioPlayer: Failed to start playback: {}", e);
                                } else {
                                    self.is_playing.store(true, Ordering::Relaxed);
                                    debug!("AudioPlayer: Playback started (manual)");
                                }
                            } else {
                                debug!("No audio stream available to play");
                            }
                        } else {
                            error!(
                                "AudioPlayer: Attempted to play track {} but it's not in cache",
                                id
                            );
                        }
                    }
                    Message::Playback(PlaybackMessage::ClearPlayerCache) => {
                        debug!("AudioPlayer: Clearing cache");
                        self.sample_queue.lock().unwrap().clear();
                        self.cached_track_indices.lock().unwrap().clear();
                        self.current_track_position.store(0, Ordering::Relaxed);
                        self.create_stream();
                        debug!("AudioPlayer: Cache cleared");
                    }
                    Message::Playback(PlaybackMessage::TrackStarted(id)) => {
                        debug!("AudioPlayer: Received track started command: {}", id);
                        self.current_track_id = id.clone();
                    }
                    Message::Playback(PlaybackMessage::TrackFinished(id)) => {
                        debug!("AudioPlayer: Received track finished command: {}", id);
                    }
                    Message::Config(ConfigMessage::ConfigChanged(config)) => {
                        debug!("AudioPlayer: Received config changed command: {:?}", config);
                        self.target_channels = config.output.channel_count;
                        self.target_sample_rate = config.output.sample_rate_khz;
                        self.target_bits_per_sample = config.output.bits_per_sample;
                        self.setup_audio_device();
                        self.create_stream();
                    }
                    _ => {} // Ignore other messages
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Ignore lag as we've increased the bus capacity
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!("AudioPlayer: bus closed");
                    break;
                }
            }
        }
    }
}
