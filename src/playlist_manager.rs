use std::collections::HashSet;

use log::{debug, error, trace};
use tokio::sync::broadcast::{Receiver, Sender};
use uuid::Uuid;

use crate::{
    playlist::{Playlist, Track},
    protocol::{self, TrackIdentifier},
};

// Manages the playlist
pub struct PlaylistManager {
    playlist: Playlist,
    bus_consumer: Receiver<protocol::Message>,
    bus_producer: Sender<protocol::Message>,
    cached_track_ids: HashSet<String>,
    max_num_cached_tracks: usize,
    current_track_duration_ms: u64,
    last_seek_ms: u64,
}

impl PlaylistManager {
    pub fn new(
        playlist: Playlist,
        bus_consumer: Receiver<protocol::Message>,
        bus_producer: Sender<protocol::Message>,
    ) -> Self {
        Self {
            playlist,
            bus_consumer,
            bus_producer,
            cached_track_ids: HashSet::new(),
            max_num_cached_tracks: 6,
            current_track_duration_ms: 0,
            last_seek_ms: u64::MAX,
        }
    }

    fn play_selected_track(&mut self) {
        self.playlist.set_playing(true);
        self.playlist
            .set_playing_track_index(Some(self.playlist.get_selected_track_index()));
        self.playlist.force_re_randomize_shuffle();

        // Notify other components about the selection and playing change
        self.broadcast_playlist_changed();

        if !self.cached_track_ids.contains(
            &self
                .playlist
                .get_track_id(self.playlist.get_selected_track_index()),
        ) {
            self.stop_decoding();
            self.cached_track_ids.clear();
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::ClearPlayerCache,
            ));
            self.cache_tracks(true);
        } else {
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::PlayTrackById(
                    self.playlist
                        .get_track_id(self.playlist.get_selected_track_index()),
                ),
            ));
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTrack(path)) => {
                        debug!("PlaylistManager: Loading track {:?}", path);
                        self.playlist.add_track(Track {
                            path,
                            id: Uuid::new_v4().to_string(),
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        debug!("PlaylistManager: Received play command");
                        if !self.playlist.is_playing()
                            && self.playlist.get_playing_track_index()
                                == Some(self.playlist.get_selected_track_index())
                        {
                            debug!("PlaylistManager: Resuming playback");
                            self.playlist.set_playing(true);
                        } else {
                            self.playlist.force_re_randomize_shuffle();
                            self.play_selected_track();
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackByIndex(
                        index,
                    )) => {
                        debug!("PlaylistManager: Received play track command: {}", index);
                        self.playlist.set_selected_indices(vec![index]);
                        self.playlist.force_re_randomize_shuffle();
                        self.play_selected_track();
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        debug!("PlaylistManager: Received stop command");
                        self.playlist.set_playing(false);
                        self.playlist.set_playing_track_index(None);
                        self.clear_cached_tracks();
                        self.cache_tracks(false);

                        // Notify other components about the selection and playing change
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                        debug!("PlaylistManager: Received pause command");
                        if self.playlist.is_playing() {
                            self.playlist.set_playing(false);
                        } else if self.playlist.get_playing_track_index()
                            == Some(self.playlist.get_selected_track_index())
                        {
                            debug!("PlaylistManager: Resuming playback via Pause button");
                            let _ = self
                                .bus_producer
                                .send(protocol::Message::Playback(protocol::PlaybackMessage::Play));
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Next) => {
                        debug!("PlaylistManager: Received next command");
                        let current_index = self
                            .playlist
                            .get_playing_track_index()
                            .unwrap_or(self.playlist.get_selected_track_index());
                        if let Some(next_index) = self.playlist.get_next_track_index(current_index)
                        {
                            self.playlist.set_selected_indices(vec![next_index]);
                            self.play_selected_track();
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Previous) => {
                        debug!("PlaylistManager: Received previous command");
                        let current_index = self
                            .playlist
                            .get_playing_track_index()
                            .unwrap_or(self.playlist.get_selected_track_index());
                        if let Some(prev_index) =
                            self.playlist.get_previous_track_index(current_index)
                        {
                            self.playlist.set_selected_indices(vec![prev_index]);
                            self.play_selected_track();
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::TrackFinished(id)) => {
                        debug!("PlaylistManager: Received track finished command: {}", id);
                        if let Some(playing_idx) = self.playlist.get_playing_track_index() {
                            let index = self.playlist.get_next_track_index(playing_idx);

                            let mut advanced = false;
                            if let Some(index) = index {
                                if index < self.playlist.num_tracks() {
                                    self.playlist.set_playing_track_index(Some(index));
                                    self.playlist.set_selected_indices(vec![index]);
                                    self.cache_tracks(false);
                                    advanced = true;
                                }
                            }

                            if !advanced {
                                self.playlist.set_playing(false);
                                self.playlist.set_playing_track_index(None);
                                let _ = self.bus_producer.send(protocol::Message::Playback(
                                    protocol::PlaybackMessage::Stop,
                                ));
                            }

                            // Notify other components about the selection and playing change
                            self.broadcast_playlist_changed();

                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::TrackFinished,
                            ));
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::TrackStarted(
                        track_started,
                    )) => {
                        debug!(
                            "PlaylistManager: Received track started command: {}",
                            track_started.id
                        );
                        self.last_seek_ms = u64::MAX;
                        if let Some(playing_idx) = self.playlist.get_playing_track_index() {
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::TrackStarted(playing_idx),
                            ));
                            // Also notify UI to update metadata/art if selection is empty
                            self.broadcast_playlist_changed();
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::ReadyForPlayback(
                        id,
                    )) => {
                        debug!(
                            "PlaylistManager: Received ready for playback command for track: {}",
                            id
                        );

                        // Inform the audio player which track to play from its cache
                        if self.playlist.is_playing() {
                            if let Some(playing_idx) = self.playlist.get_playing_track_index() {
                                let playing_track_id =
                                    self.playlist.get_track(playing_idx).id.clone();

                                if playing_track_id == id {
                                    let _ = self.bus_producer.send(protocol::Message::Playback(
                                        protocol::PlaybackMessage::PlayTrackById(id),
                                    ));
                                }
                            }
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTrack(index)) => {
                        debug!("PlaylistManager: Received delete track command: {}", index);
                        if index < self.playlist.num_tracks() {
                            if self
                                .cached_track_ids
                                .contains(&self.playlist.get_track_id(index))
                            {
                                self.cached_track_ids
                                    .remove(&self.playlist.get_track_id(index));
                                self.clear_cached_tracks();
                            }
                            self.playlist.delete_track(index);

                            // Notify other components about the index shift
                            self.broadcast_playlist_changed();
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::SelectTrackMulti {
                        index,
                        ctrl,
                        shift,
                    }) => {
                        debug!(
                            "PlaylistManager: Received multi-select command: index={}, ctrl={}, shift={}",
                            index, ctrl, shift
                        );
                        self.playlist.select_track_multi(index, ctrl, shift);

                        // Notify other components about the selection change
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeselectAll) => {
                        debug!("PlaylistManager: Received deselect all command");
                        self.playlist.deselect_all();

                        // Notify other components about the selection change
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                        indices,
                        to,
                    }) => {
                        debug!("PlaylistManager: Reordering tracks {:?} to {}", indices, to);
                        self.playlist.move_tracks(indices, to);

                        // Notify other components about the index shift
                        self.broadcast_playlist_changed();

                        // Re-cache to ensure next tracks are correct
                        self.cache_tracks(false);
                    }
                    protocol::Message::Audio(protocol::AudioMessage::TrackCached(id)) => {
                        debug!("PlaylistManager: Received TrackCached: {}", id);
                        self.cached_track_ids.insert(id);
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::ChangePlaybackOrder(order),
                    ) => {
                        debug!(
                            "PlaylistManager: Received change playback order command: {:?}",
                            order
                        );
                        self.playlist.set_playback_order(order);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ToggleRepeat) => {
                        let repeat_on = self.playlist.toggle_repeat();
                        debug!("PlaylistManager: Toggled repeat to {}", repeat_on);
                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::RepeatModeChanged(repeat_on),
                        ));
                    }
                    protocol::Message::Playback(
                        protocol::PlaybackMessage::TechnicalMetadataChanged(meta),
                    ) => {
                        debug!(
                            "PlaylistManager: Received technical metadata changed for track: {:?}",
                            meta
                        );
                        self.current_track_duration_ms = meta.duration_ms;
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Seek(percentage)) => {
                        let target_ms =
                            (percentage as f64 * self.current_track_duration_ms as f64) as u64;

                        if target_ms == self.last_seek_ms {
                            continue;
                        }
                        self.last_seek_ms = target_ms;

                        debug!(
                            "PlaylistManager: Seeking to {}ms ({}% of {})",
                            target_ms,
                            percentage * 100.0,
                            self.current_track_duration_ms
                        );

                        if let Some(playing_idx) = self.playlist.get_playing_track_index() {
                            let track = self.playlist.get_track(playing_idx);
                            let track_id = track.id.clone();
                            let track_path = track.path.clone();

                            // 1. Stop current decoding
                            self.stop_decoding();

                            // 2. Clear player cache
                            let _ = self.bus_producer.send(protocol::Message::Playback(
                                protocol::PlaybackMessage::ClearPlayerCache,
                            ));

                            // 3. Restart decoding at offset
                            let _ = self.bus_producer.send(protocol::Message::Audio(
                                protocol::AudioMessage::DecodeTracks(vec![TrackIdentifier {
                                    id: track_id,
                                    path: track_path,
                                    play_immediately: true,
                                    start_offset_ms: target_ms,
                                }]),
                            ));
                        }
                    }
                    _ => trace!("PlaylistManager: ignoring unsupported message"),
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Ignore lag as we've increased the bus capacity
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!("PlaylistManager: bus closed");
                    break;
                }
            }
        }
    }

    fn cache_tracks(&mut self, play_immediately: bool) {
        let first_index = self.playlist.get_selected_track_index();
        let mut current_index = self.playlist.get_selected_track_index();
        let mut track_paths = Vec::new();

        for _ in 0..self.max_num_cached_tracks {
            if !self
                .cached_track_ids
                .contains(&self.playlist.get_track_id(current_index))
            {
                track_paths.push(TrackIdentifier {
                    id: self.playlist.get_track(current_index).id.clone(),
                    path: self.playlist.get_track(current_index).path.clone(),
                    play_immediately: play_immediately && current_index == first_index,
                    start_offset_ms: 0,
                });
            }
            if let Some(next_index) = self.playlist.get_next_track_index(current_index) {
                current_index = next_index;
                if next_index >= self.playlist.num_tracks() {
                    break;
                }
            } else {
                break;
            }
        }
        debug!(
            "PlaylistManager: Updated cached tracks: {:?}",
            self.cached_track_ids
        );
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::DecodeTracks(track_paths),
        ));
    }

    fn clear_cached_tracks(&mut self) {
        self.cached_track_ids.clear();
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::StopDecoding,
        ));
        let _ = self.bus_producer.send(protocol::Message::Playback(
            protocol::PlaybackMessage::ClearPlayerCache,
        ));
    }

    fn stop_decoding(&mut self) {
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::StopDecoding,
        ));
    }

    fn broadcast_playlist_changed(&self) {
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistIndicesChanged {
                playing_index: self.playlist.get_playing_track_index(),
                selected_indices: self.playlist.get_selected_indices(),
                is_playing: self.playlist.is_playing(),
                repeat_on: self.playlist.get_repeat_mode() == crate::playlist::RepeatMode::On,
            },
        ));
    }
}
