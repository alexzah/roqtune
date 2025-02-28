use crate::protocol::{AudioMessage, Message, PlaylistMessage};
use log::{debug, error};
use std::path::PathBuf;
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::broadcast::{Receiver, Sender};

pub struct AudioDecoder {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
}

impl AudioDecoder {
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        Self {
            bus_receiver,
            bus_sender,
        }
    }

    pub fn run(&mut self) {
        loop {
            while let Ok(message) = self.bus_receiver.blocking_recv() {
                match message {
                    Message::Playlist(PlaylistMessage::LoadTrack(path)) => {
                        debug!("AudioDecoder: Loading track {:?}", path);
                        self.load_file(&path);
                    }
                    _ => {} // Ignore other messages for now
                }
            }
            error!("AudioDecoder: receiver error, restarting loop");
        }
    }

    fn load_file(&mut self, path: &PathBuf) {
        // Open the media source
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) => {
                error!("Failed to open file: {}", e);
                return;
            }
        };

        // Create the media source stream
        let media_source = MediaSourceStream::new(Box::new(file), Default::default());

        // Create a probe hint using the file extension
        let hint = Hint::new();

        // Probe the media source
        let format_reader = match symphonia::default::get_probe().format(
            &hint,
            media_source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(probed) => probed,
            Err(e) => {
                error!("Failed to probe media source: {}", e);
                return;
            }
        };

        // Get the default track
        let track = match format_reader.format.default_track() {
            Some(track) => track,
            None => {
                error!("No default track found");
                return;
            }
        };

        let track_id = track.id;
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let channels = track.codec_params.channels.unwrap().count();

        debug!(
            "Track info: sample_rate={}, channels={}",
            sample_rate, channels
        );

        // Create a decoder for the track
        let mut decoder = match symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
        {
            Ok(decoder) => decoder,
            Err(e) => {
                error!("Failed to create decoder: {}", e);
                return;
            }
        };

        // Decode the entire file
        let mut format_reader = format_reader;
        let mut decoded_samples = Vec::new();

        while let Ok(packet) = format_reader.format.next_packet() {
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
                }
                Err(e) => {
                    error!("Decode error: {}", e);
                    break;
                }
            }
        }

        // Send the decoded audio to the player
        debug!("Sending {} samples to player", decoded_samples.len());
        let _ = self
            .bus_sender
            .send(Message::Audio(AudioMessage::BufferReady {
                samples: decoded_samples,
                sample_rate,
                channels: channels as u16,
            }));
    }
}
