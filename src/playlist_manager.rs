use std::collections::HashSet;

use log::{debug, error, trace};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{
    playlist::{self, Playlist, Track},
    protocol,
};

// Manages the playlist
pub struct PlaylistManager {
    playlist: Playlist,
    bus_consumer: Receiver<protocol::Message>,
    bus_producer: Sender<protocol::Message>,
    cached_track_indices: HashSet<usize>,
    max_num_cached_tracks: usize,
}

impl PlaylistManager {
    pub fn new(
        playlist: Playlist,
        bus_consumer: Receiver<protocol::Message>,
        bus_producer: Sender<protocol::Message>,
    ) -> Self {
        Self {
            playlist: playlist,
            bus_consumer,
            bus_producer,
            cached_track_indices: HashSet::new(),
            max_num_cached_tracks: 2,
        }
    }

    pub fn run(&mut self) {
        loop {
            while let Ok(message) = self.bus_consumer.blocking_recv() {
                match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTrack(path)) => {
                        debug!("PlaylistManager: Loading track {:?}", path);
                        self.playlist.add_track(Track { path: path });
                        self.cache_tracks();
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        debug!("PlaylistManager: Received play command");
                        self.playlist.set_playing(true);
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrack(index)) => {
                        debug!("PlaylistManager: Received play track command: {}", index);
                        let original_selected_track_index =
                            self.playlist.get_selected_track_index();
                        self.playlist.set_selected_track(index);
                        self.playlist.set_playing(true);
                        if original_selected_track_index != index {
                            self.clear_cached_tracks();
                            self.cache_tracks();
                        } else {
                            self.bus_producer
                                .send(protocol::Message::Playback(protocol::PlaybackMessage::Play))
                                .unwrap();
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        debug!("PlaylistManager: Received stop command");
                        self.playlist.set_playing(false);
                        self.clear_cached_tracks();
                        self.cache_tracks();
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::TrackFinished) => {
                        debug!("PlaylistManager: Received track finished command");
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::ReadyForPlayback) => {
                        debug!("PlaylistManager: Received ready for playback command");
                        if self.playlist.is_playing() {
                            self.bus_producer
                                .send(protocol::Message::Playback(protocol::PlaybackMessage::Play))
                                .unwrap();
                        }
                    }
                    _ => trace!("PlaylistManager: ignoring unsupported message"),
                }
            }
        }
    }

    fn cache_tracks(&mut self) {
        let mut current_index = self.playlist.get_selected_track_index();
        let mut track_paths = Vec::new();
        if self.cached_track_indices.len() < self.max_num_cached_tracks {
            for _ in 0..self.max_num_cached_tracks {
                if !self.cached_track_indices.contains(&current_index) {
                    self.cached_track_indices.insert(current_index);
                    track_paths.push(self.playlist.get_track(current_index).path.clone());
                }
                if let Some(next_index) = self.playlist.get_next_track_index(current_index) {
                    current_index = next_index;
                } else {
                    break;
                }
            }
        }
        debug!(
            "PlaylistManager: Caching tracks: {:?}",
            self.cached_track_indices
        );
        self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::DecodeTracks(track_paths),
        ));
    }

    fn clear_cached_tracks(&mut self) {
        self.cached_track_indices.clear();
        self.bus_producer
            .send(protocol::Message::Audio(protocol::AudioMessage::ClearCache));
    }
}
