//! Playlist-domain orchestrator.
//!
//! This component owns editable and playback playlist state, persists playlist
//! mutations, and coordinates decode/playback queueing behavior via the event bus.

use std::collections::{HashMap, HashSet};

use log::{debug, error, info, trace};
use tokio::sync::broadcast::{Receiver, Sender};
use uuid::Uuid;

use crate::{
    db_manager::DbManager,
    playlist::{Playlist, Track},
    protocol::{self, TrackIdentifier},
};

const TRACK_LIST_HISTORY_LIMIT: usize = 128;

#[derive(Clone)]
struct PlaylistTrackListSnapshot {
    tracks: Vec<Track>,
    selected_indices: Vec<usize>,
}

/// Coordinates playlist editing, playback sequencing, and decode cache intent.
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
    requested_track_offsets: HashMap<String, u64>, // id -> requested start_offset_ms
    pending_start_track_id: Option<String>,
    pending_order_change: Option<protocol::PlaybackOrder>,
    max_num_cached_tracks: usize,
    current_track_duration_ms: u64,
    last_seek_ms: u64,
    started_track_id: Option<String>,
    track_list_undo_stack: Vec<PlaylistTrackListSnapshot>,
    track_list_redo_stack: Vec<PlaylistTrackListSnapshot>,
}

impl PlaylistManager {
    /// Creates a playlist manager bound to bus channels and storage backend.
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
            requested_track_offsets: HashMap::new(),
            pending_start_track_id: None,
            pending_order_change: None,
            max_num_cached_tracks: 2,
            current_track_duration_ms: 0,
            last_seek_ms: u64::MAX,
            started_track_id: None,
            track_list_undo_stack: Vec::new(),
            track_list_redo_stack: Vec::new(),
        }
    }

    fn play_playback_track(&mut self, index: usize) {
        if index >= self.playback_playlist.num_tracks() {
            debug!("play_playback_track: Index {} out of bounds", index);
            return;
        }

        self.pending_start_track_id = None;
        self.started_track_id = None;
        self.playback_playlist.set_playing(true);
        self.playback_playlist.set_playing_track_index(Some(index));

        let track_id = self.playback_playlist.get_track_id(index);

        if self.cached_track_ids.get(&track_id) == Some(&0) {
            // Already cached at start, tell player to switch immediately
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::PlayTrackById(track_id),
            ));
            self.pending_start_track_id = Some(self.playback_playlist.get_track_id(index));
        } else {
            // Not cached or cached at wrong offset, start decoding process
            self.stop_decoding();
            self.cached_track_ids.clear();
            self.fully_cached_track_ids.clear();
            self.requested_track_offsets.clear();
            let _ = self.bus_producer.send(protocol::Message::Playback(
                protocol::PlaybackMessage::ClearPlayerCache,
            ));
            self.cache_tracks(true);
        }
        self.broadcast_playlist_changed();
    }

    fn broadcast_active_playlist_column_order(&self) {
        match self
            .db_manager
            .get_playlist_column_order(&self.active_playlist_id)
        {
            Ok(order) => {
                let _ = self.bus_producer.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::ActivePlaylistColumnOrder(order),
                ));
            }
            Err(err) => {
                error!(
                    "Failed to load playlist column order for {}: {}",
                    self.active_playlist_id, err
                );
            }
        }
    }

    fn broadcast_active_playlist_column_width_overrides(&self) {
        match self
            .db_manager
            .get_playlist_column_width_overrides(&self.active_playlist_id)
        {
            Ok(overrides) => {
                let _ = self.bus_producer.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::ActivePlaylistColumnWidthOverrides(overrides),
                ));
            }
            Err(err) => {
                error!(
                    "Failed to load playlist column width overrides for {}: {}",
                    self.active_playlist_id, err
                );
            }
        }
    }

    fn paste_insert_gap(selected_indices: &[usize], playlist_len: usize) -> usize {
        selected_indices
            .iter()
            .copied()
            .filter(|&index| index < playlist_len)
            .max()
            .map(|index| index.saturating_add(1))
            .unwrap_or(playlist_len)
    }

    fn capture_track_list_snapshot(&self) -> PlaylistTrackListSnapshot {
        PlaylistTrackListSnapshot {
            tracks: (0..self.editing_playlist.num_tracks())
                .map(|index| self.editing_playlist.get_track(index).clone())
                .collect(),
            selected_indices: self.editing_playlist.get_selected_indices(),
        }
    }

    fn track_list_changed(
        previous: &PlaylistTrackListSnapshot,
        current: &PlaylistTrackListSnapshot,
    ) -> bool {
        previous.tracks != current.tracks
    }

    fn push_track_list_history_snapshot(
        stack: &mut Vec<PlaylistTrackListSnapshot>,
        snapshot: PlaylistTrackListSnapshot,
    ) {
        stack.push(snapshot);
        if stack.len() > TRACK_LIST_HISTORY_LIMIT {
            let overflow = stack.len() - TRACK_LIST_HISTORY_LIMIT;
            stack.drain(0..overflow);
        }
    }

    fn push_track_list_undo_snapshot(&mut self, snapshot: PlaylistTrackListSnapshot) {
        Self::push_track_list_history_snapshot(&mut self.track_list_undo_stack, snapshot);
        self.track_list_redo_stack.clear();
    }

    fn clear_track_list_history(&mut self) {
        self.track_list_undo_stack.clear();
        self.track_list_redo_stack.clear();
    }

    fn restored_tracks_from_snapshot(
        snapshot: &PlaylistTrackListSnapshot,
    ) -> Vec<protocol::RestoredTrack> {
        snapshot
            .tracks
            .iter()
            .map(|track| protocol::RestoredTrack {
                id: track.id.clone(),
                path: track.path.clone(),
            })
            .collect()
    }

    fn build_playlist_from_snapshot(&self, snapshot: &PlaylistTrackListSnapshot) -> Playlist {
        let mut playlist = Playlist::new();
        playlist.set_playback_order(self.playback_order);
        playlist.set_repeat_mode(self.repeat_mode);
        for track in &snapshot.tracks {
            playlist.add_track(track.clone());
        }
        playlist.set_selected_indices(snapshot.selected_indices.clone());
        playlist
    }

    fn synchronize_active_playlist_tracks_in_db(&self, snapshot: &PlaylistTrackListSnapshot) {
        if self.active_playlist_id.is_empty() {
            return;
        }

        let persisted = match self
            .db_manager
            .get_tracks_for_playlist(&self.active_playlist_id)
        {
            Ok(tracks) => tracks,
            Err(err) => {
                error!(
                    "Failed to load tracks for snapshot persistence (playlist {}): {}",
                    self.active_playlist_id, err
                );
                return;
            }
        };

        let persisted_ids: HashSet<&str> =
            persisted.iter().map(|track| track.id.as_str()).collect();
        let snapshot_ids: HashSet<&str> = snapshot
            .tracks
            .iter()
            .map(|track| track.id.as_str())
            .collect();

        for removed in persisted
            .iter()
            .filter(|track| !snapshot_ids.contains(track.id.as_str()))
        {
            if let Err(err) = self.db_manager.delete_track(&removed.id) {
                error!(
                    "Failed to delete removed snapshot track {} from database: {}",
                    removed.id, err
                );
            }
        }

        for (position, track) in snapshot.tracks.iter().enumerate() {
            if persisted_ids.contains(track.id.as_str()) {
                continue;
            }
            if let Err(err) = self.db_manager.save_track(
                &track.id,
                &self.active_playlist_id,
                track.path.to_str().unwrap_or(""),
                position,
            ) {
                error!(
                    "Failed to insert snapshot track {} at position {}: {}",
                    track.id, position, err
                );
            }
        }

        let ordered_ids: Vec<String> = snapshot
            .tracks
            .iter()
            .map(|track| track.id.clone())
            .collect();
        if let Err(err) = self.db_manager.update_positions(ordered_ids) {
            error!(
                "Failed to update track positions for snapshot persistence: {}",
                err
            );
        }
    }

    fn apply_track_list_snapshot(&mut self, snapshot: PlaylistTrackListSnapshot) {
        let is_active_playback_playlist =
            Some(&self.active_playlist_id) == self.playback_playlist_id.as_ref();
        let was_playing_active_playlist =
            is_active_playback_playlist && self.playback_playlist.is_playing();
        let previous_playing_track_id = if let (true, Some(playing_index)) = (
            is_active_playback_playlist,
            self.playback_playlist.get_playing_track_index(),
        ) {
            if playing_index < self.playback_playlist.num_tracks() {
                Some(self.playback_playlist.get_track_id(playing_index))
            } else {
                None
            }
        } else {
            None
        };

        self.editing_playlist = self.build_playlist_from_snapshot(&snapshot);

        let mut lost_active_playing_track = false;
        if is_active_playback_playlist {
            let mut next_playback_playlist = self.build_playlist_from_snapshot(&snapshot);
            let playing_index = previous_playing_track_id.as_ref().and_then(|track_id| {
                snapshot
                    .tracks
                    .iter()
                    .position(|track| &track.id == track_id)
            });
            next_playback_playlist.set_playing_track_index(playing_index);
            next_playback_playlist
                .set_playing(was_playing_active_playlist && playing_index.is_some());
            lost_active_playing_track = was_playing_active_playlist && playing_index.is_none();
            self.playback_playlist = next_playback_playlist;
        }

        self.synchronize_active_playlist_tracks_in_db(&snapshot);

        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistRestored(Self::restored_tracks_from_snapshot(
                &snapshot,
            )),
        ));

        if lost_active_playing_track {
            self.clear_cached_tracks();
            let _ = self
                .bus_producer
                .send(protocol::Message::Playback(protocol::PlaybackMessage::Stop));
        } else if is_active_playback_playlist {
            self.cache_tracks(false);
        }

        self.broadcast_playlist_changed();
        self.broadcast_selection_changed();
    }

    /// Starts the blocking event loop for playlist messages and playback coordination.
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
            self.broadcast_active_playlist_column_order();
            self.broadcast_active_playlist_column_width_overrides();
        }

        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTrack(path)) => {
                        debug!("PlaylistManager: Loading track {:?}", path);
                        let previous_track_list = self.capture_track_list_snapshot();
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

                        if Self::track_list_changed(
                            &previous_track_list,
                            &self.capture_track_list_snapshot(),
                        ) {
                            self.push_track_list_undo_snapshot(previous_track_list);
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        debug!("PlaylistManager: Received play command");
                        let has_paused_track = !self.playback_playlist.is_playing()
                            && self.playback_playlist_id.is_some()
                            && self
                                .playback_playlist
                                .get_playing_track_index()
                                .map(|index| index < self.playback_playlist.num_tracks())
                                .unwrap_or(false);
                        if has_paused_track {
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
                            let index = self
                                .editing_playlist
                                .get_selected_indices()
                                .first()
                                .copied()
                                .unwrap_or(0);
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
                        self.pending_start_track_id = None;
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
                        let current_track_id = self
                            .playback_playlist
                            .get_playing_track_index()
                            .and_then(|idx| {
                                if idx < self.playback_playlist.num_tracks() {
                                    Some(self.playback_playlist.get_track_id(idx))
                                } else {
                                    None
                                }
                            });
                        if current_track_id.as_deref() != Some(id.as_str()) {
                            debug!(
                                "PlaylistManager: Ignoring stale TrackFinished for {} (current={:?})",
                                id, current_track_id
                            );
                            continue;
                        }
                        self.pending_start_track_id = None;
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
                        let current_track_id = self
                            .playback_playlist
                            .get_playing_track_index()
                            .and_then(|idx| {
                                if idx < self.playback_playlist.num_tracks() {
                                    Some(self.playback_playlist.get_track_id(idx))
                                } else {
                                    None
                                }
                            });
                        if current_track_id.as_deref() != Some(track_started.id.as_str()) {
                            debug!(
                                "PlaylistManager: Ignoring stale TrackStarted for {} (current={:?})",
                                track_started.id, current_track_id
                            );
                            continue;
                        }
                        self.last_seek_ms = u64::MAX;
                        self.pending_start_track_id = None;
                        self.started_track_id = Some(track_started.id.clone());
                        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index()
                        {
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::TrackStarted {
                                    index: playing_idx,
                                    playlist_id: self
                                        .playback_playlist_id
                                        .clone()
                                        .unwrap_or_default(),
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
                                        let already_requested_start =
                                            self.pending_start_track_id.as_deref()
                                                == Some(id.as_str());

                                        if already_started_current_track {
                                            debug!(
                                                "PlaylistManager: Ignoring stale ReadyForPlayback for already started track: {}",
                                                id
                                            );
                                        } else if already_requested_start {
                                            debug!(
                                                "PlaylistManager: Ignoring duplicate ReadyForPlayback start request for {}",
                                                id
                                            );
                                        } else {
                                            let _ = self.bus_producer.send(
                                                protocol::Message::Playback(
                                                    protocol::PlaybackMessage::PlayTrackById(id),
                                                ),
                                            );
                                            self.pending_start_track_id = Some(playing_track_id);
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
                        let previous_track_list = self.capture_track_list_snapshot();
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

                        if Self::track_list_changed(
                            &previous_track_list,
                            &self.capture_track_list_snapshot(),
                        ) {
                            self.push_track_list_undo_snapshot(previous_track_list);
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
                    protocol::Message::Playlist(protocol::PlaylistMessage::SelectionChanged(
                        indices,
                    )) => {
                        let previous_indices = self.editing_playlist.get_selected_indices();
                        self.editing_playlist.set_selected_indices(indices);
                        if self.editing_playlist.get_selected_indices() == previous_indices {
                            continue;
                        }
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
                        let previous_track_list = self.capture_track_list_snapshot();
                        self.editing_playlist.move_tracks(indices.clone(), to);

                        if Some(&self.active_playlist_id) == self.playback_playlist_id.as_ref() {
                            self.playback_playlist.move_tracks(indices, to);
                        }

                        if !Self::track_list_changed(
                            &previous_track_list,
                            &self.capture_track_list_snapshot(),
                        ) {
                            continue;
                        }

                        // Update positions in database
                        let all_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|i| self.editing_playlist.get_track_id(i))
                            .collect();
                        if let Err(e) = self.db_manager.update_positions(all_ids) {
                            error!("Failed to update positions in database: {}", e);
                        }

                        self.push_track_list_undo_snapshot(previous_track_list);

                        // Notify other components about the index shift
                        self.broadcast_playlist_changed();

                        // Re-cache to ensure next tracks are correct
                        self.cache_tracks(false);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::PasteTracks(paths)) => {
                        if paths.is_empty() {
                            continue;
                        }
                        let previous_track_list = self.capture_track_list_snapshot();

                        let playlist_len = self.editing_playlist.num_tracks();
                        let insert_gap = Self::paste_insert_gap(
                            &self.editing_playlist.get_selected_indices(),
                            playlist_len,
                        );
                        let is_active_playback_playlist =
                            Some(&self.active_playlist_id) == self.playback_playlist_id.as_ref();

                        let mut appended_indices = Vec::with_capacity(paths.len());
                        let mut inserted_tracks = Vec::with_capacity(paths.len());

                        for path in paths {
                            let id = Uuid::new_v4().to_string();
                            let append_index = self.editing_playlist.num_tracks();
                            if let Err(err) = self.db_manager.save_track(
                                &id,
                                &self.active_playlist_id,
                                path.to_str().unwrap_or(""),
                                append_index,
                            ) {
                                error!("Failed to save pasted track to database: {}", err);
                                continue;
                            }

                            let track = Track {
                                path: path.clone(),
                                id: id.clone(),
                            };
                            self.editing_playlist.add_track(track.clone());
                            if is_active_playback_playlist {
                                self.playback_playlist.add_track(track);
                            }

                            appended_indices.push(append_index);
                            inserted_tracks.push(protocol::RestoredTrack { id, path });
                        }

                        if appended_indices.is_empty() {
                            continue;
                        }

                        self.editing_playlist
                            .move_tracks(appended_indices.clone(), insert_gap);
                        if is_active_playback_playlist {
                            self.playback_playlist
                                .move_tracks(appended_indices, insert_gap);
                        }

                        let insert_at = self
                            .editing_playlist
                            .get_selected_indices()
                            .first()
                            .copied()
                            .unwrap_or_else(|| insert_gap.min(self.editing_playlist.num_tracks()));

                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::TracksInserted {
                                tracks: inserted_tracks,
                                insert_at,
                            },
                        ));

                        let all_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|i| self.editing_playlist.get_track_id(i))
                            .collect();
                        if let Err(err) = self.db_manager.update_positions(all_ids) {
                            error!("Failed to update positions in database: {}", err);
                        }

                        if Self::track_list_changed(
                            &previous_track_list,
                            &self.capture_track_list_snapshot(),
                        ) {
                            self.push_track_list_undo_snapshot(previous_track_list);
                        }

                        self.broadcast_playlist_changed();
                        self.broadcast_selection_changed();

                        if is_active_playback_playlist {
                            self.cache_tracks(false);
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::UndoTrackListEdit) => {
                        let Some(previous_snapshot) = self.track_list_undo_stack.pop() else {
                            continue;
                        };
                        let current_snapshot = self.capture_track_list_snapshot();
                        Self::push_track_list_history_snapshot(
                            &mut self.track_list_redo_stack,
                            current_snapshot,
                        );
                        self.apply_track_list_snapshot(previous_snapshot);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::RedoTrackListEdit) => {
                        let Some(next_snapshot) = self.track_list_redo_stack.pop() else {
                            continue;
                        };
                        let current_snapshot = self.capture_track_list_snapshot();
                        Self::push_track_list_history_snapshot(
                            &mut self.track_list_undo_stack,
                            current_snapshot,
                        );
                        self.apply_track_list_snapshot(next_snapshot);
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::ApplyFilterViewSnapshot(source_indices),
                    ) => {
                        debug!(
                            "PlaylistManager: Applying filter view snapshot with {} entries",
                            source_indices.len()
                        );
                        let previous_track_list = self.capture_track_list_snapshot();
                        let old_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|index| self.editing_playlist.get_track_id(index))
                            .collect();
                        let was_playing_active_playlist = Some(&self.active_playlist_id)
                            == self.playback_playlist_id.as_ref()
                            && self.playback_playlist.is_playing();

                        self.editing_playlist
                            .apply_filter_view_snapshot(source_indices.clone());
                        if Some(&self.active_playlist_id) == self.playback_playlist_id.as_ref() {
                            self.playback_playlist
                                .apply_filter_view_snapshot(source_indices);
                        }

                        let new_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|index| self.editing_playlist.get_track_id(index))
                            .collect();
                        let new_id_set: HashSet<String> = new_ids.iter().cloned().collect();
                        for removed_id in old_ids
                            .into_iter()
                            .filter(|old_id| !new_id_set.contains(old_id))
                        {
                            if let Err(err) = self.db_manager.delete_track(&removed_id) {
                                error!("Failed to delete track from database: {}", err);
                            }
                        }
                        if let Err(err) = self.db_manager.update_positions(new_ids) {
                            error!("Failed to update positions in database: {}", err);
                        }

                        if Self::track_list_changed(
                            &previous_track_list,
                            &self.capture_track_list_snapshot(),
                        ) {
                            self.push_track_list_undo_snapshot(previous_track_list);
                        }

                        if was_playing_active_playlist && !self.playback_playlist.is_playing() {
                            let _ = self
                                .bus_producer
                                .send(protocol::Message::Playback(protocol::PlaybackMessage::Stop));
                        } else if Some(&self.active_playlist_id)
                            == self.playback_playlist_id.as_ref()
                        {
                            self.cache_tracks(false);
                        }

                        self.broadcast_playlist_changed();
                        self.broadcast_selection_changed();
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

                        self.clear_track_list_history();
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
                        self.broadcast_active_playlist_column_order();
                        self.broadcast_active_playlist_column_width_overrides();
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::SetActivePlaylistColumnOrder(column_order),
                    ) => {
                        if self.active_playlist_id.is_empty() {
                            continue;
                        }
                        if let Err(err) = self
                            .db_manager
                            .set_playlist_column_order(&self.active_playlist_id, &column_order)
                        {
                            error!(
                                "Failed to persist playlist column order for {}: {}",
                                self.active_playlist_id, err
                            );
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::RequestActivePlaylistColumnOrder,
                    ) => {
                        if self.active_playlist_id.is_empty() {
                            continue;
                        }
                        self.broadcast_active_playlist_column_order();
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                            column_key,
                            width_px,
                            persist,
                        },
                    ) => {
                        if !persist || self.active_playlist_id.is_empty() {
                            continue;
                        }
                        if let Err(err) = self.db_manager.set_playlist_column_width_override(
                            &self.active_playlist_id,
                            &column_key,
                            width_px.max(48),
                        ) {
                            error!(
                                "Failed to persist playlist column width override for {} key {}: {}",
                                self.active_playlist_id, column_key, err
                            );
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::ClearActivePlaylistColumnWidthOverride {
                            column_key,
                            persist,
                        },
                    ) => {
                        if !persist || self.active_playlist_id.is_empty() {
                            continue;
                        }
                        if let Err(err) = self.db_manager.clear_playlist_column_width_override(
                            &self.active_playlist_id,
                            &column_key,
                        ) {
                            error!(
                                "Failed to clear playlist column width override for {} key {}: {}",
                                self.active_playlist_id, column_key, err
                            );
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::RequestActivePlaylistColumnWidthOverrides,
                    ) => {
                        if self.active_playlist_id.is_empty() {
                            continue;
                        }
                        self.broadcast_active_playlist_column_width_overrides();
                    }
                    protocol::Message::Audio(protocol::AudioMessage::TrackCached(id, offset)) => {
                        debug!(
                            "PlaylistManager: Received TrackCached: {} at offset {}",
                            id, offset
                        );
                        self.requested_track_offsets.remove(&id);
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
                        self.requested_track_offsets.remove(&id);
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
                                self.pending_start_track_id = None;
                                self.started_track_id = None;
                                self.requested_track_offsets.clear();

                                // 1. Stop current decoding
                                self.stop_decoding();

                                // 2. Clear player cache
                                let _ = self.bus_producer.send(protocol::Message::Playback(
                                    protocol::PlaybackMessage::ClearPlayerCache,
                                ));

                                // 3. Restart decoding at offset
                                let _ = self.bus_producer.send(protocol::Message::Audio(
                                    protocol::AudioMessage::DecodeTracks(vec![TrackIdentifier {
                                        id: track_id.clone(),
                                        path: track_path,
                                        play_immediately: true,
                                        start_offset_ms: target_ms,
                                    }]),
                                ));
                                self.requested_track_offsets.insert(track_id, target_ms);
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
                let is_requested_at_track_start =
                    self.requested_track_offsets.get(&track_id).copied() == Some(0);
                if is_cached_at_track_start || is_requested_at_track_start {
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
        if !track_paths.is_empty() {
            for track in &track_paths {
                self.requested_track_offsets
                    .insert(track.id.clone(), track.start_offset_ms);
            }
            let _ = self.bus_producer.send(protocol::Message::Audio(
                protocol::AudioMessage::DecodeTracks(track_paths),
            ));
        }
    }

    fn clear_cached_tracks(&mut self) {
        self.pending_start_track_id = None;
        self.started_track_id = None;
        self.cached_track_ids.clear();
        self.fully_cached_track_ids.clear();
        self.requested_track_offsets.clear();
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::StopDecoding,
        ));
        let _ = self.bus_producer.send(protocol::Message::Playback(
            protocol::PlaybackMessage::ClearPlayerCache,
        ));
    }

    fn stop_decoding(&mut self) {
        self.pending_start_track_id = None;
        self.requested_track_offsets.clear();
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::StopDecoding,
        ));
    }

    fn broadcast_playlist_changed(&self) {
        let mut playing_track_path = None;

        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index() {
            if playing_idx < self.playback_playlist.num_tracks() {
                let track = self.playback_playlist.get_track(playing_idx);
                playing_track_path = Some(track.path.clone());
            }
        }

        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistIndicesChanged {
                playing_playlist_id: self.playback_playlist_id.clone(),
                playing_index: self.playback_playlist.get_playing_track_index(),
                playing_track_path,
                playing_track_metadata: None,
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

    fn wait_for_playlist_restored_track_ids(
        receiver: &mut Receiver<protocol::Message>,
        timeout: Duration,
    ) -> Vec<String> {
        let message = wait_for_message(receiver, timeout, |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(_))
            )
        });
        if let protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(tracks)) =
            message
        {
            tracks.into_iter().map(|track| track.id).collect()
        } else {
            panic!("expected PlaylistRestored message");
        }
    }

    #[test]
    fn test_paste_insert_gap_uses_end_of_current_selection() {
        assert_eq!(PlaylistManager::paste_insert_gap(&[], 4), 4);
        assert_eq!(PlaylistManager::paste_insert_gap(&[0, 2, 1], 5), 3);
        assert_eq!(PlaylistManager::paste_insert_gap(&[3], 4), 4);
        assert_eq!(PlaylistManager::paste_insert_gap(&[7, 8], 4), 4);
    }

    #[test]
    fn test_track_list_undo_and_redo_restore_reordered_playlist() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_undo_reorder_0");
        let (id1, _) = harness.add_track("pm_undo_reorder_1");
        let (id2, _) = harness.add_track("pm_undo_reorder_2");
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::ReorderTracks {
                indices: vec![2],
                to: 0,
            },
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(
                    protocol::PlaylistMessage::PlaylistIndicesChanged { .. }
                )
            )
        });
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::UndoTrackListEdit,
        ));
        let undone_order =
            wait_for_playlist_restored_track_ids(&mut harness.receiver, Duration::from_secs(1));
        assert_eq!(undone_order, vec![id0.clone(), id1.clone(), id2.clone()]);
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::RedoTrackListEdit,
        ));
        let redone_order =
            wait_for_playlist_restored_track_ids(&mut harness.receiver, Duration::from_secs(1));
        assert_eq!(redone_order, vec![id2, id0, id1]);
    }

    #[test]
    fn test_switch_playlist_clears_track_list_undo_history() {
        let mut harness = PlaylistManagerHarness::new();
        let (_id0, _) = harness.add_track("pm_undo_switch_0");
        let (_id1, _) = harness.add_track("pm_undo_switch_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::DeleteTracks(vec![1]),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(
                    protocol::PlaylistMessage::PlaylistIndicesChanged { .. }
                )
            )
        });
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::CreatePlaylist {
                name: "Undo Scope Target".to_string(),
            },
        ));

        let playlists =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistsRestored(list))
                        if list.iter().any(|playlist| playlist.name == "Undo Scope Target")
                )
            });
        let target_playlist_id = if let protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistsRestored(list),
        ) = playlists
        {
            list.into_iter()
                .find(|playlist| playlist.name == "Undo Scope Target")
                .map(|playlist| playlist.id)
                .expect("created playlist should be present in restoration list")
        } else {
            panic!("expected PlaylistsRestored message");
        };

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::SwitchPlaylist {
                id: target_playlist_id.clone(),
            },
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::ActivePlaylistChanged(id))
                    if id == &target_playlist_id
            )
        });
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::UndoTrackListEdit,
        ));
        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(_))
                )
            },
        );
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
    fn test_play_resumes_paused_track_after_deselect_all() {
        let mut harness = PlaylistManagerHarness::new();
        let (_id0, _) = harness.add_track("pm_resume_deselect_0");
        let (_id1, _) = harness.add_track("pm_resume_deselect_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(1),
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
            protocol::PlaybackMessage::Pause,
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: false,
                    playing_index: Some(1),
                    ..
                })
            )
        });

        harness.drain_messages();
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::DeselectAll,
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::SelectionChanged(indices))
                    if indices.is_empty()
            )
        });

        harness.drain_messages();
        harness.send(protocol::Message::Playback(protocol::PlaybackMessage::Play));

        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for resumed playback message");
            }
            match harness.receiver.try_recv() {
                Ok(protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks))) => {
                    panic!(
                        "unexpected decode request while resuming paused playback: {:?}",
                        tracks
                    );
                }
                Ok(protocol::Message::Playlist(
                    protocol::PlaylistMessage::PlaylistIndicesChanged {
                        is_playing: true,
                        playing_index: Some(1),
                        ..
                    },
                )) => break,
                Ok(_) => {}
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => {
                    panic!("bus closed while waiting for resumed playback message")
                }
            }
        }
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
    fn test_natural_transition_does_not_requeue_already_requested_next_track() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_transition_dedup_0");
        let (id1, _) = harness.add_track("pm_transition_dedup_1");
        harness.drain_messages();

        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::PlayTrackByIndex(0),
        ));

        // Initial decode request should include current + next track.
        let _ =
            wait_for_message(
                &mut harness.receiver,
                Duration::from_secs(1),
                |message| match message {
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => {
                        tracks
                            .iter()
                            .any(|track| track.id == id0 && track.play_immediately)
                            && tracks
                                .iter()
                                .any(|track| track.id == id1 && !track.play_immediately)
                    }
                    _ => false,
                },
            );

        harness.drain_messages();
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

        // Regression check: the next track was already requested in the initial prefetch
        // and must not be requested again on natural transition.
        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| match message {
                protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) => tracks
                    .iter()
                    .any(|track| track.id == id1 && track.start_offset_ms == 0),
                _ => false,
            },
        );
    }

    #[test]
    fn test_ready_for_playback_for_non_current_track_is_ignored() {
        let mut harness = PlaylistManagerHarness::new();
        let (_id0, _) = harness.add_track("pm_ready_non_current_0");
        let (id1, _) = harness.add_track("pm_ready_non_current_1");
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
    fn test_duplicate_ready_for_playback_before_start_emits_single_play_request() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_dup_ready_0");
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
            protocol::PlaybackMessage::ReadyForPlayback(id0.clone()),
        ));
        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackById(id))
                        if id == &id0
                )
            },
        );
    }

    #[test]
    fn test_stale_track_finished_is_ignored() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_stale_finish_0");
        let (_id1, _) = harness.add_track("pm_stale_finish_1");
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

        harness.drain_messages();
        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackFinished(id0),
        ));

        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop)
                        | protocol::Message::Playlist(protocol::PlaylistMessage::TrackFinished)
                )
            },
        );
    }

    #[test]
    fn test_stale_track_started_is_ignored() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_stale_started_0");
        let (_id1, _) = harness.add_track("pm_stale_started_1");
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

        harness.drain_messages();
        harness.send(protocol::Message::Playback(
            protocol::PlaybackMessage::TrackStarted(protocol::TrackStarted {
                id: id0,
                start_offset_ms: 0,
            }),
        ));
        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted { .. })
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
