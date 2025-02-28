use crate::protocol::{AudioMessage, Message, PlaybackMessage};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, error};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::broadcast::{Receiver, Sender};

pub struct AudioPlayer {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    // Audio state
    buffer: Arc<Mutex<Vec<f32>>>,
    buffer_position: Arc<AtomicU64>,
    is_playing: Arc<AtomicBool>,
    // Audio stream
    stream: Option<cpal::Stream>,
}

impl AudioPlayer {
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        Self {
            bus_receiver,
            bus_sender,
            buffer: Arc::new(Mutex::new(Vec::new())),
            buffer_position: Arc::new(AtomicU64::new(0)),
            is_playing: Arc::new(AtomicBool::new(false)),
            stream: None,
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
                        debug!(
                            "Received {} samples at {}Hz ({} channels)",
                            samples.len(),
                            sample_rate,
                            channels
                        );
                        self.add_samples(samples, sample_rate, channels);
                    }
                    Message::Playback(PlaybackMessage::Play) => {
                        self.is_playing.store(true, Ordering::Relaxed);
                    }
                    _ => {} // Ignore other messages
                }
            }
            error!("AudioPlayer: receiver error, restarting loop");
        }
    }

    // Add a new method for starting playback
    fn start_playback(&mut self) {
        if let Some(stream) = &self.stream {
            if let Err(e) = stream.play() {
                error!("Failed to start playback: {}", e);
                return;
            }
            self.is_playing.store(true, Ordering::Relaxed);
            debug!("Audio playback started");
        } else {
            debug!("No audio stream available to play");
        }
    }

    fn add_samples(&mut self, samples: Vec<f32>, sample_rate: u32, channels: u16) {
        // Store the samples in our buffer
        {
            let mut buffer = self.buffer.lock().unwrap();
            *buffer = samples;
        }
        self.buffer_position.store(0, Ordering::Relaxed);

        // Get default audio device
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(device) => device,
            None => {
                error!("No output device available");
                return;
            }
        };

        // Setup stream config
        let config = match device.supported_output_configs() {
            Ok(mut configs) => {
                match configs.find(|config| {
                    config.channels() == channels
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

        // Clone our handles for the audio callback
        let buffer = self.buffer.clone();
        let buffer_position = self.buffer_position.clone();
        let is_playing = self.is_playing.clone();
        let bus_sender = self.bus_sender.clone();

        // Build the output stream
        let stream = match device.build_output_stream(
            &config.into(),
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
                    is_playing.store(false, Ordering::Relaxed);
                    let _ = bus_sender.send(Message::Playback(PlaybackMessage::TrackFinished));
                    return;
                }

                // Copy samples to output
                for (i, sample) in data.iter_mut().enumerate() {
                    *sample = if pos + i < buffer_lock.len() {
                        buffer_lock[pos + i]
                    } else {
                        0.0
                    };
                }

                buffer_position.store((pos + data.len()) as u64, Ordering::Relaxed);
            },
            |err| error!("Audio stream error: {}", err),
            None,
        ) {
            Ok(stream) => stream,
            Err(e) => {
                error!("Failed to build audio stream: {}", e);
                return;
            }
        };

        // Start playback
        if let Err(e) = stream.play() {
            error!("Failed to start playback: {}", e);
            return;
        }

        // Store the stream
        self.stream = Some(stream);
        self.is_playing.store(true, Ordering::Relaxed);
        debug!("Audio playback started");
    }
}
