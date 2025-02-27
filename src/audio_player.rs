use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, Receiver};
use log::{debug, error};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

#[derive(Clone)]
pub struct AudioPlayer {
    command_sender: Sender<PlayerCommand>,
    is_playing: Arc<AtomicBool>,
    buffer: Arc<Mutex<Vec<f32>>>,
    buffer_position: Arc<AtomicU64>,
    playback_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>
}

#[derive(Debug)]
pub enum PlayerCommand {
    LoadBuffer(Vec<f32>, u32, u16), // samples, sample_rate, channels
    AppendSamples(Vec<f32>),
    Play,
    Pause,
    Stop,
    Seek(usize),
    Exit,
}

impl AudioPlayer {
    pub fn new() -> Self {
        let (cmd_sender, cmd_receiver) = channel();
        let is_playing = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let buffer_position = Arc::new(AtomicU64::new(0));

        // Spawn audio playback thread
        let playback_thread = Self::spawn_playback_thread(
            cmd_receiver,
            is_playing.clone(),
            buffer.clone(),
            buffer_position.clone(),
        );

        AudioPlayer {
            command_sender: cmd_sender,
            is_playing,
            buffer,
            buffer_position,
            playback_thread: Arc::new(Mutex::new(Some(playback_thread)))
        }
    }

    fn spawn_playback_thread(
        cmd_receiver: Receiver<PlayerCommand>,
        is_playing: Arc<AtomicBool>,
        buffer: Arc<Mutex<Vec<f32>>>,
        buffer_position: Arc<AtomicU64>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let device = match host.default_output_device() {
                Some(device) => device,
                None => {
                    error!("No output device available");
                    return;
                }
            };

            let mut current_stream: Option<cpal::Stream> = None;
            let mut current_sample_rate = 44100;
            let mut current_channels = 2;

            let buffer_clone = buffer.clone();
            let buffer_position_clone = buffer_position.clone();
            loop{
                while let Ok(command) = cmd_receiver.recv() {
                    let buffer_clone = buffer_clone.clone();
                    let buffer_position_clone = buffer_position_clone.clone();
                    let is_playing_clone = is_playing.clone();
                    match command {
                        PlayerCommand::LoadBuffer(samples, sample_rate, channels) => {
                                            debug!("Loading buffer: {} samples, {}Hz, {} channels", 
                                                samples.len(), sample_rate, channels);
                            
                                            // Update buffer
                                            {
                                                let mut buffer_lock = buffer_clone.lock().unwrap();
                                                *buffer_lock = samples;
                                            }
                                            buffer_position_clone.store(0, Ordering::Relaxed);
                            
                                            // Update stream if sample rate or channels changed
                                            if sample_rate != current_sample_rate || channels != current_channels {
                                                current_sample_rate = sample_rate;
                                                current_channels = channels;
                                
                                                let config = match device.supported_output_configs() {
                                                    Ok(mut supported_configs) => {
                                                        let supported_config = supported_configs.find(|config| {
                                                            config.channels() == channels
                                                                && config.min_sample_rate().0 <= sample_rate
                                                                && config.max_sample_rate().0 >= sample_rate
                                                        });
                                                
                                                        match supported_config {
                                                            Some(config) => config.with_sample_rate(cpal::SampleRate(sample_rate)),
                                                            None => {
                                                                error!("No matching device config found for {}Hz {} channels", sample_rate, channels);
                                                                device.default_output_config().unwrap().into()
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        error!("Failed to get supported configs: {}", e);
                                                        device.default_output_config().unwrap().into()
                                                    }
                                                };
                                                
                                                let stream = match device.build_output_stream(
                                                    &config.config(),
                                                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                                        if !is_playing_clone.load(Ordering::Relaxed) {
                                                            data.fill(0.0);
                                                            return;
                                                        }
                                                    
                                                        let buffer_lock = buffer_clone.lock().unwrap();
                                                        let mut pos = buffer_position_clone.load(Ordering::Relaxed) as usize;
                                                    
                                                        // Only stop if we've consumed all samples
                                                        if pos >= buffer_lock.len() {
                                                            debug!("Reached end of track at pos={}", pos);
                                                            is_playing_clone.store(false, Ordering::Relaxed);
                                                            data.fill(0.0);
                                                            return;
                                                        }
                                                    
                                                        // Copy available samples
                                                        for (i, sample) in data.iter_mut().enumerate() {
                                                            *sample = if pos + i < buffer_lock.len() {
                                                                buffer_lock[pos + i]
                                                            } else {
                                                                0.0
                                                            };
                                                        }
                                                    
                                                        buffer_position_clone.store((pos + data.len()) as u64, Ordering::Relaxed);
                                                    },
                                                    |err| error!("Audio stream error: {}", err),
                                                    None
                                                ) {
                                                    Ok(stream) => stream,
                                                    Err(e) => {
                                                        error!("Failed to build stream: {}", e);
                                                        continue;
                                                    }
                                                };

                                                if let Err(e) = stream.play() {
                                                    error!("Failed to play stream: {}", e);
                                                    continue;
                                                }

                                                current_stream = Some(stream);
                                            }
                                        }
                        PlayerCommand::Play => {
                                            is_playing.store(true, Ordering::Relaxed);
                                        }
                        PlayerCommand::Pause => {
                                            is_playing.store(false, Ordering::Relaxed);
                                        }
                        PlayerCommand::Stop => {
                                            is_playing.store(false, Ordering::Relaxed);
                                            buffer_position.store(0, Ordering::Relaxed);
                                        }
                        PlayerCommand::Seek(position) => {
                                            buffer_position.store(position as u64, Ordering::Relaxed);
                                        }
                        PlayerCommand::Exit => break,
                        PlayerCommand::AppendSamples(items) => {
                            let mut buffer_lock = buffer_clone.lock().unwrap();
                            buffer_lock.extend_from_slice(&items);
                            // debug!("Appended {} samples, buffer size now {}", items.len(), buffer_lock.len());
                        },
                    }
                }

                debug!("Out of the while loop, playback thread going down fast");
        }
        })
    }

    pub fn load_samples(&self, samples: Vec<f32>, sample_rate: u32, channels: u16) {
        self.command_sender
            .send(PlayerCommand::LoadBuffer(samples, sample_rate, channels))
            .unwrap_or_else(|e| error!("Failed to send load buffer command: {}", e));
    }

    pub fn play(&self) {
        self.command_sender
            .send(PlayerCommand::Play)
            .unwrap_or_else(|e| error!("Failed to send play command: {}", e));
    }

    pub fn pause(&self) {
        self.command_sender
            .send(PlayerCommand::Pause)
            .unwrap_or_else(|e| error!("Failed to send pause command: {}", e));
    }

    pub fn stop(&self) {
        self.command_sender
            .send(PlayerCommand::Stop)
            .unwrap_or_else(|e| error!("Failed to send stop command: {}", e));
    }

    pub fn seek(&self, position: usize) {
        self.command_sender
            .send(PlayerCommand::Seek(position))
            .unwrap_or_else(|e| error!("Failed to send seek command: {}", e));
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::Relaxed)
    }

    pub fn append_samples(&self, samples: Vec<f32>) {
        self.command_sender
            .send(PlayerCommand::AppendSamples(samples))
            .unwrap_or_else(|e| error!("Failed to send append samples command: {}", e));
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.command_sender.send(PlayerCommand::Exit);
    }
}