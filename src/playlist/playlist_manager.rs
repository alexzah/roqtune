//! Playlist-domain orchestrator.
//!
//! This component owns editable and playback playlist state, persists playlist
//! mutations, and coordinates decode/playback queueing behavior via the event bus.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver as StdReceiver;

use log::{debug, error, info, trace};
use symphonia::core::{
    formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint,
};
use tokio::sync::broadcast::{Receiver, Sender};
use uuid::Uuid;

use crate::{
    config::{Config, UiPlaybackOrder, UiRepeatMode},
    db_manager::DbManager,
    integration_uri::{is_remote_track_path, parse_opensubsonic_track_uri},
    playlist::{Playlist, Track},
    protocol::{self, TrackIdentifier},
};

const TRACK_LIST_HISTORY_LIMIT: usize = 128;

#[derive(Clone)]
struct PlaylistTrackListSnapshot {
    tracks: Vec<Track>,
    selected_indices: Vec<usize>,
}

#[derive(Clone)]
struct PendingMixedDetach {
    playlist_id: String,
    pending_paths: Vec<PathBuf>,
}

/// Coordinates playlist editing, playback sequencing, and decode cache intent.
pub struct PlaylistManager {
    editing_playlist: Playlist,
    active_playlist_id: String,
    playback_playlist: Playlist,
    playback_queue_source: Option<protocol::PlaybackQueueSource>,
    playback_route: protocol::PlaybackRoute,
    playback_order: protocol::PlaybackOrder,
    repeat_mode: protocol::RepeatMode,
    bus_consumer: Receiver<protocol::Message>,
    bus_producer: Sender<protocol::Message>,
    db_manager: DbManager,
    bulk_import_rx: StdReceiver<protocol::PlaylistBulkImportRequest>,
    cached_track_ids: HashMap<String, u64>, // id -> start_offset_ms
    fully_cached_track_ids: HashSet<String>,
    requested_track_offsets: HashMap<String, u64>, // id -> requested start_offset_ms
    pending_start_track_id: Option<String>,
    pending_order_change: Option<protocol::PlaybackOrder>,
    track_sample_rate_cache: HashMap<PathBuf, Option<u32>>,
    pending_rate_switch: Option<u32>,
    pending_rate_switch_play_immediately: bool,
    current_output_rate_hz: Option<u32>,
    verified_output_rates: Vec<u32>,
    sample_rate_auto_enabled: bool,
    max_num_cached_tracks: usize,
    current_track_duration_ms: u64,
    current_elapsed_ms: u64,
    last_seek_ms: u64,
    started_track_id: Option<String>,
    track_list_undo_stack: Vec<PlaylistTrackListSnapshot>,
    track_list_redo_stack: Vec<PlaylistTrackListSnapshot>,
    playback_preferences_restored_from_config: bool,
    pending_mixed_detach: Option<PendingMixedDetach>,
    suppress_remote_writeback: bool,
    last_remote_writeback_signature: HashMap<String, String>,
    remote_track_metadata_by_path: HashMap<PathBuf, protocol::TrackMetadataSummary>,
    backend_connection_states: HashMap<String, protocol::BackendConnectionState>,
    unavailable_track_ids: HashSet<String>,
}

impl PlaylistManager {
    /// Creates a playlist manager bound to bus channels and storage backend.
    pub fn new(
        playlist: Playlist,
        bus_consumer: Receiver<protocol::Message>,
        bus_producer: Sender<protocol::Message>,
        db_manager: DbManager,
        bulk_import_rx: StdReceiver<protocol::PlaylistBulkImportRequest>,
    ) -> Self {
        Self {
            editing_playlist: playlist.clone(),
            active_playlist_id: String::new(),
            playback_playlist: playlist,
            playback_queue_source: None,
            playback_route: protocol::PlaybackRoute::Local,
            playback_order: protocol::PlaybackOrder::Default,
            repeat_mode: protocol::RepeatMode::Off,
            bus_consumer,
            bus_producer,
            db_manager,
            bulk_import_rx,
            cached_track_ids: HashMap::new(),
            fully_cached_track_ids: HashSet::new(),
            requested_track_offsets: HashMap::new(),
            pending_start_track_id: None,
            pending_order_change: None,
            track_sample_rate_cache: HashMap::new(),
            pending_rate_switch: None,
            pending_rate_switch_play_immediately: false,
            current_output_rate_hz: None,
            verified_output_rates: Vec::new(),
            sample_rate_auto_enabled: true,
            max_num_cached_tracks: 2,
            current_track_duration_ms: 0,
            current_elapsed_ms: 0,
            last_seek_ms: u64::MAX,
            started_track_id: None,
            track_list_undo_stack: Vec::new(),
            track_list_redo_stack: Vec::new(),
            playback_preferences_restored_from_config: false,
            pending_mixed_detach: None,
            suppress_remote_writeback: false,
            last_remote_writeback_signature: HashMap::new(),
            remote_track_metadata_by_path: HashMap::new(),
            backend_connection_states: HashMap::new(),
            unavailable_track_ids: HashSet::new(),
        }
    }

    fn playback_playlist_id(&self) -> Option<String> {
        match self.playback_queue_source.as_ref() {
            Some(protocol::PlaybackQueueSource::Playlist { playlist_id }) => {
                Some(playlist_id.clone())
            }
            Some(protocol::PlaybackQueueSource::Library) | None => None,
        }
    }

    fn remote_binding_from_playlist_id(playlist_id: &str) -> Option<(String, String)> {
        let prefix = "remote:opensubsonic:";
        let suffix = playlist_id.strip_prefix(prefix)?;
        let (profile_id, remote_playlist_id) = suffix.split_once(':')?;
        if profile_id.trim().is_empty() || remote_playlist_id.trim().is_empty() {
            return None;
        }
        Some((profile_id.to_string(), remote_playlist_id.to_string()))
    }

    fn remote_song_ids_if_pure_playlist(&self, playlist_id: &str) -> Option<Vec<String>> {
        let (profile_id, _) = Self::remote_binding_from_playlist_id(playlist_id)?;
        let mut song_ids = Vec::new();
        for index in 0..self.editing_playlist.num_tracks() {
            let track = self.editing_playlist.get_track(index);
            let locator = parse_opensubsonic_track_uri(track.path.as_path())?;
            if locator.profile_id != profile_id {
                return None;
            }
            song_ids.push(locator.song_id);
        }
        Some(song_ids)
    }

    fn opensubsonic_sync_candidate_for_playlist(
        &self,
        playlist_id: &str,
    ) -> Option<(String, Vec<String>)> {
        if Self::remote_binding_from_playlist_id(playlist_id).is_some() {
            return None;
        }
        let tracks = self.db_manager.get_tracks_for_playlist(playlist_id).ok()?;
        if tracks.is_empty() {
            return None;
        }
        let mut profile_id: Option<String> = None;
        let mut song_ids = Vec::with_capacity(tracks.len());
        for track in tracks {
            let locator = parse_opensubsonic_track_uri(track.path.as_path())?;
            if let Some(existing_profile_id) = profile_id.as_ref() {
                if existing_profile_id != &locator.profile_id {
                    return None;
                }
            } else {
                profile_id = Some(locator.profile_id.clone());
            }
            song_ids.push(locator.song_id);
        }
        Some((profile_id?, song_ids))
    }

    fn compute_opensubsonic_sync_eligible_playlist_ids(
        &self,
        playlists: &[protocol::PlaylistInfo],
    ) -> Vec<String> {
        playlists
            .iter()
            .filter_map(|playlist| {
                self.opensubsonic_sync_candidate_for_playlist(&playlist.id)
                    .map(|_| playlist.id.clone())
            })
            .collect()
    }

    fn emit_opensubsonic_sync_eligible_playlists(&self, playlists: &[protocol::PlaylistInfo]) {
        let playlist_ids = self.compute_opensubsonic_sync_eligible_playlist_ids(playlists);
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::OpenSubsonicSyncEligiblePlaylists(playlist_ids),
        ));
    }

    fn playlist_name_by_id(&self, playlist_id: &str) -> Option<String> {
        self.db_manager
            .get_all_playlists()
            .ok()
            .and_then(|playlists| {
                playlists
                    .into_iter()
                    .find(|playlist| playlist.id == playlist_id)
                    .map(|playlist| playlist.name)
            })
    }

    fn emit_remote_writeback_state(
        &self,
        playlist_id: String,
        success: bool,
        error: Option<String>,
    ) {
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::RemotePlaylistWritebackState {
                playlist_id,
                success,
                error,
            },
        ));
    }

    fn emit_track_unavailable_metadata(&self, track_id: &str) {
        let summary = protocol::TrackMetadataSummary {
            title: "Remote track unavailable".to_string(),
            artist: String::new(),
            album: String::new(),
            album_artist: String::new(),
            date: String::new(),
            genre: String::new(),
            year: String::new(),
            track_number: String::new(),
        };
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::TrackMetadataBatchUpdated {
                updates: vec![protocol::TrackMetadataPatch {
                    track_id: track_id.to_string(),
                    summary,
                }],
            },
        ));
    }

    fn reconcile_editing_playlist_remote_availability(&mut self) {
        let mut remote_track_ids: HashSet<String> = HashSet::new();
        let mut remote_unavailable_ids: HashSet<String> = HashSet::new();
        let mut unavailable_reasons: HashMap<String, String> = HashMap::new();

        for index in 0..self.editing_playlist.num_tracks() {
            let track = self.editing_playlist.get_track(index);
            let Some(locator) = parse_opensubsonic_track_uri(track.path.as_path()) else {
                continue;
            };
            remote_track_ids.insert(track.id.clone());
            let connected = matches!(
                self.backend_connection_states.get(&locator.profile_id),
                Some(protocol::BackendConnectionState::Connected)
            );
            if !connected {
                remote_unavailable_ids.insert(track.id.clone());
                unavailable_reasons.insert(
                    track.id.clone(),
                    format!(
                        "OpenSubsonic profile '{}' is disabled or disconnected",
                        locator.profile_id
                    ),
                );
            }
        }

        for track_id in remote_track_ids {
            if remote_unavailable_ids.contains(&track_id) {
                self.unavailable_track_ids.insert(track_id.clone());
                self.emit_track_unavailable_metadata(&track_id);
                let reason = unavailable_reasons
                    .get(&track_id)
                    .cloned()
                    .unwrap_or_else(|| "Remote track unavailable".to_string());
                let _ = self.bus_producer.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::TrackUnavailable {
                        id: track_id,
                        reason,
                    },
                ));
            } else {
                self.unavailable_track_ids.remove(&track_id);
            }
        }
    }

    fn find_playable_index_from(&mut self, start_index: usize, forward: bool) -> Option<usize> {
        if self.playback_playlist.num_tracks() == 0
            || start_index >= self.playback_playlist.num_tracks()
        {
            return None;
        }
        let mut cursor = start_index;
        let mut visited: HashSet<usize> = HashSet::new();
        loop {
            if !visited.insert(cursor) {
                return None;
            }
            let track_id = self.playback_playlist.get_track_id(cursor);
            if !self.unavailable_track_ids.contains(&track_id) {
                return Some(cursor);
            }
            let next = if forward {
                self.playback_playlist.get_next_track_index(cursor)
            } else {
                self.playback_playlist.get_previous_track_index(cursor)
            };
            let next = next?;
            if next >= self.playback_playlist.num_tracks() {
                return None;
            }
            cursor = next;
        }
    }

    fn stop_playback_after_unavailable(&mut self) {
        self.pending_start_track_id = None;
        self.started_track_id = None;
        self.playback_playlist.set_playing(false);
        self.playback_playlist.set_playing_track_index(None);
        let _ = self
            .bus_producer
            .send(protocol::Message::Playback(protocol::PlaybackMessage::Stop));
        self.broadcast_playlist_changed();
    }

    fn handle_track_unavailable(&mut self, track_id: String, reason: String) {
        if !self.unavailable_track_ids.insert(track_id.clone()) {
            return;
        }
        info!(
            "PlaylistManager: Marking track unavailable id={} reason={}",
            track_id, reason
        );
        self.emit_track_unavailable_metadata(&track_id);

        let current_track_id = self
            .playback_playlist
            .get_playing_track_index()
            .and_then(|index| {
                (index < self.playback_playlist.num_tracks())
                    .then(|| self.playback_playlist.get_track_id(index))
            });
        if current_track_id.as_deref() != Some(track_id.as_str()) {
            return;
        }
        let Some(current_index) = self.playback_playlist.get_playing_track_index() else {
            return;
        };
        let next_index = self
            .playback_playlist
            .get_next_track_index(current_index)
            .and_then(|index| self.find_playable_index_from(index, true));
        if let Some(next_index) = next_index {
            self.play_playback_track(next_index, true);
            return;
        }
        self.stop_playback_after_unavailable();
    }

    fn request_opensubsonic_sync_for_playlist(&mut self, playlist_id: &str) {
        let Some((profile_id, song_ids)) =
            self.opensubsonic_sync_candidate_for_playlist(playlist_id)
        else {
            self.emit_remote_writeback_state(
                playlist_id.to_string(),
                false,
                Some(
                    "Playlist is not eligible for OpenSubsonic sync (requires only OpenSubsonic tracks from one profile)"
                        .to_string(),
                ),
            );
            return;
        };
        let Some(name) = self.playlist_name_by_id(playlist_id) else {
            self.emit_remote_writeback_state(
                playlist_id.to_string(),
                false,
                Some("Playlist could not be found".to_string()),
            );
            return;
        };
        let _ = self.bus_producer.send(protocol::Message::Integration(
            protocol::IntegrationMessage::CreateOpenSubsonicPlaylistFromLocal {
                profile_id,
                local_playlist_id: playlist_id.to_string(),
                name,
                track_song_ids: song_ids,
            },
        ));
    }

    fn should_warn_before_mixed_insert(&self, paths: &[PathBuf]) -> bool {
        let Some((profile_id, _)) = Self::remote_binding_from_playlist_id(&self.active_playlist_id)
        else {
            return false;
        };
        if paths.is_empty() {
            return false;
        }
        for path in paths {
            let Some(locator) = parse_opensubsonic_track_uri(path.as_path()) else {
                return true;
            };
            if locator.profile_id != profile_id {
                return true;
            }
        }
        false
    }

    fn request_mixed_detach_confirmation(&mut self, paths: Vec<PathBuf>) {
        let playlist_id = self.active_playlist_id.clone();
        let playlist_name = self
            .db_manager
            .get_all_playlists()
            .ok()
            .and_then(|playlists| {
                playlists
                    .into_iter()
                    .find(|playlist| playlist.id == playlist_id)
                    .map(|playlist| playlist.name)
            })
            .unwrap_or_else(|| "Remote Playlist".to_string());
        self.pending_mixed_detach = Some(PendingMixedDetach {
            playlist_id: playlist_id.clone(),
            pending_paths: paths,
        });
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::RemoteDetachConfirmationRequested {
                playlist_id,
                playlist_name,
            },
        ));
    }

    fn reload_editing_playlist_from_active(&mut self) {
        self.editing_playlist = Playlist::new();
        self.editing_playlist
            .set_playback_order(self.playback_order);
        self.editing_playlist.set_repeat_mode(self.repeat_mode);
        if let Ok(tracks) = self
            .db_manager
            .get_tracks_for_playlist(&self.active_playlist_id)
        {
            for track in tracks {
                self.editing_playlist.add_track(Track {
                    path: track.path,
                    id: track.id,
                });
            }
        }
    }

    fn remove_remote_metadata_for_profile(&mut self, profile_id: &str) {
        self.remote_track_metadata_by_path.retain(|path, _| {
            parse_opensubsonic_track_uri(path.as_path())
                .map(|locator| locator.profile_id != profile_id)
                .unwrap_or(true)
        });
    }

    fn is_remote_playlist_blocked(&self, playlist_id: &str) -> bool {
        let Some((profile_id, _)) = Self::remote_binding_from_playlist_id(playlist_id) else {
            return false;
        };
        matches!(
            self.backend_connection_states.get(&profile_id),
            Some(protocol::BackendConnectionState::Error)
        )
    }

    fn detach_active_playlist_binding(&mut self) {
        let Some((profile_id, remote_playlist_id)) =
            Self::remote_binding_from_playlist_id(&self.active_playlist_id)
        else {
            return;
        };
        let detached_id = format!("local:detached:{}:{}", profile_id, remote_playlist_id);
        let detached_name = self
            .db_manager
            .get_all_playlists()
            .ok()
            .and_then(|playlists| {
                playlists
                    .into_iter()
                    .find(|playlist| playlist.id == self.active_playlist_id)
                    .map(|playlist| playlist.name)
            })
            .unwrap_or_else(|| "Detached Playlist".to_string());
        let existing_tracks = self
            .db_manager
            .get_tracks_for_playlist(&self.active_playlist_id)
            .unwrap_or_default();
        if self
            .db_manager
            .create_playlist(&detached_id, &detached_name)
            .is_ok()
        {
            let pending: Vec<(String, PathBuf)> = existing_tracks
                .into_iter()
                .map(|track| (Uuid::new_v4().to_string(), track.path))
                .collect();
            if !pending.is_empty() {
                let _ = self.db_manager.save_tracks_batch(&detached_id, &pending, 0);
            }
            let _ = self.db_manager.delete_playlist(&self.active_playlist_id);
            self.active_playlist_id = detached_id;
            self.editing_playlist = Playlist::new();
            self.editing_playlist
                .set_playback_order(self.playback_order);
            self.editing_playlist.set_repeat_mode(self.repeat_mode);
            if let Ok(restored) = self
                .db_manager
                .get_tracks_for_playlist(&self.active_playlist_id)
            {
                for track in restored {
                    self.editing_playlist.add_track(Track {
                        path: track.path,
                        id: track.id,
                    });
                }
            }
            let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
            self.broadcast_playlist_state_snapshot(playlists);
            self.broadcast_selection_changed();
        }
    }

    fn promote_local_playlist_to_remote_binding(
        &mut self,
        local_playlist_id: &str,
        profile_id: &str,
        remote_playlist_id: &str,
    ) -> Result<String, String> {
        let remote_bound_playlist_id =
            format!("remote:opensubsonic:{}:{}", profile_id, remote_playlist_id);
        let playlist_name = self
            .playlist_name_by_id(local_playlist_id)
            .unwrap_or_else(|| "Remote Playlist".to_string());
        let existing_tracks = self
            .db_manager
            .get_tracks_for_playlist(local_playlist_id)
            .map_err(|error| format!("failed to read local playlist tracks: {error}"))?;

        let existing_playlists = self
            .db_manager
            .get_all_playlists()
            .map_err(|error| format!("failed to list playlists: {error}"))?;
        if existing_playlists
            .iter()
            .any(|playlist| playlist.id == remote_bound_playlist_id)
        {
            let existing_remote_tracks = self
                .db_manager
                .get_tracks_for_playlist(&remote_bound_playlist_id)
                .map_err(|error| format!("failed to inspect existing remote playlist: {error}"))?;
            for track in existing_remote_tracks {
                let _ = self.db_manager.delete_track(&track.id);
            }
            let _ = self
                .db_manager
                .rename_playlist(&remote_bound_playlist_id, &playlist_name);
        } else {
            self.db_manager
                .create_playlist(&remote_bound_playlist_id, &playlist_name)
                .map_err(|error| format!("failed to create remote-bound playlist: {error}"))?;
        }

        let pending: Vec<(String, PathBuf)> = existing_tracks
            .into_iter()
            .map(|track| (Uuid::new_v4().to_string(), track.path))
            .collect();
        if !pending.is_empty() {
            self.db_manager
                .save_tracks_batch(&remote_bound_playlist_id, &pending, 0)
                .map_err(|error| {
                    format!("failed to store remote-bound playlist tracks: {error}")
                })?;
        }

        self.db_manager
            .delete_playlist(local_playlist_id)
            .map_err(|error| format!("failed to remove local playlist binding: {error}"))?;

        if matches!(
            self.playback_queue_source.as_ref(),
            Some(protocol::PlaybackQueueSource::Playlist { playlist_id })
                if playlist_id == local_playlist_id
        ) {
            self.playback_queue_source = None;
        }

        if self.active_playlist_id == local_playlist_id {
            self.active_playlist_id = remote_bound_playlist_id.clone();
            self.reload_editing_playlist_from_active();
        }
        self.last_remote_writeback_signature
            .remove(local_playlist_id);
        self.last_remote_writeback_signature
            .remove(&remote_bound_playlist_id);

        Ok(remote_bound_playlist_id)
    }

    fn start_playback_queue(&mut self, request: protocol::PlaybackQueueRequest) {
        if request.tracks.is_empty() {
            return;
        }

        let mut playback_playlist = Playlist::new();
        playback_playlist.set_playback_order(self.playback_order);
        playback_playlist.set_repeat_mode(self.repeat_mode);
        for track in request.tracks {
            playback_playlist.add_track(Track {
                path: track.path,
                id: track.id,
            });
        }

        let clamped_start = request.start_index.min(playback_playlist.num_tracks() - 1);
        playback_playlist.set_selected_indices(vec![clamped_start]);
        playback_playlist.force_re_randomize_shuffle();
        self.playback_playlist = playback_playlist;
        self.playback_queue_source = Some(request.source);
        self.play_playback_track(clamped_start, true);
    }

    fn play_playback_track(&mut self, index: usize, forward: bool) {
        let Some(index) = self.find_playable_index_from(index, forward) else {
            self.stop_playback_after_unavailable();
            return;
        };
        if index >= self.playback_playlist.num_tracks() {
            debug!("play_playback_track: Index {} out of bounds", index);
            return;
        }

        self.pending_start_track_id = None;
        self.started_track_id = None;
        self.pending_rate_switch = None;
        self.pending_rate_switch_play_immediately = false;
        self.playback_playlist.set_playing(true);
        self.playback_playlist.set_playing_track_index(Some(index));
        self.current_elapsed_ms = 0;

        let track_id = self.playback_playlist.get_track_id(index);

        if self.playback_route == protocol::PlaybackRoute::Cast {
            let track = self.playback_playlist.get_track(index).clone();
            let _ =
                self.bus_producer
                    .send(protocol::Message::Cast(protocol::CastMessage::LoadTrack {
                        track_id: track_id.clone(),
                        path: track.path,
                        start_offset_ms: 0,
                    }));
            self.pending_start_track_id = Some(track_id);
            self.broadcast_playlist_changed();
            return;
        }

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

    fn handoff_to_cast_if_playing(&mut self) {
        if self.playback_route != protocol::PlaybackRoute::Cast {
            return;
        }
        if !self.playback_playlist.is_playing() {
            return;
        }
        let Some(index) = self.playback_playlist.get_playing_track_index() else {
            return;
        };
        if index >= self.playback_playlist.num_tracks() {
            return;
        }
        let track = self.playback_playlist.get_track(index).clone();
        self.stop_decoding();
        let _ = self.bus_producer.send(protocol::Message::Playback(
            protocol::PlaybackMessage::ClearPlayerCache,
        ));
        let _ = self
            .bus_producer
            .send(protocol::Message::Cast(protocol::CastMessage::LoadTrack {
                track_id: track.id.clone(),
                path: track.path,
                start_offset_ms: self.current_elapsed_ms,
            }));
        self.pending_start_track_id = Some(track.id);
        self.broadcast_playlist_changed();
    }

    fn handoff_back_to_local_if_playing(&mut self) -> bool {
        if self.playback_route != protocol::PlaybackRoute::Local {
            return false;
        }
        if !self.playback_playlist.is_playing() {
            return false;
        }
        let Some(index) = self.playback_playlist.get_playing_track_index() else {
            return false;
        };
        if index >= self.playback_playlist.num_tracks() {
            return false;
        }
        let track = self.playback_playlist.get_track(index).clone();
        let resume_offset_ms = self.current_elapsed_ms;

        self.clear_cached_tracks();
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::DecodeTracks(vec![TrackIdentifier {
                id: track.id.clone(),
                path: track.path,
                play_immediately: true,
                start_offset_ms: resume_offset_ms,
            }]),
        ));
        self.requested_track_offsets
            .insert(track.id.clone(), resume_offset_ms);
        self.pending_start_track_id = Some(track.id);
        true
    }

    fn update_runtime_policy_from_config(&mut self, config: &Config) {
        self.sample_rate_auto_enabled = config.output.sample_rate_auto;
        if !self.sample_rate_auto_enabled {
            self.pending_rate_switch = None;
            self.pending_rate_switch_play_immediately = false;
        }
    }

    fn restore_playback_preferences_from_config(&mut self, config: &Config) -> bool {
        let next_playback_order = match config.ui.playback_order {
            UiPlaybackOrder::Default => protocol::PlaybackOrder::Default,
            UiPlaybackOrder::Shuffle => protocol::PlaybackOrder::Shuffle,
            UiPlaybackOrder::Random => protocol::PlaybackOrder::Random,
        };
        let next_repeat_mode = match config.ui.repeat_mode {
            UiRepeatMode::Off => protocol::RepeatMode::Off,
            UiRepeatMode::Playlist => protocol::RepeatMode::Playlist,
            UiRepeatMode::Track => protocol::RepeatMode::Track,
        };

        let changed =
            self.playback_order != next_playback_order || self.repeat_mode != next_repeat_mode;
        self.playback_order = next_playback_order;
        self.repeat_mode = next_repeat_mode;
        self.editing_playlist
            .set_playback_order(next_playback_order);
        self.editing_playlist.set_repeat_mode(next_repeat_mode);
        self.playback_playlist
            .set_playback_order(next_playback_order);
        self.playback_playlist.set_repeat_mode(next_repeat_mode);
        changed
    }

    fn update_verified_output_rates(&mut self, mut rates: Vec<u32>) {
        rates.sort_unstable();
        rates.dedup();
        self.verified_output_rates = rates;
    }

    fn probe_track_sample_rate(path: &PathBuf) -> Option<u32> {
        let file = std::fs::File::open(path).ok()?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let hint = Hint::new();
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .ok()?;
        let format = probed.format;
        let track = format.default_track()?;
        track.codec_params.sample_rate
    }

    fn track_sample_rate_hz(&mut self, track: &Track) -> Option<u32> {
        if let Some(cached) = self.track_sample_rate_cache.get(&track.path) {
            return *cached;
        }
        if is_remote_track_path(track.path.as_path()) {
            self.track_sample_rate_cache
                .insert(track.path.clone(), None);
            return None;
        }
        let probed = Self::probe_track_sample_rate(&track.path);
        if let Some(rate) = probed {
            debug!(
                "PlaylistManager: Probed source sample-rate {} Hz for {}",
                rate,
                track.path.display()
            );
        } else {
            debug!(
                "PlaylistManager: Failed to probe source sample-rate for {}",
                track.path.display()
            );
        }
        self.track_sample_rate_cache
            .insert(track.path.clone(), probed);
        probed
    }

    fn desired_output_rate_for_track(&mut self, track: &Track) -> Option<u32> {
        if !self.sample_rate_auto_enabled {
            return self.current_output_rate_hz;
        }
        if self.verified_output_rates.is_empty() {
            return self.current_output_rate_hz;
        }

        let source_rate = match self.track_sample_rate_hz(track) {
            Some(source_rate) => source_rate,
            None => {
                if is_remote_track_path(track.path.as_path()) {
                    return self.current_output_rate_hz;
                }
                self.current_output_rate_hz
                    .or_else(|| self.verified_output_rates.last().copied())?
            }
        };
        if self.verified_output_rates.contains(&source_rate) {
            return Some(source_rate);
        }
        self.verified_output_rates
            .iter()
            .copied()
            .find(|rate| *rate > source_rate)
            .or_else(|| self.verified_output_rates.last().copied())
    }

    fn request_runtime_output_rate_switch(&mut self, sample_rate_hz: u32, play_immediately: bool) {
        let _ = self.bus_producer.send(protocol::Message::Config(
            protocol::ConfigMessage::SetRuntimeOutputRate {
                sample_rate_hz,
                reason: "playlist_rate_segment".to_string(),
            },
        ));
        self.pending_rate_switch = Some(sample_rate_hz);
        self.pending_rate_switch_play_immediately = play_immediately;
        debug!(
            "PlaylistManager: Requested runtime output-rate switch to {} Hz",
            sample_rate_hz
        );
    }

    fn maybe_start_pending_after_rate_switch(&mut self) {
        if self.pending_rate_switch.is_some() {
            return;
        }
        let play_immediately = std::mem::take(&mut self.pending_rate_switch_play_immediately);
        self.cache_tracks(play_immediately);
    }

    fn handle_technical_metadata_changed(&mut self, meta: protocol::TechnicalMetadata) {
        debug!(
            "PlaylistManager: Received technical metadata changed for track: {:?}",
            meta
        );
        self.current_track_duration_ms = meta.duration_ms;
        let source_rate_supported = self.verified_output_rates.contains(&meta.sample_rate_hz);
        let should_switch = self.playback_route == protocol::PlaybackRoute::Local
            && self.sample_rate_auto_enabled
            && self.pending_rate_switch.is_none()
            && self.current_output_rate_hz != Some(meta.sample_rate_hz)
            && source_rate_supported;
        if should_switch {
            debug!(
                "PlaylistManager: Source {} Hz differs from output {:?}; requesting runtime switch before continuing playback",
                meta.sample_rate_hz, self.current_output_rate_hz
            );
            self.clear_cached_tracks();
            self.request_runtime_output_rate_switch(meta.sample_rate_hz, true);
        } else {
            debug!(
                "PlaylistManager: Keeping output {:?} for source {} Hz (route={:?}, auto={}, pending={:?}, source_supported={})",
                self.current_output_rate_hz,
                meta.sample_rate_hz,
                self.playback_route,
                self.sample_rate_auto_enabled,
                self.pending_rate_switch,
                source_rate_supported
            );
        }
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

    fn snapshot_editing_playlist_tracks(&self) -> Vec<protocol::RestoredTrack> {
        (0..self.editing_playlist.num_tracks())
            .map(|index| {
                let track = self.editing_playlist.get_track(index);
                protocol::RestoredTrack {
                    id: track.id.clone(),
                    path: track.path.clone(),
                }
            })
            .collect()
    }

    fn broadcast_playlist_state_snapshot(&mut self, playlists: Vec<protocol::PlaylistInfo>) {
        let restored_tracks = self.snapshot_editing_playlist_tracks();
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistRestored(restored_tracks),
        ));
        self.emit_metadata_updates_for_tracks(&self.snapshot_editing_playlist_tracks());
        self.reconcile_editing_playlist_remote_availability();
        self.emit_opensubsonic_sync_eligible_playlists(&playlists);
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistsRestored(playlists),
        ));
        if !self.active_playlist_id.is_empty() {
            let _ = self.bus_producer.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::ActivePlaylistChanged(self.active_playlist_id.clone()),
            ));
            self.broadcast_active_playlist_column_order();
        }
    }

    fn sync_remote_playlists(
        &mut self,
        profile_id: &str,
        playlists: Vec<protocol::RemotePlaylistSnapshot>,
    ) {
        self.suppress_remote_writeback = true;
        let existing_before_sync = self.db_manager.get_all_playlists().unwrap_or_default();
        let mut remote_playlist_ids = HashSet::new();
        for remote_playlist in playlists {
            let remote_playlist_id = remote_playlist.remote_playlist_id.clone();
            let local_playlist_id =
                format!("remote:opensubsonic:{}:{}", profile_id, remote_playlist_id);
            remote_playlist_ids.insert(local_playlist_id.clone());
            if !existing_before_sync
                .iter()
                .any(|playlist| playlist.id == local_playlist_id)
            {
                let _ = self
                    .db_manager
                    .create_playlist(&local_playlist_id, &remote_playlist.name);
            } else {
                let _ = self
                    .db_manager
                    .rename_playlist(&local_playlist_id, &remote_playlist.name);
            }

            if let Ok(existing_tracks) = self.db_manager.get_tracks_for_playlist(&local_playlist_id)
            {
                for track in existing_tracks {
                    let _ = self.db_manager.delete_track(&track.id);
                }
            }

            let mut pending_db_rows: Vec<(String, PathBuf)> =
                Vec::with_capacity(remote_playlist.tracks.len());
            let mut metadata_updates = Vec::with_capacity(remote_playlist.tracks.len());
            for (position, remote_track) in remote_playlist.tracks.into_iter().enumerate() {
                let local_track_id = format!(
                    "remote-track:opensubsonic:{}:{}:{}:{}",
                    profile_id, remote_playlist_id, remote_track.item_id, position
                );
                self.remote_track_metadata_by_path
                    .insert(remote_track.path.clone(), remote_track.summary.clone());
                metadata_updates.push(protocol::TrackMetadataPatch {
                    track_id: local_track_id.clone(),
                    summary: remote_track.summary,
                });
                pending_db_rows.push((local_track_id, remote_track.path));
            }
            let _ = self
                .db_manager
                .save_tracks_batch(&local_playlist_id, &pending_db_rows, 0);
            if !metadata_updates.is_empty() {
                let _ = self.bus_producer.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::TrackMetadataBatchUpdated {
                        updates: metadata_updates,
                    },
                ));
            }
        }
        for stale_playlist in existing_before_sync.iter().filter(|playlist| {
            playlist
                .id
                .strip_prefix("remote:opensubsonic:")
                .and_then(|suffix| suffix.split_once(':'))
                .map(|(existing_profile_id, _)| existing_profile_id == profile_id)
                .unwrap_or(false)
                && !remote_playlist_ids.contains(&playlist.id)
        }) {
            let _ = self.db_manager.delete_playlist(&stale_playlist.id);
            if matches!(
                self.playback_queue_source.as_ref(),
                Some(protocol::PlaybackQueueSource::Playlist { playlist_id })
                    if playlist_id == &stale_playlist.id
            ) {
                self.playback_queue_source = None;
            }
        }
        if let Ok(mut playlists) = self.db_manager.get_all_playlists() {
            if playlists.is_empty() {
                let default_id = Uuid::new_v4().to_string();
                if self
                    .db_manager
                    .create_playlist(&default_id, "Default")
                    .is_ok()
                {
                    playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                }
            }
            if playlists
                .iter()
                .all(|playlist| playlist.id != self.active_playlist_id)
            {
                self.active_playlist_id = playlists
                    .first()
                    .map(|playlist| playlist.id.clone())
                    .unwrap_or_default();
            }
            if !self.active_playlist_id.is_empty() {
                self.reload_editing_playlist_from_active();
            }
            self.broadcast_playlist_state_snapshot(playlists);
            self.broadcast_playlist_changed();
            self.broadcast_selection_changed();
        }
        self.suppress_remote_writeback = false;
    }

    fn normalized_playlist_name(name: &str) -> String {
        name.trim().to_string()
    }

    fn generate_unique_playlist_name(existing_names: &[String], requested_name: &str) -> String {
        let requested = Self::normalized_playlist_name(requested_name);
        if !requested.is_empty() && !existing_names.iter().any(|name| name == &requested) {
            return requested;
        }

        let mut suffix = 1usize;
        loop {
            let candidate = format!("Playlist {suffix}");
            if !existing_names.iter().any(|name| name == &candidate) {
                return candidate;
            }
            suffix = suffix.saturating_add(1);
        }
    }

    /// Intentional paste behavior:
    /// insert after the last selected item; if nothing is selected, append.
    /// Keep this stable so paste is predictable across playlist workflows.
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

    fn append_tracks_to_playlist(
        &mut self,
        playlist_id: &str,
        paths: &[PathBuf],
    ) -> Vec<protocol::RestoredTrack> {
        let existing_len = self
            .db_manager
            .get_tracks_for_playlist(playlist_id)
            .map(|tracks| tracks.len())
            .unwrap_or(0);

        let pending: Vec<(String, PathBuf)> = paths
            .iter()
            .map(|path| (Uuid::new_v4().to_string(), path.clone()))
            .collect();
        if let Err(err) = self
            .db_manager
            .save_tracks_batch(playlist_id, &pending, existing_len)
        {
            error!(
                "Failed to append {} track(s) to playlist {} in batch: {}",
                pending.len(),
                playlist_id,
                err
            );
            return Vec::new();
        }
        let inserted: Vec<protocol::RestoredTrack> = pending
            .into_iter()
            .map(|(id, path)| protocol::RestoredTrack { id, path })
            .collect();
        inserted
    }

    fn emit_metadata_updates_for_tracks(&self, tracks: &[protocol::RestoredTrack]) {
        let updates: Vec<protocol::TrackMetadataPatch> = tracks
            .iter()
            .filter_map(|track| {
                self.remote_track_metadata_by_path
                    .get(&track.path)
                    .cloned()
                    .map(|summary| protocol::TrackMetadataPatch {
                        track_id: track.id.clone(),
                        summary,
                    })
            })
            .collect();
        if updates.is_empty() {
            return;
        }
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::TrackMetadataBatchUpdated { updates },
        ));
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
        self.editing_playlist = self.build_playlist_from_snapshot(&snapshot);

        self.synchronize_active_playlist_tracks_in_db(&snapshot);

        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistRestored(Self::restored_tracks_from_snapshot(
                &snapshot,
            )),
        ));

        self.broadcast_playlist_changed();
        self.broadcast_selection_changed();
    }

    fn import_tracks_batch(&mut self, paths: Vec<PathBuf>, source: protocol::ImportSource) {
        if paths.is_empty() {
            return;
        }
        debug!(
            "PlaylistManager: importing {} track(s) from {:?}",
            paths.len(),
            source
        );
        let previous_track_list = self.capture_track_list_snapshot();
        let insert_at = self.editing_playlist.num_tracks();
        let pending: Vec<(String, PathBuf)> = paths
            .into_iter()
            .map(|path| (Uuid::new_v4().to_string(), path))
            .collect();
        if let Err(err) =
            self.db_manager
                .save_tracks_batch(&self.active_playlist_id, &pending, insert_at)
        {
            error!(
                "Failed to persist {} batched imported track(s): {}",
                pending.len(),
                err
            );
            return;
        }
        let mut inserted_tracks = Vec::with_capacity(pending.len());
        for (id, path) in pending {
            let track = Track {
                path: path.clone(),
                id: id.clone(),
            };
            self.editing_playlist.add_track(track);
            inserted_tracks.push(protocol::RestoredTrack { id, path });
        }
        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::TracksInsertedBatch {
                tracks: inserted_tracks,
                insert_at,
            },
        ));
        if Self::track_list_changed(&previous_track_list, &self.capture_track_list_snapshot()) {
            self.push_track_list_undo_snapshot(previous_track_list);
        }
        self.broadcast_playlist_changed();
        self.broadcast_selection_changed();
    }

    fn drain_bulk_import_queue(&mut self) {
        while let Ok(request) = self.bulk_import_rx.try_recv() {
            self.import_tracks_batch(request.paths, request.source);
        }
    }

    /// Starts the blocking event loop for playlist messages and playback coordination.
    pub fn run(&mut self) {
        // Restore playlists from database
        let mut playlists = match self.db_manager.get_all_playlists() {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to get playlists from database: {}", e);
                Vec::new()
            }
        };
        if playlists.is_empty() {
            let default_id = Uuid::new_v4().to_string();
            if let Err(err) = self.db_manager.create_playlist(&default_id, "Default") {
                error!("Failed to create default playlist at startup: {}", err);
            } else {
                playlists = self.db_manager.get_all_playlists().unwrap_or_default();
            }
        }

        if !playlists.is_empty() {
            self.active_playlist_id = playlists[0].id.clone();
            info!(
                "Restoring playlists. Active: {} ({})",
                playlists[0].name, playlists[0].id
            );

            // Restore tracks for active playlist
            self.editing_playlist = Playlist::new();
            self.editing_playlist
                .set_playback_order(self.playback_order);
            self.editing_playlist.set_repeat_mode(self.repeat_mode);
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
                }
                Err(e) => {
                    error!("Failed to restore tracks from database: {}", e);
                }
            }
            self.broadcast_playlist_state_snapshot(playlists);
        }

        loop {
            self.drain_bulk_import_queue();
            match self.bus_consumer.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTrack(path)) => {
                        if self.should_warn_before_mixed_insert(std::slice::from_ref(&path)) {
                            self.request_mixed_detach_confirmation(vec![path]);
                            continue;
                        }
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

                        let inserted_track = protocol::RestoredTrack {
                            id: id.clone(),
                            path: path.clone(),
                        };
                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::TrackAdded { id, path },
                        ));
                        self.emit_metadata_updates_for_tracks(&[inserted_track]);

                        if Self::track_list_changed(
                            &previous_track_list,
                            &self.capture_track_list_snapshot(),
                        ) {
                            self.push_track_list_undo_snapshot(previous_track_list);
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::DrainBulkImportQueue,
                    ) => {
                        self.drain_bulk_import_queue();
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTracksBatch {
                        paths,
                        source,
                    }) => {
                        if self.should_warn_before_mixed_insert(&paths) {
                            self.request_mixed_detach_confirmation(paths);
                            continue;
                        }
                        self.import_tracks_batch(paths, source);
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::AddTracksToPlaylists {
                            mut playlist_ids,
                            paths,
                        },
                    ) => {
                        if playlist_ids.is_empty() || paths.is_empty() {
                            continue;
                        }
                        if playlist_ids.iter().any(|id| id == &self.active_playlist_id)
                            && self.should_warn_before_mixed_insert(&paths)
                        {
                            self.request_mixed_detach_confirmation(paths);
                            continue;
                        }

                        let mut seen_playlist_ids = HashSet::new();
                        playlist_ids
                            .retain(|playlist_id| seen_playlist_ids.insert(playlist_id.clone()));

                        let mut previous_track_list = None;
                        let mut inserted_active_tracks = Vec::new();
                        for playlist_id in &playlist_ids {
                            let inserted = self.append_tracks_to_playlist(playlist_id, &paths);
                            if inserted.is_empty() {
                                continue;
                            }

                            if *playlist_id == self.active_playlist_id {
                                if previous_track_list.is_none() {
                                    previous_track_list = Some(self.capture_track_list_snapshot());
                                }
                                for track in &inserted {
                                    let next_track = Track {
                                        path: track.path.clone(),
                                        id: track.id.clone(),
                                    };
                                    self.editing_playlist.add_track(next_track);
                                }
                                inserted_active_tracks.extend(inserted);
                            }
                        }

                        if inserted_active_tracks.is_empty() {
                            continue;
                        }

                        let insert_at = self
                            .editing_playlist
                            .num_tracks()
                            .saturating_sub(inserted_active_tracks.len());
                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::TracksInsertedBatch {
                                tracks: inserted_active_tracks,
                                insert_at,
                            },
                        ));
                        self.emit_metadata_updates_for_tracks(
                            &self
                                .snapshot_editing_playlist_tracks()
                                .into_iter()
                                .skip(insert_at)
                                .collect::<Vec<_>>(),
                        );

                        if let Some(snapshot) = previous_track_list {
                            if Self::track_list_changed(
                                &snapshot,
                                &self.capture_track_list_snapshot(),
                            ) {
                                self.push_track_list_undo_snapshot(snapshot);
                            }
                        }

                        self.broadcast_playlist_changed();
                        self.broadcast_selection_changed();
                    }
                    protocol::Message::Config(protocol::ConfigMessage::ConfigChanged(config)) => {
                        let previous_sample_rate_auto = self.sample_rate_auto_enabled;
                        self.update_runtime_policy_from_config(&config);
                        if !self.playback_preferences_restored_from_config {
                            let playback_changed =
                                self.restore_playback_preferences_from_config(&config);
                            self.playback_preferences_restored_from_config = true;
                            if playback_changed {
                                self.broadcast_playlist_changed();
                            }
                        }
                        if self.playback_playlist.is_playing()
                            && previous_sample_rate_auto != self.sample_rate_auto_enabled
                        {
                            self.cache_tracks(false);
                        }
                    }
                    protocol::Message::Config(
                        protocol::ConfigMessage::OutputDeviceCapabilitiesChanged {
                            verified_sample_rates,
                        },
                    ) => {
                        self.update_verified_output_rates(verified_sample_rates);
                        if self.playback_playlist.is_playing() {
                            self.cache_tracks(false);
                        }
                    }
                    protocol::Message::Config(protocol::ConfigMessage::AudioDeviceOpened {
                        stream_info,
                    }) => {
                        self.current_output_rate_hz = Some(stream_info.sample_rate_hz);
                        if let Some(expected_rate) = self.pending_rate_switch {
                            if stream_info.sample_rate_hz == expected_rate {
                                debug!(
                                    "PlaylistManager: Output-rate switch acknowledged at {} Hz",
                                    stream_info.sample_rate_hz
                                );
                                self.pending_rate_switch = None;
                                self.maybe_start_pending_after_rate_switch();
                            } else {
                                let fallback_rate = self
                                    .verified_output_rates
                                    .last()
                                    .copied()
                                    .unwrap_or(stream_info.sample_rate_hz);
                                if fallback_rate == stream_info.sample_rate_hz
                                    || fallback_rate == expected_rate
                                {
                                    debug!(
                                        "PlaylistManager: Rate switch fallback accepted at {} Hz (requested {} Hz, fallback {} Hz)",
                                        stream_info.sample_rate_hz, expected_rate, fallback_rate
                                    );
                                    self.pending_rate_switch = None;
                                    self.maybe_start_pending_after_rate_switch();
                                } else {
                                    debug!(
                                        "PlaylistManager: Requested {} Hz but device opened {} Hz. Falling back to {} Hz",
                                        expected_rate, stream_info.sample_rate_hz, fallback_rate
                                    );
                                    self.request_runtime_output_rate_switch(
                                        fallback_rate,
                                        self.pending_rate_switch_play_immediately,
                                    );
                                }
                            }
                        }
                    }
                    protocol::Message::Config(
                        protocol::ConfigMessage::SetRuntimeOutputRate { .. }
                        | protocol::ConfigMessage::ClearRuntimeOutputRateOverride,
                    ) => {}
                    protocol::Message::Cast(protocol::CastMessage::ConnectionStateChanged {
                        state,
                        ..
                    }) => match state {
                        protocol::CastConnectionState::Connected => {
                            self.playback_route = protocol::PlaybackRoute::Cast;
                            self.handoff_to_cast_if_playing();
                        }
                        protocol::CastConnectionState::Disconnected => {
                            if self.playback_route == protocol::PlaybackRoute::Cast {
                                let was_playing = self.playback_playlist.is_playing();
                                self.playback_route = protocol::PlaybackRoute::Local;
                                self.pending_start_track_id = None;
                                self.started_track_id = None;
                                if was_playing {
                                    let resumed = self.handoff_back_to_local_if_playing();
                                    if !resumed {
                                        self.playback_playlist.set_playing(false);
                                    }
                                }
                                self.broadcast_playlist_changed();
                            }
                        }
                        protocol::CastConnectionState::Discovering
                        | protocol::CastConnectionState::Connecting => {}
                    },
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        debug!("PlaylistManager: Received play resume command");
                        let has_paused_track = !self.playback_playlist.is_playing()
                            && self
                                .playback_playlist
                                .get_playing_track_index()
                                .map(|index| index < self.playback_playlist.num_tracks())
                                .unwrap_or(false);
                        if has_paused_track {
                            debug!("PlaylistManager: Resuming playback");
                            if self.playback_route == protocol::PlaybackRoute::Cast {
                                let _ = self
                                    .bus_producer
                                    .send(protocol::Message::Cast(protocol::CastMessage::Play));
                            }
                            self.playback_playlist.set_playing(true);
                            self.broadcast_playlist_changed();
                        }
                    }
                    protocol::Message::Playback(
                        protocol::PlaybackMessage::PlayActiveCollection,
                    ) => {
                        trace!("PlaylistManager: PlayActiveCollection handled by UiManager");
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::StartQueue(request)) => {
                        self.start_playback_queue(request);
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        debug!("PlaylistManager: Received stop command");
                        self.pending_start_track_id = None;
                        self.started_track_id = None;
                        self.playback_playlist.set_playing(false);
                        self.playback_playlist.set_playing_track_index(None);
                        self.playback_queue_source = None;
                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            let _ = self
                                .bus_producer
                                .send(protocol::Message::Cast(protocol::CastMessage::Stop));
                            self.clear_cached_tracks();
                        } else {
                            self.clear_cached_tracks();
                            self.cache_tracks(false);
                        }
                        let _ = self.bus_producer.send(protocol::Message::Config(
                            protocol::ConfigMessage::ClearRuntimeOutputRateOverride,
                        ));

                        // Notify other components about the selection and playing change
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                        debug!("PlaylistManager: Received pause command");
                        if self.playback_playlist.is_playing() {
                            if self.playback_route == protocol::PlaybackRoute::Cast {
                                let _ = self
                                    .bus_producer
                                    .send(protocol::Message::Cast(protocol::CastMessage::Pause));
                            }
                            self.playback_playlist.set_playing(false);
                            self.broadcast_playlist_changed();
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
                                self.play_playback_track(next_index, true);
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
                                self.play_playback_track(prev_index, false);
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
                                    self.play_playback_track(index, true);
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
                                    playlist_id: self.playback_playlist_id().unwrap_or_default(),
                                },
                            ));
                            // Also notify UI to update metadata/art if selection is empty
                            self.broadcast_playlist_changed();
                        }
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::ReadyForPlayback(
                        id,
                    )) => {
                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            continue;
                        }
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
                                self.unavailable_track_ids.remove(&id);

                                if let Err(e) = self.db_manager.delete_track(&id) {
                                    error!("Failed to delete track from database: {}", e);
                                }

                                self.editing_playlist.delete_track(index);
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
                        self.editing_playlist.move_tracks(indices, to);

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
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::PasteTracks(paths)) => {
                        if paths.is_empty() {
                            continue;
                        }
                        if self.should_warn_before_mixed_insert(&paths) {
                            self.request_mixed_detach_confirmation(paths);
                            continue;
                        }
                        let previous_track_list = self.capture_track_list_snapshot();

                        let playlist_len = self.editing_playlist.num_tracks();
                        let insert_gap = Self::paste_insert_gap(
                            &self.editing_playlist.get_selected_indices(),
                            playlist_len,
                        );

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
                            self.editing_playlist.add_track(track);

                            appended_indices.push(append_index);
                            inserted_tracks.push(protocol::RestoredTrack { id, path });
                        }

                        if appended_indices.is_empty() {
                            continue;
                        }

                        self.editing_playlist
                            .move_tracks(appended_indices.clone(), insert_gap);

                        // Publish the anchored insertion point used for paste.
                        let insert_at = insert_gap.min(playlist_len);

                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::TracksInserted {
                                tracks: inserted_tracks,
                                insert_at,
                            },
                        ));
                        self.emit_metadata_updates_for_tracks(
                            &self
                                .snapshot_editing_playlist_tracks()
                                .into_iter()
                                .skip(insert_at)
                                .collect::<Vec<_>>(),
                        );

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
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::ConfirmDetachRemotePlaylist { playlist_id },
                    ) => {
                        if let Some(pending) = self.pending_mixed_detach.clone() {
                            if pending.playlist_id != playlist_id {
                                continue;
                            }
                            self.pending_mixed_detach = None;
                            if playlist_id == self.active_playlist_id {
                                self.detach_active_playlist_binding();
                                let paths = pending.pending_paths;
                                let _ = self.bus_producer.send(protocol::Message::Playlist(
                                    protocol::PlaylistMessage::PasteTracks(paths),
                                ));
                            }
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::CancelDetachRemotePlaylist { playlist_id },
                    ) => {
                        if self
                            .pending_mixed_detach
                            .as_ref()
                            .is_some_and(|pending| pending.playlist_id == playlist_id)
                        {
                            self.pending_mixed_detach = None;
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

                        self.editing_playlist
                            .apply_filter_view_snapshot(source_indices);

                        let new_ids: Vec<String> = (0..self.editing_playlist.num_tracks())
                            .map(|index| self.editing_playlist.get_track_id(index))
                            .collect();
                        let new_id_set: HashSet<String> = new_ids.iter().cloned().collect();
                        for removed_id in old_ids
                            .into_iter()
                            .filter(|old_id| !new_id_set.contains(old_id))
                        {
                            self.unavailable_track_ids.remove(&removed_id);
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

                        self.broadcast_playlist_changed();
                        self.broadcast_selection_changed();
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::CreatePlaylist {
                        name,
                    }) => {
                        let existing_playlist_names = self
                            .db_manager
                            .get_all_playlists()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|playlist| playlist.name)
                            .collect::<Vec<_>>();
                        let resolved_name =
                            Self::generate_unique_playlist_name(&existing_playlist_names, &name);
                        let id = Uuid::new_v4().to_string();
                        debug!(
                            "PlaylistManager: Creating playlist {} ({})",
                            resolved_name, id
                        );
                        if let Err(e) = self.db_manager.create_playlist(&id, &resolved_name) {
                            error!("Failed to create playlist in database: {}", e);
                        } else {
                            let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                            self.emit_opensubsonic_sync_eligible_playlists(&playlists);
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
                        let existing_playlist_names = self
                            .db_manager
                            .get_all_playlists()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|playlist| playlist.id != id)
                            .map(|playlist| playlist.name)
                            .collect::<Vec<_>>();
                        let resolved_name =
                            Self::generate_unique_playlist_name(&existing_playlist_names, &name);
                        debug!(
                            "PlaylistManager: Renaming playlist {} to {}",
                            id, resolved_name
                        );
                        if let Err(e) = self.db_manager.rename_playlist(&id, &resolved_name) {
                            error!("Failed to rename playlist in database: {}", e);
                        } else {
                            let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                            self.emit_opensubsonic_sync_eligible_playlists(&playlists);
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
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::SyncPlaylistToOpenSubsonicByIndex(index),
                    ) => {
                        let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                        if let Some(playlist) = playlists.get(index) {
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::SyncPlaylistToOpenSubsonic {
                                    id: playlist.id.clone(),
                                },
                            ));
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::SyncPlaylistToOpenSubsonic { id },
                    ) => {
                        self.request_opensubsonic_sync_for_playlist(&id);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeletePlaylist {
                        id,
                    }) => {
                        debug!("PlaylistManager: Deleting playlist {}", id);

                        if let Err(e) = self.db_manager.delete_playlist(&id) {
                            error!("Failed to delete playlist from database: {}", e);
                        } else {
                            if matches!(
                                self.playback_queue_source.as_ref(),
                                Some(protocol::PlaybackQueueSource::Playlist { playlist_id })
                                    if playlist_id == &id
                            ) {
                                self.playback_queue_source = None;
                            }

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
                                protocol::PlaylistMessage::OpenSubsonicSyncEligiblePlaylists(
                                    self.compute_opensubsonic_sync_eligible_playlist_ids(
                                        &playlists,
                                    ),
                                ),
                            ));
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::PlaylistsRestored(playlists),
                            ));
                            self.broadcast_playlist_changed();
                        }
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::SwitchPlaylistByIndex(index),
                    ) => {
                        let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                        if let Some(p) = playlists.get(index) {
                            if self.is_remote_playlist_blocked(&p.id) {
                                debug!(
                                    "PlaylistManager: blocked switch to unavailable remote playlist {}",
                                    p.id
                                );
                                continue;
                            }
                            let id = p.id.clone();
                            let _ = self.bus_producer.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::SwitchPlaylist { id },
                            ));
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::SwitchPlaylist {
                        id,
                    }) => {
                        if self.is_remote_playlist_blocked(&id) {
                            debug!(
                                "PlaylistManager: blocked switch to unavailable remote playlist {}",
                                id
                            );
                            continue;
                        }
                        debug!("PlaylistManager: Switching to playlist {}", id);
                        if id == self.active_playlist_id {
                            continue;
                        }

                        self.clear_track_list_history();
                        self.active_playlist_id = id.clone();
                        match self
                            .db_manager
                            .get_tracks_for_playlist(&self.active_playlist_id)
                        {
                            Ok(tracks) => {
                                self.editing_playlist = Playlist::new();
                                self.editing_playlist
                                    .set_playback_order(self.playback_order);
                                self.editing_playlist.set_repeat_mode(self.repeat_mode);
                                for track in tracks.iter() {
                                    self.editing_playlist.add_track(Track {
                                        path: track.path.clone(),
                                        id: track.id.clone(),
                                    });
                                }
                                let _ = self.bus_producer.send(protocol::Message::Playlist(
                                    protocol::PlaylistMessage::PlaylistRestored(tracks),
                                ));
                                self.emit_metadata_updates_for_tracks(
                                    &self.snapshot_editing_playlist_tracks(),
                                );
                                self.reconcile_editing_playlist_remote_availability();
                            }
                            Err(e) => {
                                error!("Failed to switch playlist: {}", e);
                            }
                        }

                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistChanged(id),
                        ));
                        self.broadcast_active_playlist_column_order();
                        self.broadcast_playlist_changed();
                    }
                    protocol::Message::Integration(
                        protocol::IntegrationMessage::BackendSnapshotUpdated(snapshot),
                    ) => {
                        self.backend_connection_states.clear();
                        for profile in snapshot.profiles {
                            self.backend_connection_states
                                .insert(profile.profile_id, profile.connection_state);
                        }
                        self.reconcile_editing_playlist_remote_availability();
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::RequestPlaylistState,
                    ) => {
                        let playlists = self.db_manager.get_all_playlists().unwrap_or_default();
                        self.broadcast_playlist_state_snapshot(playlists);
                    }
                    protocol::Message::Integration(
                        protocol::IntegrationMessage::OpenSubsonicLibraryTracksUpdated {
                            profile_id,
                            tracks,
                        },
                    ) => {
                        if tracks.is_empty() {
                            self.remove_remote_metadata_for_profile(&profile_id);
                        }
                        for track in tracks {
                            self.remote_track_metadata_by_path.insert(
                                track.path,
                                protocol::TrackMetadataSummary {
                                    title: track.title,
                                    artist: track.artist.clone(),
                                    album: track.album,
                                    album_artist: track.album_artist,
                                    date: track.year.clone(),
                                    genre: track.genre,
                                    year: track.year,
                                    track_number: track.track_number,
                                },
                            );
                        }
                        self.emit_metadata_updates_for_tracks(
                            &self.snapshot_editing_playlist_tracks(),
                        );
                    }
                    protocol::Message::Integration(
                        protocol::IntegrationMessage::OpenSubsonicPlaylistsUpdated {
                            profile_id,
                            playlists,
                        },
                    ) => {
                        self.sync_remote_playlists(&profile_id, playlists);
                    }
                    protocol::Message::Integration(
                        protocol::IntegrationMessage::OpenSubsonicPlaylistWritebackResult {
                            local_playlist_id,
                            success,
                            error,
                        },
                    ) => {
                        let _ = self.bus_producer.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::RemotePlaylistWritebackState {
                                playlist_id: local_playlist_id,
                                success,
                                error,
                            },
                        ));
                    }
                    protocol::Message::Integration(
                        protocol::IntegrationMessage::OpenSubsonicPlaylistCreateResult {
                            profile_id,
                            local_playlist_id,
                            remote_playlist_id,
                            success,
                            error,
                        },
                    ) => {
                        if !success {
                            self.emit_remote_writeback_state(local_playlist_id, false, error);
                            continue;
                        }
                        let Some(remote_playlist_id) = remote_playlist_id else {
                            self.emit_remote_writeback_state(
                                local_playlist_id,
                                false,
                                Some(
                                    "OpenSubsonic did not return a playlist id for created playlist"
                                        .to_string(),
                                ),
                            );
                            continue;
                        };
                        match self.promote_local_playlist_to_remote_binding(
                            &local_playlist_id,
                            &profile_id,
                            &remote_playlist_id,
                        ) {
                            Ok(remote_bound_playlist_id) => {
                                let playlists =
                                    self.db_manager.get_all_playlists().unwrap_or_default();
                                self.broadcast_playlist_state_snapshot(playlists);
                                self.broadcast_playlist_changed();
                                self.broadcast_selection_changed();
                                let _ = self.bus_producer.send(protocol::Message::Integration(
                                    protocol::IntegrationMessage::SyncBackendProfile { profile_id },
                                ));
                                self.emit_remote_writeback_state(
                                    remote_bound_playlist_id,
                                    true,
                                    None,
                                );
                            }
                            Err(promotion_error) => {
                                self.emit_remote_writeback_state(
                                    local_playlist_id,
                                    false,
                                    Some(format!(
                                        "Failed to convert playlist to remote binding: {}",
                                        promotion_error
                                    )),
                                );
                            }
                        }
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
                    protocol::Message::Audio(protocol::AudioMessage::TrackCached(id, offset)) => {
                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            continue;
                        }
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
                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            continue;
                        }
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

                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            self.broadcast_playlist_changed();
                            continue;
                        }

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
                        self.handle_technical_metadata_changed(meta);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackUnavailable {
                        id,
                        reason,
                    }) => {
                        self.handle_track_unavailable(id, reason);
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlaybackProgress {
                        elapsed_ms,
                        total_ms,
                    }) => {
                        self.current_elapsed_ms = elapsed_ms;
                        if total_ms > 0 {
                            self.current_track_duration_ms = total_ms;
                        }
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

                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            let _ = self.bus_producer.send(protocol::Message::Cast(
                                protocol::CastMessage::SeekMs(target_ms),
                            ));
                            self.current_elapsed_ms = target_ms;
                            continue;
                        }

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
                    protocol::Message::Playback(protocol::PlaybackMessage::SetVolume(volume)) => {
                        if self.playback_route == protocol::PlaybackRoute::Cast {
                            let _ = self.bus_producer.send(protocol::Message::Cast(
                                protocol::CastMessage::SetVolume(volume),
                            ));
                        }
                    }
                    _ => trace!("PlaylistManager: ignoring unsupported message"),
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(
                        "PlaylistManager lagged on control bus, skipped {} message(s)",
                        skipped
                    );
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
        if self.pending_rate_switch.is_some() {
            self.pending_rate_switch_play_immediately |= play_immediately;
            return;
        }

        let first_index = self
            .playback_playlist
            .get_playing_track_index()
            .unwrap_or(0);
        let first_track = self.playback_playlist.get_track(first_index).clone();
        let desired_first_rate = self
            .desired_output_rate_for_track(&first_track)
            .or(self.current_output_rate_hz);
        if let Some(desired_first_rate) = desired_first_rate {
            if self.current_output_rate_hz != Some(desired_first_rate) {
                self.request_runtime_output_rate_switch(desired_first_rate, play_immediately);
                return;
            }
        }

        let mut current_index = first_index;
        let mut track_paths = Vec::new();
        let mut segment_rate: Option<u32> = desired_first_rate;

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

                let track = self.playback_playlist.get_track(current_index).clone();
                let desired_rate = self
                    .desired_output_rate_for_track(&track)
                    .or(self.current_output_rate_hz);
                if let Some(required_segment_rate) = segment_rate {
                    if desired_rate != Some(required_segment_rate) {
                        break;
                    }
                } else {
                    segment_rate = desired_rate;
                }

                track_paths.push(TrackIdentifier {
                    id: track_id,
                    path: track.path.clone(),
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
        self.pending_rate_switch = None;
        self.pending_rate_switch_play_immediately = false;
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
        self.pending_rate_switch = None;
        self.pending_rate_switch_play_immediately = false;
        self.requested_track_offsets.clear();
        let _ = self.bus_producer.send(protocol::Message::Audio(
            protocol::AudioMessage::StopDecoding,
        ));
    }

    fn broadcast_playlist_changed(&mut self) {
        let mut playing_track_path = None;

        if let Some(playing_idx) = self.playback_playlist.get_playing_track_index() {
            if playing_idx < self.playback_playlist.num_tracks() {
                let track = self.playback_playlist.get_track(playing_idx);
                playing_track_path = Some(track.path.clone());
            }
        }

        let _ = self.bus_producer.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistIndicesChanged {
                playing_playlist_id: self.playback_playlist_id(),
                playing_index: self.playback_playlist.get_playing_track_index(),
                playing_track_path,
                playing_track_metadata: None,
                selected_indices: self.editing_playlist.get_selected_indices(),
                is_playing: self.playback_playlist.is_playing(),
                playback_order: self.playback_order,
                repeat_mode: self.repeat_mode,
            },
        ));

        if self.suppress_remote_writeback {
            return;
        }
        let Some((profile_id, remote_playlist_id)) =
            Self::remote_binding_from_playlist_id(&self.active_playlist_id)
        else {
            return;
        };
        let Some(song_ids) = self.remote_song_ids_if_pure_playlist(&self.active_playlist_id) else {
            return;
        };
        let signature = song_ids.join(",");
        if self
            .last_remote_writeback_signature
            .get(&self.active_playlist_id)
            .is_some_and(|previous| previous == &signature)
        {
            return;
        }
        self.last_remote_writeback_signature
            .insert(self.active_playlist_id.clone(), signature);
        let _ = self.bus_producer.send(protocol::Message::Integration(
            protocol::IntegrationMessage::PushOpenSubsonicPlaylistUpdate {
                profile_id,
                remote_playlist_id,
                local_playlist_id: self.active_playlist_id.clone(),
                track_song_ids: song_ids,
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
        active_playlist_id: String,
    }

    impl PlaylistManagerHarness {
        fn new() -> Self {
            let (bus_sender, _) = broadcast::channel(4096);
            let manager_bus_sender = bus_sender.clone();
            let manager_receiver = bus_sender.subscribe();
            let (_bulk_import_tx, bulk_import_rx) =
                std::sync::mpsc::sync_channel::<protocol::PlaylistBulkImportRequest>(64);
            let db_manager = DbManager::new_in_memory().expect("failed to create in-memory db");

            thread::spawn(move || {
                let mut manager = PlaylistManager::new(
                    Playlist::new(),
                    manager_receiver,
                    manager_bus_sender,
                    db_manager,
                    bulk_import_rx,
                );
                manager.run();
            });

            let mut receiver = bus_sender.subscribe();
            let active_playlist_message =
                wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
                    matches!(
                        message,
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistChanged(_)
                        )
                    )
                });
            let active_playlist_id = if let protocol::Message::Playlist(
                protocol::PlaylistMessage::ActivePlaylistChanged(id),
            ) = active_playlist_message
            {
                id
            } else {
                panic!("expected ActivePlaylistChanged message");
            };

            let mut harness = Self {
                bus_sender,
                receiver,
                active_playlist_id,
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

        fn start_queue(
            &self,
            source: protocol::PlaybackQueueSource,
            tracks: Vec<protocol::RestoredTrack>,
            start_index: usize,
        ) {
            self.send(protocol::Message::Playback(
                protocol::PlaybackMessage::StartQueue(protocol::PlaybackQueueRequest {
                    source,
                    tracks,
                    start_index,
                }),
            ));
        }

        fn start_playlist_queue_from_ids(&self, track_ids: &[String], start_index: usize) {
            let tracks = track_ids
                .iter()
                .map(|id| protocol::RestoredTrack {
                    id: id.clone(),
                    path: PathBuf::from(format!("/tmp/{id}.mp3")),
                })
                .collect();
            self.start_queue(
                protocol::PlaybackQueueSource::Playlist {
                    playlist_id: self.active_playlist_id.clone(),
                },
                tracks,
                start_index,
            );
        }

        fn start_library_queue(&self, tracks: Vec<protocol::RestoredTrack>, start_index: usize) {
            self.start_queue(protocol::PlaybackQueueSource::Library, tracks, start_index);
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

    fn stream_info_for_rate(sample_rate_hz: u32) -> protocol::OutputStreamInfo {
        protocol::OutputStreamInfo {
            device_name: "default".to_string(),
            sample_rate_hz,
            channel_count: 2,
            bits_per_sample: 32,
            sample_format: protocol::OutputSampleFormat::F32,
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
    fn test_generate_unique_playlist_name_prefers_requested_name_when_available() {
        let existing = vec!["Default".to_string(), "Playlist 1".to_string()];
        let resolved = PlaylistManager::generate_unique_playlist_name(&existing, "Roadtrip");
        assert_eq!(resolved, "Roadtrip");
    }

    #[test]
    fn test_generate_unique_playlist_name_falls_back_to_non_conflicting_default() {
        let existing = vec![
            "Default".to_string(),
            "Playlist 1".to_string(),
            "Playlist 2".to_string(),
        ];
        let empty_requested = PlaylistManager::generate_unique_playlist_name(&existing, "");
        assert_eq!(empty_requested, "Playlist 3");

        let duplicate_requested =
            PlaylistManager::generate_unique_playlist_name(&existing, "Playlist 2");
        assert_eq!(duplicate_requested, "Playlist 3");
    }

    #[test]
    fn test_paste_tracks_inserts_after_last_selected_track() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        let _ = harness.add_track("pm_paste_anchor_0");
        let _ = harness.add_track("pm_paste_anchor_1");
        let _ = harness.add_track("pm_paste_anchor_2");
        let _ = harness.add_track("pm_paste_anchor_3");
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::SelectionChanged(vec![1, 2]),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::SelectionChanged(indices))
                    if indices == &vec![1, 2]
            )
        });
        thread::sleep(Duration::from_millis(20));
        harness.drain_messages();

        let pasted_paths = vec![
            PathBuf::from("/tmp/pm_paste_anchor_new_0.mp3"),
            PathBuf::from("/tmp/pm_paste_anchor_new_1.mp3"),
        ];
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PasteTracks(pasted_paths.clone()),
        ));

        let inserted_message =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::TracksInserted {
                        tracks, ..
                    }) if tracks.len() == 2
                )
            });
        if let protocol::Message::Playlist(protocol::PlaylistMessage::TracksInserted {
            tracks,
            insert_at,
        }) = inserted_message
        {
            assert_eq!(insert_at, 3);
            let actual_paths: Vec<PathBuf> = tracks.into_iter().map(|track| track.path).collect();
            assert_eq!(actual_paths, pasted_paths);
        } else {
            panic!("expected TracksInserted message");
        }
    }

    #[test]
    fn test_paste_tracks_without_selection_appends_to_playlist_end() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        let _ = harness.add_track("pm_paste_end_0");
        let _ = harness.add_track("pm_paste_end_1");
        let _ = harness.add_track("pm_paste_end_2");
        harness.drain_messages();

        let pasted_paths = vec![PathBuf::from("/tmp/pm_paste_end_new_0.mp3")];
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PasteTracks(pasted_paths.clone()),
        ));

        let inserted_message =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::TracksInserted {
                        tracks, ..
                    }) if tracks.len() == 1
                )
            });
        if let protocol::Message::Playlist(protocol::PlaylistMessage::TracksInserted {
            tracks,
            insert_at,
        }) = inserted_message
        {
            assert_eq!(insert_at, 3);
            let actual_paths: Vec<PathBuf> = tracks.into_iter().map(|track| track.path).collect();
            assert_eq!(actual_paths, pasted_paths);
        } else {
            panic!("expected TracksInserted message");
        }
    }

    #[test]
    fn test_request_playlist_state_replays_current_snapshot() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::RequestPlaylistState,
        ));

        let start = Instant::now();
        let timeout = Duration::from_secs(1);
        let mut saw_playlists = false;
        let mut saw_active = false;
        let mut saw_tracks = false;
        while start.elapsed() < timeout && !(saw_playlists && saw_active && saw_tracks) {
            match harness.receiver.try_recv() {
                Ok(protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistsRestored(
                    playlists,
                ))) => {
                    saw_playlists = !playlists.is_empty();
                }
                Ok(protocol::Message::Playlist(
                    protocol::PlaylistMessage::ActivePlaylistChanged(id),
                )) => {
                    saw_active = id == harness.active_playlist_id;
                }
                Ok(protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(
                    _tracks,
                ))) => {
                    saw_tracks = true;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => break,
            }
        }

        assert!(saw_playlists, "expected PlaylistsRestored snapshot");
        assert!(saw_active, "expected ActivePlaylistChanged snapshot");
        assert!(saw_tracks, "expected PlaylistRestored snapshot");
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
    fn test_add_tracks_to_playlists_appends_to_active_playlist_and_supports_undo() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        let added_paths = vec![
            PathBuf::from("/tmp/pm_add_to_active_0.mp3"),
            PathBuf::from("/tmp/pm_add_to_active_1.mp3"),
        ];
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::AddTracksToPlaylists {
                playlist_ids: vec![harness.active_playlist_id.clone()],
                paths: added_paths.clone(),
            },
        ));

        let inserted_message =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::TracksInsertedBatch {
                        tracks,
                        ..
                    }) if tracks.len() == 2
                )
            });
        let inserted_paths =
            if let protocol::Message::Playlist(protocol::PlaylistMessage::TracksInsertedBatch {
                tracks,
                ..
            }) = inserted_message
            {
                tracks
                    .into_iter()
                    .map(|track| track.path)
                    .collect::<Vec<_>>()
            } else {
                panic!("expected TracksInsertedBatch message");
            };
        assert_eq!(inserted_paths, added_paths);
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::UndoTrackListEdit,
        ));
        let restored_message =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(_))
                )
            });
        if let protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(tracks)) =
            restored_message
        {
            assert!(tracks.is_empty(), "undo should restore active playlist");
        } else {
            panic!("expected PlaylistRestored message");
        }
    }

    #[test]
    fn test_add_tracks_to_playlists_to_non_active_playlist_persists_without_ui_insert() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::CreatePlaylist {
                name: "Add To Target".to_string(),
            },
        ));
        let playlists_message =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistsRestored(list))
                        if list.iter().any(|playlist| playlist.name == "Add To Target")
                )
            });
        let target_playlist_id = if let protocol::Message::Playlist(
            protocol::PlaylistMessage::PlaylistsRestored(list),
        ) = playlists_message
        {
            list.into_iter()
                .find(|playlist| playlist.name == "Add To Target")
                .map(|playlist| playlist.id)
                .expect("created playlist should be present")
        } else {
            panic!("expected PlaylistsRestored message");
        };
        harness.drain_messages();

        let target_path = PathBuf::from("/tmp/pm_add_to_other_0.mp3");
        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::AddTracksToPlaylists {
                playlist_ids: vec![target_playlist_id.clone()],
                paths: vec![target_path.clone()],
            },
        ));
        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::TracksInsertedBatch { .. }
                    )
                )
            },
        );

        harness.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::SwitchPlaylist {
                id: target_playlist_id.clone(),
            },
        ));
        let restored_message =
            wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(_))
                )
            });
        if let protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(tracks)) =
            restored_message
        {
            assert_eq!(tracks.len(), 1);
            assert_eq!(tracks[0].path, target_path);
        } else {
            panic!("expected PlaylistRestored message");
        }
    }

    #[test]
    fn test_play_pause_next_previous_event_sequence() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_play_0");
        let (id1, _) = harness.add_track("pm_play_1");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1.clone()], 0);

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
        let (id0, _) = harness.add_track("pm_resume_deselect_0");
        let (id1, _) = harness.add_track("pm_resume_deselect_1");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(&[id0, id1], 1);
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
    fn test_play_resumes_paused_library_queue_without_restarting_active_playlist() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        let library_tracks = vec![
            protocol::RestoredTrack {
                id: "lib_queue_track_0".to_string(),
                path: PathBuf::from("/tmp/lib_queue_track_0.mp3"),
            },
            protocol::RestoredTrack {
                id: "lib_queue_track_1".to_string(),
                path: PathBuf::from("/tmp/lib_queue_track_1.mp3"),
            },
        ];
        harness.start_library_queue(library_tracks, 1);
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
        harness.send(protocol::Message::Playback(protocol::PlaybackMessage::Play));

        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("timed out waiting for library-queue resume message");
            }
            match harness.receiver.try_recv() {
                Ok(protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks))) => {
                    panic!(
                        "unexpected decode request while resuming paused library queue: {:?}",
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
                    panic!("bus closed while waiting for library-queue resume message")
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

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1.clone()], 0);
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

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1.clone()], 0);
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
    fn test_repeat_playlist_does_not_wrap_early_for_long_queue() {
        let mut harness = PlaylistManagerHarness::new();
        let mut ids = Vec::new();
        for index in 0..30usize {
            let (id, _) = harness.add_track(&format!("pm_repeat_long_{index}"));
            ids.push(id);
        }
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(&ids, 0);
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

        for index in 0..10usize {
            harness.send(protocol::Message::Playback(
                protocol::PlaybackMessage::TrackFinished(ids[index].clone()),
            ));
            let expected_next_index = index + 1;
            let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                        is_playing: true,
                        playing_index: Some(next_index),
                        ..
                    }) if *next_index == expected_next_index
                )
            });
        }
    }

    #[test]
    fn test_repeat_track_replays_same_track_on_finish() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_repeat_track_0");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);
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

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);
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

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1.clone()], 0);
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

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1.clone()], 0);

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
        let (id0, _) = harness.add_track("pm_ready_non_current_0");
        let (id1, _) = harness.add_track("pm_ready_non_current_1");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(&[id0, id1.clone()], 0);
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

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);
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
        let (id1, _) = harness.add_track("pm_stale_finish_1");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1], 0);
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
        let (id1, _) = harness.add_track("pm_stale_started_1");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(&[id0.clone(), id1], 0);
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
    fn test_config_changed_without_policy_change_does_not_retrigger_rate_switch() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_cfg_no_rate_switch_0");
        harness.drain_messages();

        harness.send(protocol::Message::Config(
            protocol::ConfigMessage::OutputDeviceCapabilitiesChanged {
                verified_sample_rates: vec![44_100],
            },
        ));
        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Config(protocol::ConfigMessage::SetRuntimeOutputRate {
                    sample_rate_hz: 44_100,
                    ..
                })
            )
        });
        harness.send(protocol::Message::Config(
            protocol::ConfigMessage::AudioDeviceOpened {
                stream_info: stream_info_for_rate(44_100),
            },
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(_))
            )
        });
        harness.drain_messages();

        // Simulate output reopening at a different rate while playback remains active.
        harness.send(protocol::Message::Config(
            protocol::ConfigMessage::AudioDeviceOpened {
                stream_info: stream_info_for_rate(192_000),
            },
        ));
        harness.drain_messages();

        let mut config = Config::default();
        config.output.output_device_name = "default".to_string();
        config.output.output_device_auto = true;
        config.output.sample_rate_auto = true;
        config.output.sample_rate_khz = 192_000;
        harness.send(protocol::Message::Config(
            protocol::ConfigMessage::ConfigChanged(config),
        ));

        assert_no_message(
            &mut harness.receiver,
            Duration::from_millis(250),
            |message| {
                matches!(
                    message,
                    protocol::Message::Config(protocol::ConfigMessage::SetRuntimeOutputRate { .. })
                )
            },
        );
    }

    #[test]
    fn test_config_changed_restores_playback_preferences_only_once() {
        let mut harness = PlaylistManagerHarness::new();
        harness.drain_messages();

        let mut initial = Config::default();
        initial.ui.playback_order = UiPlaybackOrder::Shuffle;
        initial.ui.repeat_mode = UiRepeatMode::Track;
        harness.send(protocol::Message::Config(
            protocol::ConfigMessage::ConfigChanged(initial),
        ));
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    playback_order: protocol::PlaybackOrder::Shuffle,
                    repeat_mode: protocol::RepeatMode::Track,
                    ..
                })
            )
        });
        harness.drain_messages();

        let mut later = Config::default();
        later.ui.playback_order = UiPlaybackOrder::Random;
        later.ui.repeat_mode = UiRepeatMode::Playlist;
        harness.send(protocol::Message::Config(
            protocol::ConfigMessage::ConfigChanged(later),
        ));
        harness.drain_messages();

        let (id0, _) = harness.add_track("pm_cfg_restore_once_0");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);
        let _ = wait_for_message(&mut harness.receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                    is_playing: true,
                    playing_index: Some(0),
                    playback_order: protocol::PlaybackOrder::Shuffle,
                    repeat_mode: protocol::RepeatMode::Track,
                    ..
                })
            )
        });
    }

    #[test]
    fn test_seek_restarts_decode_from_requested_offset() {
        let mut harness = PlaylistManagerHarness::new();
        let (id0, _) = harness.add_track("pm_seek_0");
        harness.drain_messages();

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);
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
                channel_count: 2,
                duration_ms: 100_000,
                bits_per_sample: 16,
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

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);

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

        harness.start_playlist_queue_from_ids(std::slice::from_ref(&id0), 0);

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

    fn make_direct_manager() -> (PlaylistManager, Receiver<protocol::Message>) {
        let (bus_sender, _) = broadcast::channel(256);
        let receiver = bus_sender.subscribe();
        let manager_receiver = bus_sender.subscribe();
        let (_bulk_import_tx, bulk_import_rx) =
            std::sync::mpsc::sync_channel::<protocol::PlaylistBulkImportRequest>(64);
        let db_manager = DbManager::new_in_memory().expect("failed to create in-memory db");
        let manager = PlaylistManager::new(
            Playlist::new(),
            manager_receiver,
            bus_sender,
            db_manager,
            bulk_import_rx,
        );
        (manager, receiver)
    }

    #[test]
    fn test_desired_output_rate_match_track_prefers_exact_then_above_then_below() {
        let (mut manager, _receiver) = make_direct_manager();
        manager.sample_rate_auto_enabled = true;
        manager.verified_output_rates = vec![44_100, 48_000, 96_000];

        let track = Track {
            id: "rate_track".to_string(),
            path: PathBuf::from("/tmp/rate_track.flac"),
        };

        manager
            .track_sample_rate_cache
            .insert(track.path.clone(), Some(48_000));
        assert_eq!(manager.desired_output_rate_for_track(&track), Some(48_000));

        manager
            .track_sample_rate_cache
            .insert(track.path.clone(), Some(50_000));
        assert_eq!(manager.desired_output_rate_for_track(&track), Some(96_000));

        manager
            .track_sample_rate_cache
            .insert(track.path.clone(), Some(192_000));
        assert_eq!(manager.desired_output_rate_for_track(&track), Some(96_000));
    }

    #[test]
    fn test_cache_tracks_splits_decode_batches_at_rate_boundaries() {
        let (mut manager, mut receiver) = make_direct_manager();
        manager.sample_rate_auto_enabled = true;
        manager.verified_output_rates = vec![44_100, 48_000];
        manager.current_output_rate_hz = Some(44_100);
        manager.playback_playlist = Playlist::new();
        manager.playback_playlist.add_track(Track {
            id: "t0".to_string(),
            path: PathBuf::from("/tmp/t0.flac"),
        });
        manager.playback_playlist.add_track(Track {
            id: "t1".to_string(),
            path: PathBuf::from("/tmp/t1.flac"),
        });
        manager.playback_playlist.set_playing_track_index(Some(0));
        manager.playback_playlist.set_playing(true);
        manager
            .track_sample_rate_cache
            .insert(PathBuf::from("/tmp/t0.flac"), Some(44_100));
        manager
            .track_sample_rate_cache
            .insert(PathBuf::from("/tmp/t1.flac"), Some(48_000));

        manager.cache_tracks(false);

        let message = wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
            matches!(message, protocol::Message::Audio(_))
        });
        let protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(tracks)) = message else {
            panic!("expected DecodeTracks message");
        };
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "t0");
    }

    #[test]
    fn test_cache_tracks_requests_runtime_switch_before_decoding_when_needed() {
        let (mut manager, mut receiver) = make_direct_manager();
        manager.sample_rate_auto_enabled = true;
        manager.verified_output_rates = vec![44_100, 48_000];
        manager.current_output_rate_hz = Some(44_100);
        manager.playback_playlist = Playlist::new();
        manager.playback_playlist.add_track(Track {
            id: "t0".to_string(),
            path: PathBuf::from("/tmp/t0_48.flac"),
        });
        manager.playback_playlist.set_playing_track_index(Some(0));
        manager.playback_playlist.set_playing(true);
        manager
            .track_sample_rate_cache
            .insert(PathBuf::from("/tmp/t0_48.flac"), Some(48_000));

        manager.cache_tracks(true);

        let message = wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Config(protocol::ConfigMessage::SetRuntimeOutputRate { .. })
            )
        });
        let protocol::Message::Config(protocol::ConfigMessage::SetRuntimeOutputRate {
            sample_rate_hz,
            ..
        }) = message
        else {
            panic!("expected runtime output-rate switch request");
        };
        assert_eq!(sample_rate_hz, 48_000);
        assert_eq!(manager.pending_rate_switch, Some(48_000));
        assert!(
            !receiver
                .try_recv()
                .map(|message| matches!(
                    message,
                    protocol::Message::Audio(protocol::AudioMessage::DecodeTracks(_))
                ))
                .unwrap_or(false),
            "DecodeTracks should not be queued before output-rate switch ack"
        );
    }

    #[test]
    fn test_technical_metadata_switches_output_rate_even_after_track_started() {
        let (mut manager, mut receiver) = make_direct_manager();
        manager.sample_rate_auto_enabled = true;
        manager.verified_output_rates = vec![44_100, 48_000, 192_000];
        manager.current_output_rate_hz = Some(192_000);
        manager.playback_playlist = Playlist::new();
        manager.playback_playlist.add_track(Track {
            id: "remote_t0".to_string(),
            path: PathBuf::from(
                "rtq://open_subsonic/test-profile/song-001?endpoint=https%3A%2F%2Fmusic.example.com&username=alice&format=flac",
            ),
        });
        manager.playback_playlist.set_playing_track_index(Some(0));
        manager.playback_playlist.set_playing(true);
        manager.started_track_id = Some("remote_t0".to_string());

        manager.handle_technical_metadata_changed(protocol::TechnicalMetadata {
            format: "FLAC".to_string(),
            bitrate_kbps: 850,
            sample_rate_hz: 44_100,
            channel_count: 2,
            duration_ms: 123_000,
            bits_per_sample: 16,
        });

        let _ = wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Audio(protocol::AudioMessage::StopDecoding)
            )
        });
        let message = wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Config(protocol::ConfigMessage::SetRuntimeOutputRate { .. })
            )
        });
        let protocol::Message::Config(protocol::ConfigMessage::SetRuntimeOutputRate {
            sample_rate_hz,
            ..
        }) = message
        else {
            panic!("expected runtime output-rate switch request");
        };
        assert_eq!(sample_rate_hz, 44_100);
        assert_eq!(manager.pending_rate_switch, Some(44_100));
        assert!(manager.pending_rate_switch_play_immediately);
    }

    #[test]
    fn test_reconcile_marks_legacy_remote_track_unavailable_when_disconnected() {
        let (mut manager, mut receiver) = make_direct_manager();
        manager.editing_playlist = Playlist::new();
        manager.editing_playlist.add_track(Track {
            id: "remote_legacy".to_string(),
            path: PathBuf::from(
                "rtq://open_subsonic/test-profile/song-001?ENDPOINT=https%3A%2F%2Fmusic.example.com&USERNAME=alice&FORMAT=flac",
            ),
        });

        manager.reconcile_editing_playlist_remote_availability();

        let metadata_message = wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
            matches!(
                message,
                protocol::Message::Playlist(
                    protocol::PlaylistMessage::TrackMetadataBatchUpdated { .. }
                )
            )
        });
        let protocol::Message::Playlist(protocol::PlaylistMessage::TrackMetadataBatchUpdated {
            updates,
        }) = metadata_message
        else {
            panic!("expected TrackMetadataBatchUpdated");
        };
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].track_id, "remote_legacy");
        assert_eq!(updates[0].summary.title, "Remote track unavailable");

        let unavailable_message =
            wait_for_message(&mut receiver, Duration::from_secs(1), |message| {
                matches!(
                    message,
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackUnavailable { .. })
                )
            });
        let protocol::Message::Playlist(protocol::PlaylistMessage::TrackUnavailable { id, .. }) =
            unavailable_message
        else {
            panic!("expected TrackUnavailable");
        };
        assert_eq!(id, "remote_legacy");
        assert!(manager.unavailable_track_ids.contains("remote_legacy"));
    }
}
