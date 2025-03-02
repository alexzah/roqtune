use crate::protocol::{self, AudioMessage, Message, PlaybackMessage, PlaylistMessage};
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
    resampler: Option<SincFixedIn<f32>>,
    resample_buffer: VecDeque<f32>,
}

impl DecodeWorker {
    pub fn new(bus_sender: Sender<Message>, work_receiver: MpscReceiver<DecodeWorkItem>) -> Self {
        Self {
            bus_sender,
            work_receiver: work_receiver,
            work_queue: VecDeque::new(),
            resampler: None,
            resample_buffer: VecDeque::new(),
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

    fn create_resampler(&mut self, source_sample_rate: u32, chunk_size: usize) -> SincFixedIn<f32> {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        // TODO: get sample_rate and channels as message from player
        let target_sample_rate = 48000;
        let target_channels = 2;
        SincFixedIn::<f32>::new(
            target_sample_rate as f64 / source_sample_rate as f64,
            2.0,
            params,
            chunk_size,
            target_channels,
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

        // TODO: get channels as message from player
        let channels: usize = 2;
        let target_sample_rate = 48000;
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
            let deinterleaved = Self::deinterleave(&samples, channels);
            let mut waves_out = vec![vec![]; channels];
            if deinterleaved[0].len() == resampler.input_frames_next() {
                waves_out = resampler.process(&deinterleaved, None).unwrap();
            } else {
                waves_out = resampler
                    .process_partial(Some(&deinterleaved), None)
                    .unwrap();
                if let Ok(result) = resampler.process_partial::<&[f32]>(None, None) {
                    for i in 0..channels {
                        waves_out[i].extend(result[i].iter());
                    }
                }
            }
            result = Self::interleave(&waves_out);
        }

        result
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

        if self.resampler.is_none() {
            self.resampler = Some(self.create_resampler(sample_rate, 2048));
        }

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

                    // Send chunk when we have enough samples
                    if decoded_chunk.len() >= chunk_size {
                        trace!(
                            "Got chunk of size {} samples, expecting {}",
                            decoded_chunk.len(),
                            chunk_size
                        );
                        self.resample_buffer.extend(decoded_chunk.iter());
                        if self.resample_buffer.len() >= chunk_size {
                            let resampled_samples = self.resample_next_frame(sample_rate);
                            let _ =
                                self.bus_sender
                                    .send(Message::Audio(AudioMessage::BufferReady {
                                        samples: resampled_samples,
                                        sample_rate,
                                        channels: channels as u16,
                                    }));
                        }

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
            let resampled_samples = self.resample_next_frame(sample_rate);
            // debug!("Sending final chunk of {} samples", decoded_chunk.len());
            let _ = self
                .bus_sender
                .send(Message::Audio(AudioMessage::BufferReady {
                    samples: resampled_samples,
                    sample_rate,
                    channels: channels as u16,
                }));
        }

        // Flush resampler queue
        while self.resample_buffer.len() > 0 {
            if !self.should_continue() {
                debug!("DecodeWorker: Stopping");
                return;
            }
            self.resample_next_frame(sample_rate);
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
