use crate::protocol::{AudioMessage, Message, PlaybackMessage};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, error, trace};
use rubato::{
    FftFixedIn, Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use symphonia::core::{codecs::Decoder, formats::Packet};
use tokio::sync::broadcast::{Receiver, Sender};

pub struct Sample {
    sample: f32,
    sample_rate: u32,
    channels: u16,
}

pub struct AudioPlayer {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    // Audio state
    target_sample_rate: u32,
    target_channels: u16,
    sample_queue: Arc<Mutex<VecDeque<Sample>>>,
    buffer_position: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
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
            buffer_position: Arc::new(AtomicU64::new(0)),
            is_playing: Arc::new(AtomicBool::new(false)),
            device: None,
            config: None,
            stream: None,
            target_sample_rate: 0,
            target_channels: 0,
            resampler: None,
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

    fn load_samples(&mut self, samples: Vec<f32>, sample_rate: u32, channels: u16) {
        // Create new stream if none exists or if config changed
        if self.stream.is_none() {
            self.create_stream();
        }
        trace!("AudioPlayer: Loading {} samples", samples.len());
        let mut queue = self.sample_queue.lock().unwrap();
        for sample in samples {
            queue.push_back(Sample {
                sample,
                sample_rate,
                channels,
            });
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
        let buffer = self.sample_queue.clone();
        let buffer_position = self.buffer_position.clone();
        let is_playing = self.is_playing.clone();
        let bus_sender = self.bus_sender.clone();

        // Build the output stream
        match device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if !is_playing.load(Ordering::Relaxed) {
                    data.fill(0.0);
                    return;
                }

                let buffer_lock = buffer.lock().unwrap();
                let pos = buffer_position.load(Ordering::Relaxed) as usize;

                // Check if we've reached the end
                if pos >= buffer_lock.len() {
                    data.fill(0.0);
                    let _ = bus_sender.send(Message::Playback(PlaybackMessage::TrackFinished));
                    return;
                }

                // Copy samples to output
                for (i, sample) in data.iter_mut().enumerate() {
                    *sample = if pos + i < buffer_lock.len() {
                        buffer_lock[pos + i].sample
                    } else {
                        0.0
                    };
                }

                trace!("AudioPlayer: Copied {} samples to output", data.len());
                buffer_position.store((pos + data.len()) as u64, Ordering::Relaxed);
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
                    Message::Audio(AudioMessage::BufferReady {
                        samples,
                        sample_rate,
                        channels,
                    }) => {
                        trace!("AudioPlayer: Received {} samples", samples.len());
                        self.load_samples(samples, sample_rate, channels);
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
                    Message::Playback(PlaybackMessage::Stop) => {
                        if let Some(stream) = &self.stream {
                            stream.pause().unwrap();
                            self.buffer_position.store(0, Ordering::Relaxed);
                            self.is_playing.store(false, Ordering::Relaxed);
                        }
                    }
                    Message::Playback(PlaybackMessage::ClearPlayerCache) => {
                        debug!("AudioPlayer: Clearing cache");
                        self.buffer_position.store(0, Ordering::Relaxed);
                        self.sample_queue.lock().unwrap().clear();
                        self.bus_sender
                            .send(Message::Playback(PlaybackMessage::ReadyForPlayback))
                            .unwrap();
                    }
                    _ => {} // Ignore other messages
                }
            }
            error!("AudioPlayer: receiver error, restarting loop");
        }
    }
}
