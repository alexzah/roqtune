use crate::protocol::{self, AudioMessage, Message, PlaybackMessage, PlaylistMessage};
use log::{debug, error, trace};
use std::collections::VecDeque;
use std::path::{self, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender};

#[derive(Debug, Clone)]
enum DecodeWorkItem {
    DecodeFile(PathBuf),
    Stop,
}

// Worker to decode in a separate thread
struct DecodeWorker {
    bus_sender: Sender<Message>,
    work_receiver: MpscReceiver<DecodeWorkItem>,
    work_queue: VecDeque<DecodeWorkItem>,
}

impl DecodeWorker {
    pub fn new(bus_sender: Sender<Message>, work_receiver: MpscReceiver<DecodeWorkItem>) -> Self {
        Self {
            bus_sender,
            work_receiver: work_receiver,
            work_queue: VecDeque::new(),
        }
    }

    pub fn run(&mut self) {
        loop {
            while let Some(item) = self.work_receiver.blocking_recv() {
                match item {
                    DecodeWorkItem::Stop => {
                        debug!("DecodeWorker: Received stop signal");
                        self.work_queue.clear();
                        self.bus_sender
                            .send(Message::Playback(PlaybackMessage::ClearPlayerCache))
                            .unwrap();
                    }
                    DecodeWorkItem::DecodeFile(path) => {
                        self.work_queue
                            .push_back(DecodeWorkItem::DecodeFile(path.clone()));
                        if let Some(DecodeWorkItem::DecodeFile(path)) = self.work_queue.pop_front()
                        {
                            self.decode_file(path);
                        }
                    }
                }
            }
        }
    }

    fn should_continue(&mut self) -> bool {
        // try_recv() returns None if channel is empty, Some if there's a message
        match self.work_receiver.try_recv() {
            Ok(DecodeWorkItem::Stop) => {
                self.work_queue.clear();
                self.bus_sender
                    .send(Message::Playback(PlaybackMessage::ClearPlayerCache))
                    .unwrap();
                false
            }
            Ok(other) => {
                self.work_queue.push_back(other);
                true
            }
            Err(_) => true,
        }
    }

    pub fn load_file(&mut self, path: PathBuf) {
        debug!("DecodeWorker: Loading file: {:?}", path);
        self.work_queue.push_back(DecodeWorkItem::DecodeFile(path));
    }

    pub fn decode_file(&mut self, path: PathBuf) {
        debug!("DecodeWorker: Decoding file: {:?}", path);
        let file = match std::fs::File::open(path.clone()) {
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

        // Decode in chunks
        let mut format_reader = format_reader;
        let chunk_size = sample_rate as usize * channels * 2; // 2 seconds worth of audio
        let mut decoded_chunk = Vec::with_capacity(chunk_size);

        while let Ok(packet) = format_reader.format.next_packet() {
            if !self.should_continue() {
                debug!("DecodeWorker: Stopping");
                return;
            }

            if packet.track_id() != track_id {
                continue;
            }

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = decoded.spec();
                    let duration = decoded.capacity() as u64;

                    let mut sample_buffer = SampleBuffer::<f32>::new(duration, *spec);
                    sample_buffer.copy_interleaved_ref(decoded);

                    decoded_chunk.extend_from_slice(sample_buffer.samples());

                    // Send chunk when we have enough samples
                    if decoded_chunk.len() >= chunk_size {
                        trace!("Sending chunk of {} samples", decoded_chunk.len());
                        let _ = self
                            .bus_sender
                            .send(Message::Audio(AudioMessage::BufferReady {
                                samples: decoded_chunk,
                                sample_rate,
                                channels: channels as u16,
                            }));
                        decoded_chunk = Vec::with_capacity(chunk_size);
                    }
                }
                Err(e) => {
                    error!("Decode error: {}", e);
                    break;
                }
            }
        }

        // Send any remaining samples
        if !decoded_chunk.is_empty() {
            if !self.should_continue() {
                debug!("DecodeWorker: Stopping");
                return;
            }
            debug!("Sending final chunk of {} samples", decoded_chunk.len());
            let _ = self
                .bus_sender
                .send(Message::Audio(AudioMessage::BufferReady {
                    samples: decoded_chunk,
                    sample_rate,
                    channels: channels as u16,
                }));
        }
    }
}

pub struct AudioDecoder {
    bus_receiver: Receiver<Message>,
    bus_sender: Sender<Message>,
    worker_sender: MpscSender<DecodeWorkItem>,
}

impl AudioDecoder {
    pub fn new(bus_receiver: Receiver<Message>, bus_sender: Sender<Message>) -> Self {
        let (worker_sender, worker_receiver) = mpsc::channel(20);
        let mut audio_decoder = Self {
            bus_receiver,
            bus_sender,
            worker_sender: worker_sender,
        };
        audio_decoder.spawn_decode_worker(worker_receiver);
        audio_decoder
    }

    pub fn run(&mut self) {
        loop {
            while let Ok(message) = self.bus_receiver.blocking_recv() {
                match message {
                    Message::Audio(AudioMessage::DecodeTracks(paths)) => {
                        debug!("AudioDecoder: Loading tracks {:?}", paths);
                        for path in paths {
                            self.worker_sender
                                .blocking_send(DecodeWorkItem::DecodeFile(path))
                                .unwrap();
                        }
                    }
                    Message::Audio(AudioMessage::ClearCache) => {
                        debug!("AudioDecoder: Clearing cache");
                        self.worker_sender.blocking_send(DecodeWorkItem::Stop);
                    }
                    _ => {} // Ignore other messages for now
                }
            }
            error!("AudioDecoder: receiver error, restarting loop");
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
