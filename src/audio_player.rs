use crate::protocol::{AudioMessage, AudioPacket, ConfigMessage, Message, PlaybackMessage, TrackStarted};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, error};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tokio::sync::broadcast::{Receiver, Sender};

#[derive(Debug, Clone)]
pub struct TrackHeader {
    pub id: String,
    pub start_offset_ms: u64,
}

#[derive(Debug, Clone)]
enum AudioSample {
    Sample(f32),
    TrackHeader(TrackHeader),
    TrackFooter(String),
}

#[derive(Debug, Clone)]
struct TrackIndex {
    start: usize,
    end: Option<usize>,
    technical_metadata: crate::protocol::TechnicalMetadata,
}

pub struct AudioPlayer {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    // Audio state
    target_sample_rate: Arc<AtomicUsize>,
    target_channels: Arc<AtomicUsize>,
    target_bits_per_sample: u16,
    sample_queue: Arc<Mutex<VecDeque<AudioSample>>>,
    is_playing: Arc<AtomicBool>,
    current_track_id: Arc<Mutex<String>>,
    current_track_position: Arc<AtomicUsize>,
    current_track_offset_ms: Arc<AtomicUsize>,
    current_metadata: Arc<Mutex<Option<crate::protocol::TechnicalMetadata>>>,

    // Setup cache
    cached_track_indices: Arc<Mutex<HashMap<String, TrackIndex>>>,

    // Audio stream
    config: Option<cpal::StreamConfig>,
    device: Option<cpal::Device>,
    stream: Option<cpal::Stream>,
}

impl AudioPlayer {
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        let is_playing = Arc::new(AtomicBool::new(false));
        let current_track_position = Arc::new(AtomicUsize::new(0));
        let cached_track_indices = Arc::new(Mutex::new(HashMap::new()));
        let current_track_id = Arc::new(Mutex::new(String::new()));
        let current_metadata = Arc::new(Mutex::new(None));
        let current_track_offset_ms = Arc::new(AtomicUsize::new(0));
        let target_sample_rate = Arc::new(AtomicUsize::new(44100));
        let target_channels = Arc::new(AtomicUsize::new(2));

        let mut player = Self {
            bus_receiver,
            bus_sender: bus_sender.clone(),
            sample_queue: Arc::new(Mutex::new(VecDeque::new())),
            cached_track_indices: cached_track_indices.clone(),
            is_playing: is_playing.clone(),
            device: None,
            config: None,
            stream: None,
            target_sample_rate: target_sample_rate.clone(),
            target_channels: target_channels.clone(),
            target_bits_per_sample: 0,
            current_track_id: current_track_id.clone(),
            current_track_position: current_track_position.clone(),
            current_track_offset_ms: current_track_offset_ms.clone(),
            current_metadata: current_metadata.clone(),
        };

        player.setup_audio_device();

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

                    let elapsed_samples = if current_pos > start_pos {
                        current_pos - start_pos
                    } else {
                        0
                    };
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

        let sample_rate = self.target_sample_rate.load(Ordering::Relaxed) as u32;
        let channels = self.target_channels.load(Ordering::Relaxed) as u16;

        let config = match device.supported_output_configs() {
            Ok(mut configs) => match configs.find(|config| {
                (channels == 0 || config.channels() == channels)
                    && (sample_rate == 0
                        || (config.min_sample_rate().0 <= sample_rate
                            && config.max_sample_rate().0 >= sample_rate))
            }) {
                Some(config) => {
                    let sr = if sample_rate == 0 {
                        config.max_sample_rate().0
                    } else {
                        sample_rate
                    };
                    config.with_sample_rate(cpal::SampleRate(sr))
                }
                None => {
                    error!("No matching device config found");
                    return;
                }
            },
            Err(e) => {
                error!("Error getting device configs: {}", e);
                return;
            }
        };

        self.target_channels
            .store(config.channels() as usize, Ordering::Relaxed);
        self.target_sample_rate
            .store(config.sample_rate().0 as usize, Ordering::Relaxed);

        let actual_sample_rate = config.sample_rate().0;
        let actual_channels = config.channels();

        self.config = Some(config.into());
        self.device = Some(device);
        debug!(
            "AudioPlayer: Audio device initialized: sr={}, channels={}",
            self.target_sample_rate.load(Ordering::Relaxed),
            self.target_channels.load(Ordering::Relaxed)
        );

        let _ = self.bus_sender.send(crate::protocol::Message::Config(
            crate::protocol::ConfigMessage::AudioDeviceOpened {
                sample_rate: actual_sample_rate,
                channels: actual_channels,
            },
        ));
    }

    fn create_stream(&mut self) {
        if self.stream.is_some() {
            return;
        }

        let device = self.device.as_ref().unwrap();
        let config = self.config.as_ref().unwrap();

        let sample_queue = self.sample_queue.clone();
        let bus_sender_clone = self.bus_sender.clone();
        let is_playing = self.is_playing.clone();
        let current_track_position = self.current_track_position.clone();

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

                while output_current_position < output_buffer.len() {
                    let sample = &mut output_buffer[output_current_position];
                    *sample = if input_current_position < sample_queue_unlocked.len() {
                        let next_sample = &mut sample_queue_unlocked[input_current_position];
                        match next_sample {
                            AudioSample::Sample(s) => {
                                input_current_position += 1;
                                *s
                            }
                            AudioSample::TrackHeader(TrackHeader { id, start_offset_ms }) => {
                                let _ = bus_sender_clone.send(Message::Playback(
                                    PlaybackMessage::TrackStarted(TrackStarted {
                                        id: id.clone(),
                                        start_offset_ms: *start_offset_ms,
                                    }),
                                ));
                                input_current_position += 1;
                                continue;
                            }
                            AudioSample::TrackFooter(id) => {
                                let _ = bus_sender_clone.send(Message::Playback(
                                    PlaybackMessage::TrackFinished(id.clone()),
                                ));
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
            },
            |err| error!("Audio stream error: {}", err),
            None,
        ) {
            Ok(stream) => {
                let _ = stream.play();
                self.stream = Some(stream);
                debug!("Audio stream created");
            }
            Err(e) => error!("Failed to build audio stream: {}", e),
        }
    }

    fn load_samples(&mut self, samples: AudioPacket) {
        if self.stream.is_none() {
            self.create_stream();
        }

        match samples {
            AudioPacket::Samples { samples, .. } => {
                let mut queue = self.sample_queue.lock().unwrap();
                for s in samples {
                    queue.push_back(AudioSample::Sample(s));
                }
            }
            AudioPacket::TrackHeader {
                id,
                play_immediately,
                technical_metadata,
                start_offset_ms,
            } => {
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioSample::TrackHeader(TrackHeader{id: id.clone(), start_offset_ms: start_offset_ms}));
                let start_index = queue.len() - 1;
                drop(queue);

                // debug!("AudioPlayer: Loaded track header: id {} technical metadata {:?}", id, technical_metadata);
                self.cached_track_indices.lock().unwrap().insert(
                    id.clone(),
                    TrackIndex {
                        start: start_index,
                        end: None,
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
                    self.is_playing.store(true, Ordering::Relaxed);
                    let _ = self.bus_sender.send(Message::Playback(
                        PlaybackMessage::TechnicalMetadataChanged(technical_metadata),
                    ));
                }
            }
            AudioPacket::TrackFooter { id } => {
                let mut queue = self.sample_queue.lock().unwrap();
                queue.push_back(AudioSample::TrackFooter(id.clone()));
                let footer_index = queue.len() - 1;
                drop(queue);

                if let Some(info) = self.cached_track_indices.lock().unwrap().get_mut(&id) {
                    info.end = Some(footer_index);
                }

                self.bus_sender
                    .send(Message::Audio(AudioMessage::TrackCached(id.clone())))
                    .unwrap();

                let is_current = *self.current_track_id.lock().unwrap() == id;
                if !is_current || !self.is_playing.load(Ordering::Relaxed) {
                    self.bus_sender
                        .send(Message::Playback(PlaybackMessage::ReadyForPlayback(id)))
                        .unwrap();
                }
            }
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    Message::Audio(AudioMessage::AudioPacket(buffer)) => {
                        self.load_samples(buffer);
                    }
                    Message::Playback(PlaybackMessage::Play) => {
                        self.is_playing.store(true, Ordering::Relaxed);
                        debug!("AudioPlayer: Playback resumed");
                    }
                    Message::Playback(PlaybackMessage::Pause) => {
                        self.is_playing.store(false, Ordering::Relaxed);
                        debug!("AudioPlayer: Playback paused");
                    }
                    Message::Playback(PlaybackMessage::Stop) => {
                        self.is_playing.store(false, Ordering::Relaxed);
                        debug!("AudioPlayer: Playback stopped");
                    }
                    Message::Playback(PlaybackMessage::PlayTrackById(id)) => {
                        let track_info =
                            self.cached_track_indices.lock().unwrap().get(&id).cloned();
                        if let Some(info) = track_info {
                            *self.current_track_id.lock().unwrap() = id;
                            *self.current_metadata.lock().unwrap() =
                                Some(info.technical_metadata.clone());
                            self.current_track_position
                                .store(info.start, Ordering::Relaxed);
                            self.current_track_offset_ms
                                .store(0, Ordering::Relaxed);
                            self.is_playing.store(true, Ordering::Relaxed);
                            debug!("AudioPlayer: Playback started (manual)");
                            let _ = self.bus_sender.send(Message::Playback(
                                PlaybackMessage::TechnicalMetadataChanged(info.technical_metadata),
                            ));
                        }
                    }
                    Message::Playback(PlaybackMessage::ClearPlayerCache) => {
                        self.is_playing.store(false, Ordering::Relaxed);
                        self.sample_queue.lock().unwrap().clear();
                        self.cached_track_indices.lock().unwrap().clear();
                        self.current_track_position.store(0, Ordering::Relaxed);
                        *self.current_metadata.lock().unwrap() = None;
                        debug!("AudioPlayer: Cache cleared");
                    }
                    Message::Config(ConfigMessage::ConfigChanged(config)) => {
                        self.target_sample_rate
                            .store(config.output.sample_rate_khz as usize, Ordering::Relaxed);
                        self.target_channels
                            .store(config.output.channel_count as usize, Ordering::Relaxed);
                        self.target_bits_per_sample = config.output.bits_per_sample;
                        self.setup_audio_device();
                        if self.stream.is_some() {
                            self.stream = None;
                            self.create_stream();
                        }
                    }
                    Message::Playback(PlaybackMessage::TrackStarted(track_started)) => {
                        debug!("AudioPlayer: Track started: {}", track_started.id);
                        // Handle the case when a track automatically starts playing (not caused by a user action)
                        self.current_track_id.lock().unwrap().clone_from(&track_started.id);
                        self.current_track_offset_ms.store(track_started.start_offset_ms as usize, Ordering::Relaxed);
                        let track_info =
                            self.cached_track_indices.lock().unwrap().get(&track_started.id).cloned();
                        if let Some(info) = track_info {
                            let _ = self.bus_sender.send(Message::Playback(
                                    PlaybackMessage::TechnicalMetadataChanged(info.technical_metadata.clone()),
                                ));
                            *self.current_metadata.lock().unwrap() = Some(info.technical_metadata);
                        }
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
