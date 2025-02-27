use std::path::PathBuf;
use std::sync::mpsc::{Sender, Receiver};
use log::{debug, error, info};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use crate::audio_player::AudioPlayer;

pub struct AudioDecoder {
    command_receiver: Receiver<Command>,
    event_sender: Option<Sender<Event>>,
    player: AudioPlayer,
    decode_handle: Option<std::thread::JoinHandle<()>>
}

pub enum Command {
    Load(PathBuf),
    Play(PathBuf),
    Pause,
    Resume,
    Stop,
    Seek(u64), // milliseconds
    Exit,
}

pub enum Event {
    TrackLoaded {
        duration_ms: u64,
        sample_rate: u32,
        channels: u16,
    },
    TrackFinished,
    PositionChanged(u64), // milliseconds
    PlaybackPaused,
    PlaybackResumed,
    PlaybackStopped,
    Error(String),
}

impl AudioDecoder {
    pub fn new(command_receiver: Receiver<Command>) -> Self {
        AudioDecoder {
            command_receiver,
            event_sender: None,
            player: AudioPlayer::new(),
            decode_handle: None,
        }
    }

    pub fn set_event_sender(&mut self, sender: Sender<Event>) {
        self.event_sender = Some(sender);
    }

    pub fn run(&mut self) {
        while let Ok(command) = self.command_receiver.recv() {
            match command {
                Command::Load(path) => self.load_file(&path),
                Command::Play(path) => {
                    self.load_file(&path);
                    self.player.play();
                }
                Command::Pause => {
                    self.player.pause();
                    self.send_event(Event::PlaybackPaused);
                }
                Command::Resume => {
                    self.player.play();
                    self.send_event(Event::PlaybackResumed);
                }
                Command::Stop => {
                    self.player.stop();
                    self.send_event(Event::PlaybackStopped);
                }
                Command::Seek(ms) => self.seek_to_ms(ms),
                Command::Exit => break,
            }
        }
    }

    fn load_file(&mut self, path: &PathBuf) {
        info!("Loading file: {}", path.display());
    
        // Open the media source
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                self.send_event(Event::Error(format!("Failed to open file: {}", e)));
                return;
            }
        };
    
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
    
        // Create probe hint from file extension
        let mut hint = Hint::new();
        if let Some(extension) = path.extension() {
            if let Some(ext_str) = extension.to_str() {
                hint.with_extension(ext_str);
            }
        }
    
        let probed = match symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        {
            Ok(probed) => probed,
            Err(e) => {
                self.send_event(Event::Error(format!("Failed to probe media format: {}", e)));
                return;
            }
        };
    
        let mut format = probed.format;
        let track = match format.default_track() {
            Some(track) => track,
            None => {
                self.send_event(Event::Error("No default track found".to_string()));
                return;
            }
        };
    
        let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.unwrap().count() as u16;

    debug!("Track info: sample_rate={}, channels={}", sample_rate, channels);

    // Get the decoder
    let mut decoder = match symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
    {
        Ok(decoder) => decoder,
        Err(e) => {
            self.send_event(Event::Error(format!("Failed to create decoder: {}", e)));
            return;
        }
    };

    // Calculate duration if available
    let duration_ms = track.codec_params.time_base
        .map(|tb| track.codec_params.n_frames.unwrap_or(0) * 1000 / tb.denom as u64)
        .unwrap_or(0);

    // Notify about track properties
    self.send_event(Event::TrackLoaded {
        duration_ms,
        sample_rate,
        channels: channels as u16,
    });

    // Decode initial buffer (first few seconds)
    let mut decoded_samples = Vec::new();
    let initial_samples = sample_rate as usize * channels as usize * 5; // 5 seconds worth of audio

    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = decoded.spec();
                let duration = decoded.capacity() as u64;
                
                let mut sample_buffer = SampleBuffer::<f32>::new(duration, *spec);
                sample_buffer.copy_interleaved_ref(decoded);
                
                decoded_samples.extend_from_slice(sample_buffer.samples());

                if decoded_samples.len() >= initial_samples {
                    break;
                }
            }
            Err(e) => {
                debug!("Decode error: {}", e);
                break;
            }
        }
    }

    // Load initial samples into player
    self.player.load_samples(decoded_samples, sample_rate, channels as u16);

    // Start background decoding thread
    let player = self.player.clone();
    let format = std::sync::Arc::new(std::sync::Mutex::new(format));
    let decoder = std::sync::Arc::new(std::sync::Mutex::new(decoder));
    let track_id = track_id.clone();

    self.decode_handle = Some(std::thread::spawn(move || {
        let mut format = format.lock().unwrap();
        let mut decoder = decoder.lock().unwrap();
        let mut buffer = Vec::new();
        let chunk_size = sample_rate as usize * channels as usize; // 1 second of audio

        debug!("Starting background decode loop");
        let mut total_samples_sent = 0;

        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = decoded.spec();
                    let duration = decoded.capacity() as u64;
                    
                    let mut sample_buffer = SampleBuffer::<f32>::new(duration, *spec);
                    sample_buffer.copy_interleaved_ref(decoded);
                    
                    buffer.extend_from_slice(sample_buffer.samples());
                    
                    // Send chunks of samples to the player
                    if buffer.len() >= chunk_size {
                        debug!("Sending chunk of {} samples. Total sent so far: {}", buffer.len(), total_samples_sent);
                        player.append_samples(buffer.clone());
                        total_samples_sent += buffer.len();
                        buffer.clear();
                    }
                }
                Err(e) => {
                    error!("Decode error in background thread: {}", e);
                    break;
                }
            }
        }

        // Send any remaining samples
        if !buffer.is_empty() {
            debug!("Sending final chunk of {} samples. Total sent: {}", buffer.len(), total_samples_sent + buffer.len());
            player.append_samples(buffer);
        }

        debug!("Background decode loop finished");
    }));
    }

    fn seek_to_ms(&mut self, position_ms: u64) {
        // Convert milliseconds to samples
        // This is a simplified version - in reality you'd want to account for
        // the actual sample rate and number of channels
        let sample_position = (position_ms as f64 * 44100.0 / 1000.0) as usize * 2;
        self.player.seek(sample_position);
        self.send_event(Event::PositionChanged(position_ms));
    }

    fn send_event(&self, event: Event) {
        if let Some(sender) = &self.event_sender {
            if let Err(e) = sender.send(event) {
                error!("Failed to send event: {}", e);
            }
        }
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        // AudioPlayer will be dropped automatically
    }
}