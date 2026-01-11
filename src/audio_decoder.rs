use crate::protocol::{
    self, AudioMessage, AudioPacket, Config, ConfigMessage, Message, PlaybackMessage,
    PlaylistMessage, TrackIdentifier,
};
use log::{debug, error, trace};
use rubato::{
    FftFixedIn, Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::cmp::min;
use std::collections::VecDeque;
use std::path::{self, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::{FormatOptions, Track};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::mpsc::{self, Receiver as MpscReceiver, Sender as MpscSender};

#[derive(Debug, Clone)]
enum DecodeWorkItem {
    DecodeTracks(Vec<TrackIdentifier>),
    DecodeTrack(TrackIdentifier),
    Stop,
    ConfigChanged(Config),
}

// Worker to decode in a separate thread
struct DecodeWorker {
    bus_sender: Sender<Message>,
    work_receiver: MpscReceiver<DecodeWorkItem>,
    work_queue: VecDeque<DecodeWorkItem>,
    resampler: Option<SincFixedIn<f32>>,
    resample_buffer: VecDeque<f32>,
    target_sample_rate: u32,
    target_channels: u16,
    target_bits_per_sample: u16,
}

impl DecodeWorker {
    pub fn new(bus_sender: Sender<Message>, work_receiver: MpscReceiver<DecodeWorkItem>) -> Self {
        Self {
            bus_sender,
            work_receiver: work_receiver,
            work_queue: VecDeque::new(),
            resampler: None,
            resample_buffer: VecDeque::new(),
            target_sample_rate: 0,
            target_channels: 0,
            target_bits_per_sample: 0,
        }
    }

    pub fn run(&mut self) {
        loop {
            // Process any items in the work queue first
            while let Some(item) = self.work_queue.pop_front() {
                match item {
                    DecodeWorkItem::Stop => {
                        debug!("DecodeWorker: Received stop signal");
                        self.work_queue.clear();
                        self.resample_buffer.clear();
                        self.resampler = None;
                    }
                    DecodeWorkItem::DecodeTracks(tracks) => {
                        for track in tracks {
                            self.work_queue
                                .push_back(DecodeWorkItem::DecodeTrack(track));
                        }
                    }
                    DecodeWorkItem::DecodeTrack(track) => {
                        self.decode_track(track);
                    }
                    DecodeWorkItem::ConfigChanged(config) => {
                        self.target_sample_rate = config.output.sample_rate_khz;
                        self.target_channels = config.output.channel_count;
                        self.target_bits_per_sample = config.output.bits_per_sample;
                    }
                }
            }

            // Wait for new work
            if let Some(item) = self.work_receiver.blocking_recv() {
                self.work_queue.push_back(item);
            }
        }
    }

    fn should_continue(&mut self) -> bool {
        // try_recv() returns None if channel is empty, Some if there's a message
        match self.work_receiver.try_recv() {
            Ok(DecodeWorkItem::Stop) => {
                debug!("DecodeWorker: Received stop signal");
                self.work_queue.clear();
                self.resample_buffer.clear();
                self.resampler = None;
                false
            }
            Ok(other) => {
                self.work_queue.push_back(other);
                true
            }
            Err(_) => true,
        }
    }

    fn create_resampler(&mut self, source_sample_rate: u32, chunk_size: usize) -> SincFixedIn<f32> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        // TODO: get sample_rate and channels as message from player
        SincFixedIn::<f32>::new(
            self.target_sample_rate as f64 / source_sample_rate as f64,
            2.0,
            params,
            chunk_size,
            self.target_channels as usize,
        )
        .unwrap()
    }

    fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
        let mut deinterleaved = vec![vec![]; channels];
        for (i, sample) in samples.iter().enumerate() {
            deinterleaved[i % channels].push(*sample);
        }
        deinterleaved
    }

    fn interleave(samples: &[Vec<f32>]) -> Vec<f32> {
        let mut interleaved = Vec::new();
        for i in 0..samples[0].len() {
            for sample in samples {
                interleaved.push(sample[i]);
            }
        }
        interleaved
    }

    pub fn resample_next_frame(&mut self, sample_rate: u32) -> Vec<f32> {
        let mut samples = Vec::new();
        // Resample the incoming audio
        if self.resampler.is_none() {
            self.resampler = Some(self.create_resampler(sample_rate, 2048));
        }

        let channels: usize = self.target_channels as usize;
        let mut result: Vec<f32> = Vec::new();
        if let Some(resampler) = &mut self.resampler {
            trace!(
                "Attempting to pull {} samples from resample buffer of size {}",
                resampler.input_frames_next() * channels,
                self.resample_buffer.len()
            );
            for _ in 0..min(
                resampler.input_frames_next() * channels,
                self.resample_buffer.len(),
            ) {
                samples.push(self.resample_buffer.pop_front().unwrap());
            }

            // No need to resample
            if sample_rate == self.target_sample_rate {
                return samples;
            }

            let deinterleaved = Self::deinterleave(&samples, channels);
            let mut waves_out = vec![vec![]; channels];
            if deinterleaved[0].len() == resampler.input_frames_next() {
                waves_out = resampler.process(&deinterleaved, None).unwrap();
            } else {
                waves_out = resampler
                    .process_partial(Some(&deinterleaved), None)
                    .unwrap();
                if let Ok(result) = resampler.process_partial::<&[f32]>(None, None) {
                    // if result.is_empty() || result[0].is_empty() {
                    //     break;
                    // }
                    for i in 0..channels {
                        waves_out[i].extend(result[i].iter());
                    }
                }
            }
            result = Self::interleave(&waves_out);
        }

        result
    }

    pub fn decode_track(&mut self, input_track: TrackIdentifier) {
        debug!("DecodeWorker: Decoding file: {:?}", input_track);
        let file = match std::fs::File::open(input_track.path.clone()) {
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

        if self.resampler.is_none() {
            self.resampler = Some(self.create_resampler(sample_rate, 2048));
        }

        self.resampler
            .as_mut()
            .expect("Resampler just initialized")
            .reset();

        let target_sample_rate = 48000;
        let target_channels = 2;

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

        // Send the track header
        self.bus_sender
            .send(Message::Audio(AudioMessage::AudioPacket(
                AudioPacket::TrackHeader {
                    id: input_track.id.to_string(),
                    play_immediately: input_track.play_immediately,
                },
            )))
            .unwrap();

        // Decode in chunks
        let mut format_reader = format_reader;
        let chunk_size = self.resampler.as_ref().unwrap().input_frames_next() * channels;
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

                    trace!(
                        "Got chunk of size {} samples, expecting {}",
                        decoded_chunk.len(),
                        chunk_size
                    );
                    self.resample_buffer.extend(decoded_chunk.iter());
                    while self.resample_buffer.len() >= chunk_size {
                        let resampled_samples = self.resample_next_frame(sample_rate);
                        let _ = self
                            .bus_sender
                            .send(Message::Audio(AudioMessage::AudioPacket(
                                AudioPacket::Samples {
                                    samples: resampled_samples,
                                    sample_rate,
                                    channels: channels as u16,
                                },
                            )));
                    }

                    decoded_chunk = Vec::with_capacity(chunk_size);
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
            self.resample_buffer.extend(decoded_chunk.iter());
        }

        // Flush resampler queue
        while self.resample_buffer.len() > 0 {
            if !self.should_continue() {
                debug!("DecodeWorker: Stopping");
                return;
            }
            let resampled_samples = self.resample_next_frame(sample_rate);
            let _ = self
                .bus_sender
                .send(Message::Audio(AudioMessage::AudioPacket(
                    AudioPacket::Samples {
                        samples: resampled_samples,
                        sample_rate,
                        channels: channels as u16,
                    },
                )));
        }

        self.bus_sender
            .send(Message::Audio(AudioMessage::AudioPacket(
                AudioPacket::TrackFooter {
                    id: input_track.id.clone(),
                },
            )))
            .unwrap();

        self.resampler = None;
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
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    Message::Audio(AudioMessage::DecodeTracks(paths)) => {
                        debug!("AudioDecoder: Loading tracks {:?}", paths);
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::DecodeTracks(paths))
                            .unwrap();
                    }
                    Message::Audio(AudioMessage::StopDecoding) => {
                        debug!("AudioDecoder: Clearing cache");
                        let _ = self.worker_sender.blocking_send(DecodeWorkItem::Stop);
                    }
                    Message::Config(ConfigMessage::ConfigChanged(config)) => {
                        debug!(
                            "AudioDecoder: Received config changed command: {:?}",
                            config
                        );
                        self.worker_sender
                            .blocking_send(DecodeWorkItem::ConfigChanged(config))
                            .unwrap();
                    }
                    _ => {} // Ignore other messages for now
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Ignore lag as we've increased the bus capacity
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!("AudioDecoder: bus closed");
                    break;
                }
            }
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
