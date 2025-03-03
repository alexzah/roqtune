use crate::protocol::{AudioMessage, AudioPacket, Message, PlaybackMessage};
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
            resampler: None,
            current_track_id: String::new(),
            current_track_position: Arc::new(AtomicUsize::new(0)),
            max_cached_tracks: 2,
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

        let sample_rate = 48000u32;
        let channels = 2usize;
        // Setup stream config
        let config = match device.supported_output_configs() {
            Ok(mut configs) => {
                match configs.find(|config| {
                    config.channels() == channels as u16
                        && config.min_sample_rate().0 <= sample_rate
                        && config.max_sample_rate().0 >= sample_rate
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

        self.target_channels = config.channels(); // config.channels();
        self.target_sample_rate = config.sample_rate().0; // config.sample_rate().0;
        self.config = Some(config.into());
        self.device = Some(device);
        debug!(
            "AudioPlayer: Audio device initialized with target sample rate: {} and channels: {}",
            self.target_sample_rate, self.target_channels
        );
    }

    fn create_resampler(&mut self, source_sample_rate: u32, chunk_size: usize) -> FftFixedIn<f32> {
        // let params = SincInterpolationParameters {
        //     sinc_len: 256,
        //     f_cutoff: 0.95,
        //     interpolation: SincInterpolationType::Nearest,
        //     oversampling_factor: 128,
        //     window: WindowFunction::BlackmanHarris2,
        // };
        let resampler = FftFixedIn::<f32>::new(
            source_sample_rate as usize,
            self.target_sample_rate as usize,
            chunk_size,
            4,
            self.target_channels as usize,
        );
        resampler.unwrap()
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
                sample_rate,
                channels,
            } => {
                for sample in samples {
                    self.sample_queue
                        .lock()
                        .unwrap()
                        .push_back(AudioSample::Sample(sample));
                }
            }
            AudioPacket::TrackHeader {
                id,
                play_immediately,
            } => {
                debug!("AudioPlayer: Received track header: {}", id);
                self.sample_queue
                    .lock()
                    .unwrap()
                    .push_back(AudioSample::TrackHeader(id.clone()));

                let start_index = self.sample_queue.lock().unwrap().len() - 1;
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
                    let oldest_track_header = sample_queue_unlocked.front().unwrap().clone();
                    if let AudioSample::TrackHeader(oldest_track_id) = oldest_track_header {
                        let old_end_index = self.cached_track_indices.lock().unwrap().get(&oldest_track_id).expect(&format!("Track id {} not found in index", oldest_track_id)).end.expect("Track end index not found");
                        let current_player_position = self.current_track_position.load(Ordering::Relaxed);

                        // Evict oldest track from cache if it's not currently playing
                        if current_player_position > old_end_index {
                            debug!("AudioPlayer: Evicting oldest track from cache: {}", oldest_track_id);

                            // Remove track from index
                            self.cached_track_indices.lock().unwrap().remove(&oldest_track_id);

                            // Remove all samples from the queue until we hit the end of the track
                            while let Some(sample) = sample_queue_unlocked.pop_front() {
                                if let AudioSample::TrackFooter(_) = sample {
                                    break;
                                }
                            }

                            // Decrement all cached indices now that we've evicted an item
                            for (_, track_index) in self.cached_track_indices.lock().unwrap().iter_mut() {
                                if track_index.start > old_end_index {
                                    track_index.start  = track_index.start - old_end_index -1;
                                }
                                if track_index.end.is_some() && track_index.end.unwrap() > old_end_index {
                                    track_index.end = Some(track_index.end.unwrap() - old_end_index - 1);
                                }
                            }

                            // Decrement the current track position, if needed
                            if current_player_position > old_end_index {
                                self.current_track_position.fetch_sub(old_end_index + 1, Ordering::Relaxed);
                            }

                            self.bus_sender.send(Message::Audio(AudioMessage::TrackEvictedFromCache(oldest_track_id.clone()))).unwrap();
                        } else {
                            debug!("AudioPlayer: Unable to evict oldest track from cache because it's currently playing. track_id={}", oldest_track_id);
                            break;
                        }
                    }
                }

                if play_immediately {
                    self.current_track_id = id.clone();
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
                self.sample_queue
                    .lock()
                    .unwrap()
                    .push_back(AudioSample::TrackFooter(id.clone()));
                let mut track_indices = self
                    .cached_track_indices
                    .lock()
                    .expect("Failed to lock track indices");
                let _ = track_indices
                    .get_mut(&id)
                    .expect(&format!("Track id {} not found in index", id))
                    .end
                    .insert(self.sample_queue.lock().unwrap().len() - 1);
                self.bus_sender
                    .send(Message::Audio(AudioMessage::TrackCached(id)))
                    .unwrap();
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
            while let Ok(message) = self.bus_receiver.blocking_recv() {
                match message {
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
                    Message::Playback(PlaybackMessage::PlayTrackByIndex(_)) => {
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
                    Message::Playback(PlaybackMessage::PlayTrackById(id)) => {
                        self.current_track_id = id.clone();
                        let current_track_index =
                            self.cached_track_indices.lock().unwrap().get(&id).unwrap().start;
                        self.current_track_position
                            .store(current_track_index, Ordering::Relaxed);
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
                    Message::Playback(PlaybackMessage::Stop) => {
                        if let Some(stream) = &self.stream {
                            stream.pause().unwrap();
                            self.is_playing.store(false, Ordering::Relaxed);
                        }
                    }
                    Message::Playback(PlaybackMessage::ClearPlayerCache) => {
                        debug!("AudioPlayer: Clearing cache");
                        self.sample_queue.lock().unwrap().clear();
                        self.create_stream();

                        debug!("AudioPlayer: Ready for playback");
                        self.bus_sender
                            .send(Message::Playback(PlaybackMessage::ReadyForPlayback))
                            .unwrap();
                    }
                    Message::Playback(PlaybackMessage::TrackStarted(id)) => {
                        debug!("AudioPlayer: Received track started command: {}", id);
                        self.current_track_id = id.clone();
                    }
                    Message::Playback(PlaybackMessage::TrackFinished(id)) => {
                        debug!("AudioPlayer: Received track finished command: {}", id);
                    }
                    _ => {} // Ignore other messages
                }
            }
            error!("AudioPlayer: receiver error, restarting loop");
        }
    }
}
