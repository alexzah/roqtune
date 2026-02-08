use std::collections::{HashMap, HashSet};

use log::{debug, error, info, trace};
use tokio::sync::broadcast::{Receiver, Sender};
use uuid::Uuid;

use crate::{
    db_manager::DbManager,
    playlist::{Playlist, Track},
    protocol::{self, TrackIdentifier},
};

// Manages the playlist
pub struct PlaylistManager {
    editing_playlist: Playlist,
    active_playlist_id: String,
    playback_playlist: Playlist,
    playback_playlist_id: Option<String>,
    playback_order: protocol::PlaybackOrder,
    repeat_mode: protocol::RepeatMode,
    bus_consumer: Receiver<protocol::Message>,
    bus_producer: Sender<protocol::Message>,
    db_manager: DbManager,
    cached_track_ids: HashMap<String, u64>, // id -> start_offset_ms
    fully_cached_track_ids: HashSet<String>,
    pending_order_change: Option<protocol::PlaybackOrder>,
    max_num_cached_tracks: usize,
    current_track_duration_ms: u64,
    last_seek_ms: u64,
    started_track_id: Option<String>,
}

impl PlaylistManager {
    pub fn new(
        playlist: Playlist,
        bus_consumer: Receiver<protocol::Message>,
        bus_producer: Sender<protocol::Message>,
        db_manager: DbManager,
    ) -> Self {
        Self {
            editing_playlist: playlist.clone(),
            active_playlist_id: String::new(),
            playback_playlist: playlist,
            playback_playlist_id: None,
            playback_order: protocol::PlaybackOrder::Default,
            repeat_mode: protocol::RepeatMode::Off,
            bus_consumer,
            bus_producer,
            db_manager,
            cached_track_ids: HashMap::new(),
            fully_cached_track_ids: HashSet::new(),
            pending_order_change: None,
            max_num_cached_tracks: 2,
            current_track_duration_ms: 0,
            last_seek_ms: u64::MAX,
            started_track_id: None,
        }
    }

    fn play_playback_track(&mut self, index: usize) {
        if index >= self.playback_playlist.num_tracks() {
            debug!("play_playback_track: Index {} out of bounds", index);
            return;
        }

        self.started_track_id = None;
        self.playback_playlist.set_playing(true);
        self.playback_playlist.set_playing_track_index(Some(index));

        let track_id = self.playback_playlist.get_track_id(index);

        if self.cached_track_ids.get(&track_id) == Some(&0) {
            // Already cached at start, tell player to switch immediately
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::PlayTrackById(track_id),
            ));
        } else {
            // Not cached or cached at wrong offset, start decoding process
            self.stop_decoding();
            self.cached_track_ids.clear();
            self.fully_cached_track_ids.clear();
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::ClearPlayerCache,
            ));
            self.cache_tracks(true);
        }
        self.broadcast_playlist_changed();
    }

    pub fn run(&mut self) {
        // Restore playlists from database
        let playlists = match self.db_manager.get_all_playlists() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to get playlists from database: {}", e);
                Vec::new()
            }
        };

        if !playlists.is_empty() {
            self.active_playlist_id = playlists[0].id.clone();
            info!(
                "Restoring playlists. Active: {} ({})",
                playlists[0].name, playlists[0].id
            );

            // Restore tracks for active playlist
            match self
                .db_manager
                .get_tracks_for_playlist(&self.active_playlist_id)
            {
                Ok(tracks) => {
                    info!("Restoring {} tracks from database", tracks.len());
                    for track in tracks.iter() {
                        self.editing_playlist.add_track(Track {
                            path: track.path.clone(),
                            id: track.id.clone(),
                        });
                    }
                    let _ = self.bus_producer.send(protocol::Message::Playlist(
                        protocol::PlaylistMessage::PlaylistRestored(tracks),
                    ));
                }
                Err(e) => {
                    error!("Failed to restore tracks from database: {}", e);
                }
            }

            let _ = self.bus_producer.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::PlaylistsRestored(playlists),
            ));
            let _ = self.bus_producer.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::ActivePlaylistChanged(self.active_playlist_id.clone()),
            ));
        }

        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTrack(path)) => {
                        debug!("PlaylistManager: Loading track {:?}", path);
                        let id = Uuid::new_v4().to_string();
                        let position = self.editing_playlist.num_tracks();

                        if let Err(e) = self.db_manager.save_track(
                            &id,
                            &self.active_playlist_id,
                            path.to_str().unwrap_or(""),
                            position,
                        ) {
                            error!("Failed to save track to database: {}", e);
                        }

                        self.editing_playlist.add_track(Track {
                            path: path.clone(),
                            id: id.clone(),
                        });

                        if Some(&self.active_playlist_id) == self.playback_playlist_id.as_ref() {
                            self.playback_playlist.add_track(Track {
                                path: path.clone(),
                                id: id.clone(),
                            });
                        }

                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::TrackAdded { id, path },
                        ));
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        debug!("PlaylistManager: Received play command");
                        if !self.playback_playlist.is_playing()
                            && self.playback_playlist_id == Some(self.active_playlist_id.clone())
                            && self.playback_playlist.num_tracks() > 0
                            && self.playback_playlist.get_playing_track_index()
                                == Some(self.editing_playlist.get_selected_track_index())
                        {
                            debug!("PlaylistManager: Resuming playback");
                            self.playback_playlist.set_playing(true);
                            self.broadcast_playlist_changed();
                        } else if self.editing_playlist.num_tracks() > 0 {
                            // Set playback playlist to editing playlist
                            self.playback_playlist = self.editing_playlist.clone();
                            self.playback_playlist_id = Some(self.active_playlist_id.clone());
                            // Ensure the new playback playlist has correct settings
                            self.playback_playlist
                                .set_playback_order(self.playback_order);
                            self.playback_playlist.set_repeat_mode(self.repeat_mode);

                            self.playback_playlist.force_re_randomize_shuffle();
                            let index = self.playback_playlist.get_selected_track_index();
                            self.play_playback_track(index);
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackByIndex(
                        index,
                    )) => {
                        debug!("PlaylistManager: Received play track command: {}", index);
                        if index < self.editing_playlist.num_tracks() {
                            self.editing_playlist.set_selected_indices(vec![index]);
                            // Set playback playlist to editing playlist
                            self.playback_playlist = self.editing_playlist.clone();
                            self.playback_playlist_id = Some(self.active_playlist_id.clone());
                            // Ensure the new playback playlist has correct settings
                            self.playback_playlist
                                .set_playback_order(self.playback_order);
                            self.playback_playlist.set_repeat_mode(self.repeat_mode);

                            self.playback_playlist.force_re_randomize_shuffle();
                            self.play_playback_track(index);
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        debug!("PlaylistManager: Received stop command");
                        self.started_track_id = None;
                        self.playback_playlist.set_playing(false);
                        self.playback_playlist.set_playing_track_index(None);
                        self.playback_playlist_id = None;
                        self.clear_cached_tracks();
                        self.cache_tracks(false);

                        // Notify other components about the selection and playing change
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                        debug!("PlaylistManager: Received pause command");
                        if self.playback_playlist.is_playing() {
                            self.playback_playlist.set_playing(false);
                            self.broadcast_playlist_changed();
                        } else if self.playback_playlist_id == Some(self.active_playlist_id.clone())
                            && self.playback_playlist.num_tracks() > 0
                            && self.playback_playlist.get_playing_track_index()
                                == Some(self.editing_playlist.get_selected_track_index())
                        {
                            debug!("PlaylistManager: Resuming playback via Pause button");
                            let _ = self
                                .bus_producer
                                .send(protocol::Message::Playback(protocol::PlaybackMessage::Play));
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Next) => {
                        debug!("PlaylistManager: Received next command");
                        if self.playback_playlist.num_tracks() > 0 {
                            let current_index = self
                                .playback_playlist
                                .get_playing_track_index()
                                .unwrap_or(0); // fallback to 0 if nothing playing
                            if let Some(next_index) =
                                self.playback_playlist.get_next_track_index(current_index)
                            {
                                self.play_playback_track(next_index);
                            }
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Previous) => {
                        debug!("PlaylistManager: Received previous command");
                        if self.playback_playlist.num_tracks() > 0 {
                            let current_index = self
                                .playback_playlist
                                .get_playing_track_index()
                                .unwrap_or(0);
                            if let Some(prev_index) = self
                                .playback_playlist
                                .get_previous_track_index(current_index)
                            {
                                self.play_playback_track(prev_index);
                            }
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::TrackFinished(id)) => {
                        debug!("PlaylistManager: Received track finished command: {}", id);
                        self.started_track_id = None;
                        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index()
                        {
                            let index = self.playback_playlist.get_next_track_index(playing_idx);

                            let mut advanced = false;
                            if let Some(index) = index {
                                if index < self.playback_playlist.num_tracks() {
                                    self.playback_playlist.set_playing_track_index(Some(index));
                                    self.playback_playlist.set_selected_indices(vec![index]);
                                    self.cache_tracks(false);
                                    advanced = true;
                                }
                            }

                            if !advanced {
                                self.playback_playlist.set_playing(false);
                                self.playback_playlist.set_playing_track_index(None);
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
                        self.started_track_id = Some(track_started.id.clone());
                        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index()
                        {
                            let metadata = self
                                .db_manager
                                .get_track_metadata(&track_started.id)
                                .unwrap_or(protocol::DetailedMetadata {
                                    title: "Unknown".to_string(),
                                    artist: "".to_string(),
                                    album: "".to_string(),
                                    date: "".to_string(),
                                    genre: "".to_string(),
                                });

                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::TrackStarted {
                                    index: playing_idx,
                                    playlist_id: self
                                        .playback_playlist_id
                                        .clone()
                                        .unwrap_or_default(),
                                    metadata,
                                },
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
                        if self.playback_playlist.is_playing() {
                            if let Some(playing_idx) =
                                self.playback_playlist.get_playing_track_index()
                            {
                                if playing_idx < self.playback_playlist.num_tracks() {
                                    let playing_track_id =
                                        self.playback_playlist.get_track(playing_idx).id.clone();

                                    if playing_track_id == id {
                                        let already_started_current_track =
                                            self.started_track_id.as_deref() == Some(id.as_str());

                                        if already_started_current_track {
                                            debug!(
                                                "PlaylistManager: Ignoring stale ReadyForPlayback for already started track: {}",
                                                id
                                            );
                                        } else {
                                            let _ = self.bus_producer.send(
                                                protocol::Message::Playback(
                                                    protocol::PlaybackMessage::PlayTrackById(id),
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTracks(
                        mut indices,
                    )) => {
                        debug!(
                            "PlaylistManager: Received delete tracks command: {:?}",
                            indices
                        );
                        indices.sort_by(|a, b| b.cmp(a));

                        for index in indices {
                            if index < self.editing_playlist.num_tracks() {
                                let id = self.editing_playlist.get_track_id(index);

                                if self.cached_track_ids.contains_key(&id) {
                                    self.cached_track_ids.remove(&id);
                                    self.fully_cached_track_ids.remove(&id);
                                    self.clear_cached_tracks();
                                }

                                if let Err(e) = self.db_manager.delete_track(&id) {
                                    error!("Failed to delete track from database: {}", e);
                                }

                                self.editing_playlist.delete_track(index);

                                if Some(&self.active_playlist_id)
                                    == self.playback_playlist_id.as_ref()
                                {
                                    self.playback_playlist.delete_track(index);
                                }
                            }
                        }

                        // Update positions for remaining tracks in DB
                        let all_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|i| self.editing_playlist.get_track_id(i))
                            .collect();
                        if let Err(e) = self.db_manager.update_positions(all_ids) {
                            error!("Failed to update positions in database: {}", e);
                        }

                        // Notify other components about the index shift
                        self.broadcast_playlist_changed();

                        // Notify UI manager about selection change (delete modifies selection)
                        self.broadcast_selection_changed();
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
                        self.editing_playlist.select_track_multi(index, ctrl, shift);

                        self.broadcast_selection_changed();
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeselectAll) => {
                        debug!("PlaylistManager: Received deselect all command");
                        self.editing_playlist.deselect_all();

                        self.broadcast_selection_changed();
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                        indices,
                        to,
                    }) => {
                        debug!("PlaylistManager: Reordering tracks {:?} to {}", indices, to);
                        self.editing_playlist.move_tracks(indices.clone(), to);

                        if Some(&self.active_playlist_id) == self.playback_playlist_id.as_ref() {
                            self.playback_playlist.move_tracks(indices, to);
                        }

                        // Update positions in database
                        let all_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|i| self.editing_playlist.get_track_id(i))
                            .collect();
                        if let Err(e) = self.db_manager.update_positions(all_ids) {
                            error!("Failed to update positions in database: {}", e);
                        }

                        // Notify other components about the index shift
                        self.broadcast_playlist_changed();

                        // Re-cache to ensure next tracks are correct
                        self.cache_tracks(false);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::UpdateMetadata {
                        id,
                        metadata,
                    }) => {
                        debug!("PlaylistManager: Updating metadata for track {}", id);
                        if let Err(e) = self.db_manager.update_metadata(&id, &metadata) {
                            error!("Failed to update metadata in database: {}", e);
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::CreatePlaylist {
                        name,
                    }) => {
                        let id = Uuid::new_v4().to_string();
                        debug!("PlaylistManager: Creating playlist {} ({})", name, id);
                        if let Err(e) = self.db_manager.create_playlist(&id, &name) {
                            error!("Failed to create playlist in database: {}", e);
                        } else {
                            let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::PlaylistsRestored(playlists),
                            ));
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::RenamePlaylistByIndex(index, name),
                    ) => {
                        let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                        if let Some(p) = playlists.get(index) {
                            let id = p.id.clone();
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::RenamePlaylist { id, name },
                            ));
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::RenamePlaylist {
                        id,
                        name,
                    }) => {
                        debug!("PlaylistManager: Renaming playlist {} to {}", id, name);
                        if let Err(e) = self.db_manager.rename_playlist(&id, &name) {
                            error!("Failed to rename playlist in database: {}", e);
                        } else {
                            let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::PlaylistsRestored(playlists),
                            ));
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::DeletePlaylistByIndex(index),
                    ) => {
                        let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                        if let Some(p) = playlists.get(index) {
                            let id = p.id.clone();
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::DeletePlaylist { id },
                            ));
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeletePlaylist {
                        id,
                    }) => {
                        debug!("PlaylistManager: Deleting playlist {}", id);

                        // If deleting the currently playing playlist, stop it
                        if Some(&id) == self.playback_playlist_id.as_ref() {
                            self.playback_playlist.set_playing(false);
                            self.playback_playlist.set_playing_track_index(None);
                            self.playback_playlist_id = None;
                            self.clear_cached_tracks();
                        }

                        if let Err(e) = self.db_manager.delete_playlist(&id) {
                            error!("Failed to delete playlist from database: {}", e);
                        } else {
                            let playlists = self.db_manager.get_all_playlists().unwrap_or_default();

                            // If we just deleted the playlist we were editing, switch to another one
                            if id == self.active_playlist_id {
                                if !playlists.is_empty() {
                                    let new_id = playlists[0].id.clone();
                                    let _ = self.bus_producer.send(protocol::Message::Playlist(
                                        protocol::PlaylistMessage::SwitchPlaylist { id: new_id },
                                    ));
                                } else {
                                    // No playlists left, create a default one
                                    let _ = self.bus_producer.send(protocol::Message::Playlist(
                                        protocol::PlaylistMessage::CreatePlaylist {
                                            name: "Default".to_string(),
                                        },
                                    ));
                                }
                            }

                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::PlaylistsRestored(playlists),
                            ));
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::SwitchPlaylistByIndex(index),
                    ) => {
                        let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                        if let Some(p) = playlists.get(index) {
                            let id = p.id.clone();
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::SwitchPlaylist { id },
                            ));
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::SwitchPlaylist {
                        id,
                    }) => {
                        debug!("PlaylistManager: Switching to playlist {}", id);
                        if id == self.active_playlist_id {
                            continue;
                        }

                        self.active_playlist_id = id.clone();
                        self.editing_playlist = Playlist::new(); // Clear in-memory editing playlist
                                                                 // Ensure settings are applied to new editing playlist
                        self.editing_playlist
                            .set_playback_order(self.playback_order);
                        self.editing_playlist.set_repeat_mode(self.repeat_mode);

                        match self.db_manager.get_tracks_for_playlist(&id) {
                            Ok(tracks) => {
                                for track in tracks.iter() {
                                    self.editing_playlist.add_track(Track {
                                        path: track.path.clone(),
                                        id: track.id.clone(),
                                    });
                                }
                                let _ = self.bus_producer.send(protocol::Message::Playlist(
                                    protocol::PlaylistMessage::PlaylistRestored(tracks),
                                ));
                            }
                            Err(e) => {
                                error!("Failed to switch playlist: {}", e);
                            }
                        }

                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistChanged(id),
                        ));
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Audio(protocol::AudioMessage::TrackCached(id, offset)) => {
                        debug!(
                            "PlaylistManager: Received TrackCached: {} at offset {}",
                            id, offset
                        );
                        self.cached_track_ids.insert(id.clone(), offset);
                        self.fully_cached_track_ids.insert(id.clone());

                        // Check if this was the track we were waiting for to change playback order
                        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index()
                        {
                            let current_id = self.playback_playlist.get_track_id(playing_idx);
                            if current_id == id {
                                if let Some(pending_order) = self.pending_order_change.take() {
                                    debug!("PlaylistManager: Current track fully cached, applying pending order change: {:?}", pending_order);
                                    self.apply_playback_order_change(pending_order);
                                }
                            }
                        }
                    }
                    protocol::Message::Audio(protocol::AudioMessage::TrackEvicted(id)) => {
                        debug!("PlaylistManager: Received TrackEvicted: {}", id);
                        self.cached_track_ids.remove(&id);
                        self.fully_cached_track_ids.remove(&id);
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::ChangePlaybackOrder(order),
                    ) => {
                        debug!(
                            "PlaylistManager: Received change playback order command: {:?}",
                            order
                        );
                        self.playback_order = order;
                        self.playback_playlist.set_playback_order(order);
                        self.editing_playlist.set_playback_order(order);

                        // If something is playing, clear the "next" tracks and re-cache
                        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index()
                        {
                            let current_id = self.playback_playlist.get_track_id(playing_idx);

                            if self.fully_cached_track_ids.contains(&current_id) {
                                // Current track is already fully buffered, we can swap the queue now
                                self.apply_playback_order_change(order);
                            } else {
                                // Wait for the current track to finish buffering before swapping queue
                                debug!("PlaylistManager: Delaying order change until {} is fully cached", current_id);
                                self.pending_order_change = Some(order);
                            }
                        } else {
                            // Nothing playing, clear all and re-cache
                            self.clear_cached_tracks();
                            self.cache_tracks(false);
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ToggleRepeat) => {
                        let repeat_mode = self.playback_playlist.toggle_repeat();
                        self.repeat_mode = repeat_mode;
                        self.editing_playlist.set_repeat_mode(repeat_mode);
                        debug!("PlaylistManager: Toggled repeat to {:?}", repeat_mode);
                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::RepeatModeChanged(repeat_mode),
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

                        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index()
                        {
                            if playing_idx < self.playback_playlist.num_tracks() {
                                let track = self.playback_playlist.get_track(playing_idx);
                                let track_id = track.id.clone();
                                let track_path = track.path.clone();

                                // Remove from cached list since it's no longer cached at offset 0
                                self.cached_track_ids.remove(&track_id);
                                self.fully_cached_track_ids.remove(&track_id);
                                self.started_track_id = None;

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

    fn apply_playback_order_change(&mut self, _order: protocol::PlaybackOrder) {
        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index() {
            let current_id = self.playback_playlist.get_track_id(playing_idx);

            // 1. Stop current decoding
            self.stop_decoding();

            // 2. Clear our knowledge of cached tracks except current
            let current_offset = self.cached_track_ids.get(&current_id).cloned().unwrap_or(0);
            self.cached_track_ids.clear();
            self.fully_cached_track_ids.clear();
            self.cached_track_ids
                .insert(current_id.clone(), current_offset);
            self.fully_cached_track_ids.insert(current_id);

            // 3. Tell player to clear its buffer after the current track
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::ClearNextTracks,
            ));

            // 4. Start caching according to new order
            self.cache_tracks(false);
        }
    }

    fn cache_tracks(&mut self, play_immediately: bool) {
        if self.playback_playlist.num_tracks() == 0 {
            return;
        }

        let first_index = self
            .playback_playlist
            .get_playing_track_index()
            .unwrap_or(0);
        let mut current_index = first_index;
        let mut track_paths = Vec::new();

        for _ in 0..self.max_num_cached_tracks {
            if current_index < self.playback_playlist.num_tracks() {
                let track_id = self.playback_playlist.get_track_id(current_index);
                let is_cached_at_track_start =
                    self.cached_track_ids.get(&track_id).copied() == Some(0);
                if is_cached_at_track_start {
                    if let Some(next_index) =
                        self.playback_playlist.get_next_track_index(current_index)
                    {
                        current_index = next_index;
                        if next_index >= self.playback_playlist.num_tracks() {
                            break;
                        }
                    } else {
                        break;
                    }
                    continue;
                }

                track_paths.push(TrackIdentifier {
                    id: track_id,
                    path: self.playback_playlist.get_track(current_index).path.clone(),
                    play_immediately: play_immediately && current_index == first_index,
                    start_offset_ms: 0,
                });
            }
            if let Some(next_index) = self.playback_playlist.get_next_track_index(current_index) {
                current_index = next_index;
                if next_index >= self.playback_playlist.num_tracks() {
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
        self.fully_cached_track_ids.clear();
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
        let mut playing_track_path = None;
        let mut playing_track_metadata = None;

        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index() {
            if playing_idx < self.playback_playlist.num_tracks() {
                let track = self.playback_playlist.get_track(playing_idx);
                playing_track_path = Some(track.path.clone());
                playing_track_metadata = self.db_manager.get_track_metadata(&track.id).ok();
            }
        }

        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistIndicesChanged {
                playing_playlist_id: self.playback_playlist_id.clone(),
                playing_index: self.playback_playlist.get_playing_track_index(),
                playing_track_path,
                playing_track_metadata,
                selected_indices: self.editing_playlist.get_selected_indices(),
                is_playing: self.playback_playlist.is_playing(),
                playback_order: self.playback_order,
                repeat_mode: self.repeat_mode,
            },
        ));
    }

    fn broadcast_selection_changed(&self) {
        let selected_indices = self.editing_playlist.get_selected_indices();
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::SelectionChanged(selected_indices),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast::{self, error::TryRecvError, Receiver, Sender};

    struct PlaylistManagerHarness {
        bus_sender: Sender<protocol::Message>,
        receiver: Receiver<protocol::Message>,
    }

    impl PlaylistManagerHarness {
        fn new() -> Self {
            let (bus_sender, _) = broadcast::channel(4096);
            let manager_bus_sender = bus_sender.clone();
            let manager_receiver = bus_sender.subscribe();
            let db_manager = DbManager::new_in_memory().expect("failed to create in-memory db");

            thread::spawn(move || {
                let mut manager = PlaylistManager::new(
                    Playlist::new(),
                    manager_receiver,
                    manager_bus_sender,
                    db_manager,
                );
                manager.run();
            });

            let mut receiver = bus_sender.subscribe();
            let _ = wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::ActivePlaylistChanged(
                        _
                    ))
                )
            });

            let mut harness = Self {
                bus_sender,
                receiver,
            };
            harness.drain_messages();
            harness
        }

        fn send(&self, message: protocol::Message) {
            self.bus_sender
                .send(message)
                .expect("failed to send message to bus");
        }

        fn add_track(&mut self, name: &str) -> (String, PathBuf) {
            let path = PathBuf::from(format!("/tmp/{}.mp3", name));
            self.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::LoadTrack(path.clone()),
            ));

            let message =
                wait_for_message(
                    &mut self.receiver,
                    Duration::from_secs(1),
                    |message| match message {
                        protocol::Message::Playlist(protocol::PlaylistMessage::TrackAdded {
                            path: added_path,
                            ..
                        }) => added_path == &path,
                        _ => false,
                    },
                );

            if let protocol::Message::Playlist(protocol::PlaylistMessage::TrackAdded { id, path }) =
                message
            {
                (id, path)
            } else {
                panic!("expected TrackAdded message");
            }
        }

        fn drain_messages(&mut self) {
            loop {
                match self.receiver.try_recv() {
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(TryRecvError::Closed) => break,
                }
            }
        }
    }

    fn wait_for_message<F>(
        receiver: &mut Receiver<protocol::Message>,
        timeout: Duration,
        mut predicate: F,
    ) -> protocol::Message
    where
        F: FnMut(&protocol::Message) -> bool,
    {
        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                panic!("timed out waiting for expected message");
            }
            match receiver.try_recv() {
                Ok(message) => {
                    if predicate(&message) {
                        return message;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => panic!("bus closed while waiting for message"),
            }
        }
    }

    fn assert_no_message<F>(
        receiver: &mut Receiver<protocol::Message>,
        timeout: Duration,
        mut predicate: F,
    ) where
        F: FnMut(&protocol::Message) -> bool,
    {
        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                return;
            }
            match receiver.try_recv() {
                Ok(message) => {
                    if predicate(&message) {
                        panic!("received unexpected message: {:?}", message);
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => return,
            }
        }
    }

    #[test]
    fn test_play_pause_next_previous_event_sequence() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_play_0");
        let (id1, _) = harness.add_track("pm_play_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));

        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playback(protocol::PlaybackMessage::ClearPlayerCache)
            )
        });

        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .first()
                            .map(|track| track.id == id0 && track.play_immediately)
                            .unwrap_or(false)
                    }
                    _ => false,
                },
            );

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::ReadyForPlayback(id0.clone()),
        ));

        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackById(id))
                    if id == &id0
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::Pause,
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: false,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(protocol::PlaybackMessage::Play));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(protocol::PlaybackMessage::Next));
        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .first()
                            .map(|track| track.id == id1 && track.play_immediately)
                            .unwrap_or(false)
                    }
                    _ => false,
                },
            );

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::ReadyForPlayback(id1.clone()),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackById(id))
                    if id == &id1
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::Previous,
        ));
        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .first()
                            .map(|track| track.id == id0 && track.play_immediately)
                            .unwrap_or(false)
                    }
                    _ => false,
                },
            );
    }

    #[test]
    fn test_natural_transition_stops_after_last_track_when_repeat_off() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_transition_0");
        let (id1, _) = harness.add_track("pm_transition_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id0.clone()),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(1),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id1.clone()),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playback(protocol::PlaybackMessage::Stop)
            )
        });
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: false,
                    playing_index: None,
                    ..
                })
            )
        });
    }

    #[test]
    fn test_repeat_playlist_wraps_after_last_track() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_repeat_playlist_0");
        let (id1, _) = harness.add_track("pm_repeat_playlist_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::ToggleRepeat,
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::RepeatModeChanged(
                    protocol::RepeatMode::Playlist
                ))
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id0.clone()),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(1),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id1.clone()),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });
    }

    #[test]
    fn test_repeat_track_replays_same_track_on_finish() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_repeat_track_0");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::ToggleRepeat,
        ));
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::ToggleRepeat,
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::RepeatModeChanged(
                    protocol::RepeatMode::Track
                ))
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id0.clone()),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });
    }

    #[test]
    fn test_repeat_track_recaches_from_start_after_nonzero_offset_cache() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_repeat_seek_end_0");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        // Set Repeat=Track
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::ToggleRepeat,
        ));
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::ToggleRepeat,
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::RepeatModeChanged(
                    protocol::RepeatMode::Track
                ))
            )
        });

        // Simulate a seek-near-end cache state where current track is cached at non-zero offset.
        harness.send(protocol::Message::Audio(
            protocol::AudioMessage::TrackCached(id0.clone(), 98_000),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Audio(protocol::AudioMessage::TrackCached(id, offset))
                    if id == &id0 && *offset == 98_000
            )
        });

        harness.drain_messages();
        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id0.clone()),
        ));

        // Regression check: repeat-track must decode from start (offset 0), not assume
        // non-zero cached offset is reusable.
        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .iter()
                            .any(|track| track.id == id0 && track.start_offset_ms == 0)
                    }
                    _ => false,
                },
            );
    }

    #[test]
    fn test_ready_for_playback_does_not_restart_already_started_track() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_late_ready_0");
        let (id1, _) = harness.add_track("pm_late_ready_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(1),
                    ..
                })
            )
        });

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackStarted(protocol::TrackStarted {
                id: id1.clone(),
                start_offset_ms: 0,
            }),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted {
                    index: 1,
                    ..
                })
            )
        });

        harness.drain_messages();
        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::ReadyForPlayback(id1.clone()),
        ));

        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackById(id))
                        if id == &id1
                )
            },
        );
    }

    #[test]
    fn test_seek_restarts_decode_from_requested_offset() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_seek_0");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    ..
                })
            )
        });
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TechnicalMetadataChanged(protocol::TechnicalMetadata {
                format: "MP3".to_string(),
                bitrate_kbps: 320,
                sample_rate_hz: 44_100,
                duration_ms: 100_000,
            }),
        ));
        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::Seek(0.5),
        ));

        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Audio(protocol::AudioMessage::StopDecoding)
            )
        });

        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playback(protocol::PlaybackMessage::ClearPlayerCache)
            )
        });

        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks.iter().any(|track| {
                            track.id == id0
                                && track.play_immediately
                                && track.start_offset_ms == 50_000
                        })
                    }
                    _ => false,
                },
            );
    }

    #[test]
    fn test_track_evicted_invalidates_cached_entry() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_track_evicted_0");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));

        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playback(protocol::PlaybackMessage::ClearPlayerCache)
            )
        });
        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .first()
                            .map(|track| track.id == id0 && track.play_immediately)
                            .unwrap_or(false)
                    }
                    _ => false,
                },
            );

        harness.drain_messages();
        harness.send(protocol::Message::Audio(
            protocol::AudioMessage::TrackCached(id0.clone(), 0),
        ));
        harness.send(protocol::Message::Audio(
            protocol::AudioMessage::TrackEvicted(id0.clone()),
        ));
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));

        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .first()
                            .map(|track| track.id == id0 && track.play_immediately)
                            .unwrap_or(false)
                    }
                    _ => false,
                },
            );
    }
}
