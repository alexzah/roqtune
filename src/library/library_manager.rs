//! Library indexing and query runtime component.
//!
//! This manager maintains a lightweight metadata index for Library mode,
//! handles manual scans, and serves pre-sorted query results over the bus.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, info, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::db_manager::{
    DbManager, FavoriteSyncQueueEntry, LibraryScanState, LibraryTrackMetadataUpdate,
    LibraryTrackScanStub,
};
use crate::integration_uri::parse_opensubsonic_track_uri;
use crate::metadata_tags;
use crate::protocol::{self, IntegrationMessage, LibraryMessage, Message};

const SUPPORTED_AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "wav", "ogg", "flac", "aac", "m4a", "mp4"];
const LIBRARY_SCAN_UPSERT_BATCH_SIZE: usize = 256;
const LIBRARY_SCAN_METADATA_BATCH_SIZE: usize = 128;
const LIBRARY_SCAN_PROGRESS_INTERVAL: usize = 256;
const LIBRARY_SCAN_PLAYBACK_COOPERATE_INTERVAL: usize = 96;
const LIBRARY_SCAN_PLAYBACK_COOPERATE_SLEEP: Duration = Duration::from_millis(1);

struct LibraryTrackMetadata {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    genre: String,
    year: String,
    track_number: String,
}

/// Coordinates library index scans and query responses.
pub struct LibraryManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    db_manager: DbManager,
    library_folders: Vec<String>,
    scan_progress_tx: SyncSender<LibraryMessage>,
    playback_active: bool,
    remote_tracks_by_profile: HashMap<String, Vec<protocol::LibraryTrack>>,
}

impl LibraryManager {
    /// Creates a library manager bound to bus channels and storage backend.
    pub fn new(
        bus_consumer: Receiver<Message>,
        bus_producer: Sender<Message>,
        db_manager: DbManager,
        scan_progress_tx: SyncSender<LibraryMessage>,
        initial_library_config: crate::config::LibraryConfig,
    ) -> Self {
        Self {
            bus_consumer,
            bus_producer,
            db_manager,
            library_folders: initial_library_config.folders,
            scan_progress_tx,
            playback_active: false,
            remote_tracks_by_profile: HashMap::new(),
        }
    }

    fn all_remote_tracks(&self) -> Vec<protocol::LibraryTrack> {
        let mut merged = Vec::new();
        for tracks in self.remote_tracks_by_profile.values() {
            merged.extend(tracks.iter().cloned());
        }
        merged
    }

    fn merged_local_and_remote_tracks(&self) -> Result<Vec<protocol::LibraryTrack>, String> {
        let mut tracks = self
            .db_manager
            .get_library_tracks()
            .map_err(|err| format!("Failed to load tracks: {}", err))?;
        tracks.extend(self.all_remote_tracks());
        tracks.sort_by(|left, right| {
            left.title
                .to_ascii_lowercase()
                .cmp(&right.title.to_ascii_lowercase())
                .then_with(|| {
                    left.artist
                        .to_ascii_lowercase()
                        .cmp(&right.artist.to_ascii_lowercase())
                })
                .then_with(|| {
                    left.album
                        .to_ascii_lowercase()
                        .cmp(&right.album.to_ascii_lowercase())
                })
        });
        Ok(tracks)
    }

    fn is_supported_audio_file(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| {
                SUPPORTED_AUDIO_EXTENSIONS
                    .iter()
                    .any(|supported| ext.eq_ignore_ascii_case(supported))
            })
            .unwrap_or(false)
    }

    fn collect_audio_files_from_folder(folder_path: &Path) -> Vec<PathBuf> {
        let mut pending_directories = vec![folder_path.to_path_buf()];
        let mut tracks = Vec::new();

        while let Some(directory) = pending_directories.pop() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(err) => {
                    debug!(
                        "Library scan: failed to read {}: {}",
                        directory.display(),
                        err
                    );
                    continue;
                }
            };

            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        debug!(
                            "Library scan: failed to read entry in {}: {}",
                            directory.display(),
                            err
                        );
                        continue;
                    }
                };

                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(err) => {
                        debug!(
                            "Library scan: failed to inspect {}: {}",
                            path.display(),
                            err
                        );
                        continue;
                    }
                };

                if file_type.is_dir() {
                    pending_directories.push(path);
                    continue;
                }

                if file_type.is_file() && Self::is_supported_audio_file(&path) {
                    tracks.push(path);
                }
            }
        }

        tracks.sort_unstable();
        tracks
    }

    fn stable_library_track_id(path: &Path) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        format!("lib-{:x}", hasher.finish())
    }

    fn unix_now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    }

    fn favorite_secondary_for_track(track: &protocol::LibraryTrack) -> String {
        let artist = track.artist.trim();
        let album = track.album.trim();
        if !artist.is_empty() && !album.is_empty() {
            format!("{artist} · {album}")
        } else if !artist.is_empty() {
            artist.to_string()
        } else {
            album.to_string()
        }
    }

    fn favorite_from_library_track(
        track: &protocol::LibraryTrack,
    ) -> Option<protocol::FavoriteEntityRef> {
        let (entity_key, remote_profile_id, remote_item_id) =
            if let Some(locator) = parse_opensubsonic_track_uri(track.path.as_path()) {
                (
                    format!("os:{}:song:{}", locator.profile_id, locator.song_id),
                    Some(locator.profile_id),
                    Some(locator.song_id),
                )
            } else if track.path.is_absolute() {
                (format!("file:{}", track.path.to_string_lossy()), None, None)
            } else {
                (format!("uri:{}", track.path.to_string_lossy()), None, None)
            };
        if entity_key.trim().is_empty() {
            return None;
        }
        Some(protocol::FavoriteEntityRef {
            kind: protocol::FavoriteEntityKind::Track,
            entity_key,
            display_primary: track.title.clone(),
            display_secondary: Self::favorite_secondary_for_track(track),
            track_path: Some(track.path.clone()),
            remote_profile_id,
            remote_item_id,
        })
    }

    fn normalize_sort_key(value: &str, fallback: &str) -> String {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return fallback.to_string();
        }
        trimmed.to_ascii_lowercase()
    }

    fn fallback_title_from_path(path: &Path) -> String {
        path.file_stem()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Unknown Title".to_string())
    }

    fn read_library_track_metadata(path: &Path) -> LibraryTrackMetadata {
        let fallback_title = Self::fallback_title_from_path(path);
        let mut metadata = LibraryTrackMetadata {
            title: fallback_title,
            artist: "Unknown Artist".to_string(),
            album: "Unknown Album".to_string(),
            album_artist: String::new(),
            genre: String::new(),
            year: String::new(),
            track_number: String::new(),
        };

        if let Some(parsed) = metadata_tags::read_common_track_metadata(path) {
            let metadata_tags::CommonTrackMetadata {
                title,
                artist,
                album,
                album_artist,
                date: _,
                year,
                genre,
                track_number,
            } = parsed;

            if !title.is_empty() {
                metadata.title = title;
            }
            if !artist.is_empty() {
                metadata.artist = artist;
            }
            if !album.is_empty() {
                metadata.album = album;
            }
            if !album_artist.is_empty() {
                metadata.album_artist = album_artist;
            }
            if !genre.is_empty() {
                metadata.genre = genre;
            }
            if !year.is_empty() {
                metadata.year = year;
            }
            if !track_number.is_empty() {
                metadata.track_number = track_number;
            }
        }

        if metadata.album_artist.is_empty() {
            metadata.album_artist = metadata.artist.clone();
        }
        metadata
    }

    fn file_scan_state(path: &Path) -> (i64, i64) {
        match std::fs::metadata(path) {
            Ok(meta) => {
                let modified_unix_ms = meta
                    .modified()
                    .ok()
                    .and_then(|modified| {
                        modified
                            .duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|duration| duration.as_millis() as i64)
                    })
                    .unwrap_or(0);
                (modified_unix_ms, meta.len() as i64)
            }
            Err(_) => (0, 0),
        }
    }

    fn fallback_scan_stub(
        file_path: &Path,
        path_string: String,
        track_id: String,
        modified_unix_ms: i64,
        file_size_bytes: i64,
        scan_started_unix_ms: i64,
    ) -> LibraryTrackScanStub {
        let title = Self::fallback_title_from_path(file_path);
        let artist = "Unknown Artist".to_string();
        let album = "Unknown Album".to_string();
        let album_artist = artist.clone();
        LibraryTrackScanStub {
            track_id,
            path: path_string,
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            album_artist,
            genre: String::new(),
            year: String::new(),
            track_number: String::new(),
            sort_title: Self::normalize_sort_key(&title, "unknown title"),
            sort_artist: Self::normalize_sort_key(&artist, "unknown artist"),
            sort_album: Self::normalize_sort_key(&album, "unknown album"),
            modified_unix_ms,
            file_size_bytes,
            metadata_ready: false,
            last_scanned_unix_ms: scan_started_unix_ms,
        }
    }

    fn metadata_update_from_file(
        file_path: &Path,
        path_string: String,
        modified_unix_ms: i64,
        file_size_bytes: i64,
        scan_started_unix_ms: i64,
    ) -> LibraryTrackMetadataUpdate {
        let metadata = Self::read_library_track_metadata(file_path);
        LibraryTrackMetadataUpdate {
            path: path_string,
            title: metadata.title.clone(),
            artist: metadata.artist.clone(),
            album: metadata.album.clone(),
            album_artist: metadata.album_artist.clone(),
            genre: metadata.genre.clone(),
            year: metadata.year.clone(),
            track_number: metadata.track_number.clone(),
            sort_title: Self::normalize_sort_key(&metadata.title, "unknown title"),
            sort_artist: Self::normalize_sort_key(&metadata.artist, "unknown artist"),
            sort_album: Self::normalize_sort_key(&metadata.album, "unknown album"),
            modified_unix_ms,
            file_size_bytes,
            metadata_ready: true,
            last_scanned_unix_ms: scan_started_unix_ms,
        }
    }

    fn send_scan_failed(&self, error_text: String) {
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::ScanFailed(error_text)));
    }

    fn push_scan_progress_update(&self, message: LibraryMessage, allow_drop_when_full: bool) {
        let queued = if allow_drop_when_full {
            match self.scan_progress_tx.try_send(message) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => false,
                Err(TrySendError::Disconnected(_)) => false,
            }
        } else {
            self.scan_progress_tx.send(message).is_ok()
        };
        if queued {
            let _ = self
                .bus_producer
                .send(Message::Library(LibraryMessage::DrainScanProgressQueue));
        }
    }

    fn maybe_cooperate_for_playback(&self, processed_units: usize) {
        if self.playback_active
            && processed_units.is_multiple_of(LIBRARY_SCAN_PLAYBACK_COOPERATE_INTERVAL)
        {
            std::thread::sleep(LIBRARY_SCAN_PLAYBACK_COOPERATE_SLEEP);
        }
    }

    fn scan_library(&mut self) {
        self.push_scan_progress_update(LibraryMessage::ScanStarted, false);

        let scan_started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        let existing_scan_states: HashMap<String, LibraryScanState> = match self
            .db_manager
            .get_library_scan_states_by_path()
        {
            Ok(states) => states,
            Err(err) => {
                self.push_scan_progress_update(
                    LibraryMessage::ScanFailed(format!("Failed to load scan baseline: {}", err)),
                    false,
                );
                return;
            }
        };

        let mut all_files = Vec::new();
        for folder in &self.library_folders {
            if folder.trim().is_empty() {
                continue;
            }
            let folder_path = PathBuf::from(folder);
            if !folder_path.exists() {
                warn!(
                    "Library scan: folder does not exist: {}",
                    folder_path.display()
                );
                continue;
            }
            let files = Self::collect_audio_files_from_folder(&folder_path);
            all_files.extend(files);
        }
        all_files.sort_unstable();

        let mut scanned_paths: HashSet<String> = HashSet::new();
        let mut scan_stubs_batch: Vec<LibraryTrackScanStub> =
            Vec::with_capacity(LIBRARY_SCAN_UPSERT_BATCH_SIZE);
        let mut metadata_backfill_targets: Vec<(PathBuf, String, i64, i64)> = Vec::new();
        let mut discovered = 0usize;
        let mut indexed = 0usize;
        let mut metadata_pending = 0usize;

        for file_path in all_files {
            let path_string = file_path.to_string_lossy().to_string();
            let (modified_unix_ms, file_size_bytes) = Self::file_scan_state(&file_path);
            let track_id = Self::stable_library_track_id(&file_path);
            scanned_paths.insert(path_string.clone());
            discovered = discovered.saturating_add(1);

            let needs_metadata = existing_scan_states
                .get(&path_string)
                .map(|state| {
                    state.modified_unix_ms != modified_unix_ms
                        || state.file_size_bytes != file_size_bytes
                        || !state.metadata_ready
                })
                .unwrap_or(true);

            if needs_metadata {
                scan_stubs_batch.push(Self::fallback_scan_stub(
                    &file_path,
                    path_string.clone(),
                    track_id,
                    modified_unix_ms,
                    file_size_bytes,
                    scan_started_unix_ms,
                ));
                metadata_backfill_targets.push((
                    file_path,
                    path_string,
                    modified_unix_ms,
                    file_size_bytes,
                ));
                metadata_pending = metadata_pending.saturating_add(1);
                if scan_stubs_batch.len() >= LIBRARY_SCAN_UPSERT_BATCH_SIZE {
                    if let Err(err) = self
                        .db_manager
                        .upsert_library_track_scan_stub_batch(&scan_stubs_batch)
                    {
                        self.push_scan_progress_update(
                            LibraryMessage::ScanFailed(format!(
                                "Failed to upsert scan batch ({} rows): {}",
                                scan_stubs_batch.len(),
                                err
                            )),
                            false,
                        );
                        return;
                    }
                    indexed = indexed.saturating_add(scan_stubs_batch.len());
                    scan_stubs_batch.clear();
                }
            } else {
                indexed = indexed.saturating_add(1);
            }

            if discovered.is_multiple_of(LIBRARY_SCAN_PROGRESS_INTERVAL) {
                self.push_scan_progress_update(
                    LibraryMessage::ScanProgress {
                        discovered,
                        indexed,
                        metadata_pending,
                    },
                    true,
                );
            }
            self.maybe_cooperate_for_playback(discovered);
        }

        if !scan_stubs_batch.is_empty() {
            if let Err(err) = self
                .db_manager
                .upsert_library_track_scan_stub_batch(&scan_stubs_batch)
            {
                self.push_scan_progress_update(
                    LibraryMessage::ScanFailed(format!(
                        "Failed to upsert final scan batch ({} rows): {}",
                        scan_stubs_batch.len(),
                        err
                    )),
                    false,
                );
                return;
            }
        }

        if let Err(err) = self
            .db_manager
            .delete_library_paths_not_in_set(&scanned_paths)
        {
            self.push_scan_progress_update(
                LibraryMessage::ScanFailed(format!(
                    "Failed to prune removed library files: {}",
                    err
                )),
                false,
            );
            return;
        }

        self.push_scan_progress_update(
            LibraryMessage::ScanCompleted {
                indexed_tracks: scanned_paths.len(),
            },
            false,
        );

        let mut metadata_batch: Vec<LibraryTrackMetadataUpdate> =
            Vec::with_capacity(LIBRARY_SCAN_METADATA_BATCH_SIZE);
        let mut metadata_updated = 0usize;
        let total_pending = metadata_backfill_targets.len();
        for (target_index, (file_path, path_string, modified_unix_ms, file_size_bytes)) in
            metadata_backfill_targets.into_iter().enumerate()
        {
            metadata_batch.push(Self::metadata_update_from_file(
                &file_path,
                path_string,
                modified_unix_ms,
                file_size_bytes,
                scan_started_unix_ms,
            ));
            if metadata_batch.len() >= LIBRARY_SCAN_METADATA_BATCH_SIZE {
                if let Err(err) = self
                    .db_manager
                    .update_library_track_metadata_batch(&metadata_batch)
                {
                    self.push_scan_progress_update(
                        LibraryMessage::ScanFailed(format!(
                            "Failed metadata backfill batch ({} rows): {}",
                            metadata_batch.len(),
                            err
                        )),
                        false,
                    );
                    return;
                }
                metadata_updated = metadata_updated.saturating_add(metadata_batch.len());
                metadata_batch.clear();
                self.push_scan_progress_update(
                    LibraryMessage::MetadataBackfillProgress {
                        updated: metadata_updated,
                        remaining: total_pending.saturating_sub(metadata_updated),
                    },
                    true,
                );
            }
            self.maybe_cooperate_for_playback(target_index.saturating_add(1));
        }

        if !metadata_batch.is_empty() {
            if let Err(err) = self
                .db_manager
                .update_library_track_metadata_batch(&metadata_batch)
            {
                self.push_scan_progress_update(
                    LibraryMessage::ScanFailed(format!(
                        "Failed final metadata backfill batch ({} rows): {}",
                        metadata_batch.len(),
                        err
                    )),
                    false,
                );
                return;
            }
            metadata_updated = metadata_updated.saturating_add(metadata_batch.len());
        }
        if total_pending > 0 {
            self.push_scan_progress_update(
                LibraryMessage::MetadataBackfillProgress {
                    updated: metadata_updated,
                    remaining: total_pending.saturating_sub(metadata_updated),
                },
                true,
            );
        }

        info!(
            "Library scan completed: indexed {} track(s), metadata backfill {} track(s)",
            scanned_paths.len(),
            total_pending
        );
    }

    fn publish_tracks(&self) {
        match self.merged_local_and_remote_tracks() {
            Ok(tracks) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::TracksResult(tracks)));
            }
            Err(err) => self.send_scan_failed(err),
        }
    }

    fn publish_artists(&self) {
        match self.db_manager.get_library_artists() {
            Ok(artists) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::ArtistsResult(artists)));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load artists: {}", err)),
        }
    }

    fn publish_albums(&self) {
        match self.db_manager.get_library_albums() {
            Ok(albums) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::AlbumsResult(albums)));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load albums: {}", err)),
        }
    }

    fn publish_genres(&self) {
        match self.db_manager.get_library_genres() {
            Ok(genres) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::GenresResult(genres)));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load genres: {}", err)),
        }
    }

    fn publish_decades(&self) {
        match self.db_manager.get_library_decades() {
            Ok(decades) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::DecadesResult(decades)));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load decades: {}", err)),
        }
    }

    fn publish_global_search_data(&self) {
        let tracks = match self.merged_local_and_remote_tracks() {
            Ok(tracks) => tracks,
            Err(error_text) => {
                self.send_scan_failed(error_text);
                return;
            }
        };
        let artists = match self.db_manager.get_library_artists() {
            Ok(artists) => artists,
            Err(err) => {
                self.send_scan_failed(format!("Failed to load artists: {}", err));
                return;
            }
        };
        let albums = match self.db_manager.get_library_albums() {
            Ok(albums) => albums,
            Err(err) => {
                self.send_scan_failed(format!("Failed to load albums: {}", err));
                return;
            }
        };

        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::GlobalSearchDataResult {
                tracks,
                artists,
                albums,
            }));
    }

    fn publish_root_counts(&self) {
        let tracks = match self.db_manager.get_library_tracks_count() {
            Ok(count) => count.saturating_add(self.all_remote_tracks().len()),
            Err(err) => {
                warn!("Failed to load tracks count: {}", err);
                return;
            }
        };
        let artists = match self.db_manager.get_library_artists_count() {
            Ok(count) => count,
            Err(err) => {
                warn!("Failed to load artists count: {}", err);
                return;
            }
        };
        let albums = match self.db_manager.get_library_albums_count() {
            Ok(count) => count,
            Err(err) => {
                warn!("Failed to load albums count: {}", err);
                return;
            }
        };
        let genres = match self.db_manager.get_library_genres_count() {
            Ok(count) => count,
            Err(err) => {
                warn!("Failed to load genres count: {}", err);
                return;
            }
        };
        let decades = match self.db_manager.get_library_decades_count() {
            Ok(count) => count,
            Err(err) => {
                warn!("Failed to load decades count: {}", err);
                return;
            }
        };
        let favorites = match self.db_manager.get_favorites_count() {
            Ok(count) => count,
            Err(err) => {
                warn!("Failed to load favorites count: {}", err);
                return;
            }
        };

        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::RootCountsResult {
                tracks,
                artists,
                albums,
                genres,
                decades,
                favorites,
            }));
    }

    fn publish_favorites_snapshot(&self) {
        match self.db_manager.get_all_favorites() {
            Ok(items) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::FavoritesSnapshot {
                            items,
                        }));
            }
            Err(err) => {
                self.send_scan_failed(format!("Failed to load favorites snapshot: {}", err))
            }
        }
    }

    fn publish_favorites_root_page(&self, request_id: u64, offset: usize, limit: usize) {
        let track_count = self
            .db_manager
            .get_favorites_count_by_kind(protocol::FavoriteEntityKind::Track)
            .unwrap_or(0);
        let artist_count = self
            .db_manager
            .get_favorites_count_by_kind(protocol::FavoriteEntityKind::Artist)
            .unwrap_or(0);
        let album_count = self
            .db_manager
            .get_favorites_count_by_kind(protocol::FavoriteEntityKind::Album)
            .unwrap_or(0);
        let all_rows = vec![
            protocol::LibraryEntryPayload::FavoriteCategory(protocol::FavoriteCategory {
                kind: protocol::FavoriteEntityKind::Track,
                title: "Favorite Tracks".to_string(),
                count: track_count,
            }),
            protocol::LibraryEntryPayload::FavoriteCategory(protocol::FavoriteCategory {
                kind: protocol::FavoriteEntityKind::Artist,
                title: "Favorite Artists".to_string(),
                count: artist_count,
            }),
            protocol::LibraryEntryPayload::FavoriteCategory(protocol::FavoriteCategory {
                kind: protocol::FavoriteEntityKind::Album,
                title: "Favorite Albums".to_string(),
                count: album_count,
            }),
        ];
        let total = all_rows.len();
        let entries = all_rows
            .into_iter()
            .skip(offset)
            .take(limit.max(1))
            .collect();
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::LibraryPageResult {
                request_id,
                total,
                entries,
            }));
    }

    fn publish_favorite_entities_page(
        &self,
        request_id: u64,
        kind: protocol::FavoriteEntityKind,
        offset: usize,
        limit: usize,
    ) {
        let result = self
            .db_manager
            .get_favorites_page_by_kind(kind, offset, limit.max(1));
        match result {
            Ok((favorites, total)) => {
                let entries = favorites
                    .into_iter()
                    .map(|favorite| match favorite.kind {
                        protocol::FavoriteEntityKind::Track => {
                            let track_path = favorite.track_path.unwrap_or_else(|| {
                                PathBuf::from(favorite.entity_key.trim_start_matches("file:"))
                            });
                            protocol::LibraryEntryPayload::Track(protocol::LibraryTrack {
                                id: favorite.entity_key.clone(),
                                path: track_path,
                                title: favorite.display_primary.clone(),
                                artist: favorite.display_secondary.clone(),
                                album: String::new(),
                                album_artist: String::new(),
                                genre: String::new(),
                                year: String::new(),
                                track_number: String::new(),
                            })
                        }
                        protocol::FavoriteEntityKind::Artist => {
                            let (album_count, track_count) = match self
                                .db_manager
                                .get_library_artist_detail(&favorite.display_primary)
                            {
                                Ok((albums, tracks)) => (albums.len() as u32, tracks.len() as u32),
                                Err(err) => {
                                    warn!(
                                        "Failed to load artist favorite detail for '{}': {}",
                                        favorite.display_primary, err
                                    );
                                    (0, 0)
                                }
                            };
                            protocol::LibraryEntryPayload::Artist(protocol::LibraryArtist {
                                artist: favorite.display_primary.clone(),
                                album_count,
                                track_count,
                            })
                        }
                        protocol::FavoriteEntityKind::Album => {
                            let (track_count, representative_track_path) =
                                match self.db_manager.get_library_album_tracks(
                                    &favorite.display_primary,
                                    &favorite.display_secondary,
                                ) {
                                    Ok(tracks) => {
                                        let representative_track_path =
                                            tracks.first().map(|track| track.path.clone());
                                        (tracks.len() as u32, representative_track_path)
                                    }
                                    Err(err) => {
                                        warn!(
                                        "Failed to load album favorite detail for '{} / {}': {}",
                                        favorite.display_primary, favorite.display_secondary, err
                                    );
                                        (0, None)
                                    }
                                };
                            protocol::LibraryEntryPayload::Album(protocol::LibraryAlbum {
                                album: favorite.display_primary.clone(),
                                album_artist: favorite.display_secondary.clone(),
                                track_count,
                                representative_track_path,
                            })
                        }
                    })
                    .collect();
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::LibraryPageResult {
                            request_id,
                            total,
                            entries,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load favorites page: {}", err)),
        }
    }

    fn queue_track_favorite_sync(
        &self,
        favorite: &protocol::FavoriteEntityRef,
        favorited: bool,
        updated_unix_ms: i64,
    ) {
        let (Some(remote_profile_id), Some(remote_item_id)) = (
            favorite.remote_profile_id.as_deref(),
            favorite.remote_item_id.as_deref(),
        ) else {
            return;
        };
        if self
            .db_manager
            .upsert_favorite_sync_queue(
                protocol::FavoriteEntityKind::Track,
                &favorite.entity_key,
                remote_profile_id,
                remote_item_id,
                favorited,
                updated_unix_ms,
            )
            .is_err()
        {
            return;
        }
        let _ = self.bus_producer.send(Message::Integration(
            IntegrationMessage::PushOpenSubsonicTrackFavoriteUpdate {
                profile_id: remote_profile_id.to_string(),
                song_id: remote_item_id.to_string(),
                favorited,
                entity_key: favorite.entity_key.clone(),
            },
        ));
    }

    fn apply_toggle_favorite(
        &self,
        favorite: protocol::FavoriteEntityRef,
        desired: Option<bool>,
    ) -> Result<(), String> {
        let is_currently_favorited = self
            .db_manager
            .is_favorited(&favorite.entity_key)
            .map_err(|err| format!("Failed to query favorite state: {}", err))?;
        let next_favorited = desired.unwrap_or(!is_currently_favorited);
        let now = Self::unix_now_ms();
        if next_favorited {
            self.db_manager
                .upsert_favorite(&favorite, "local", now)
                .map_err(|err| format!("Failed to save favorite: {}", err))?;
        } else {
            self.db_manager
                .remove_favorite(&favorite.entity_key)
                .map_err(|err| format!("Failed to remove favorite: {}", err))?;
        }
        if favorite.kind == protocol::FavoriteEntityKind::Track {
            self.queue_track_favorite_sync(&favorite, next_favorited, now);
        }
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::FavoriteStateChanged {
                entity: favorite,
                favorited: next_favorited,
            }));
        self.publish_root_counts();
        self.publish_favorites_snapshot();
        Ok(())
    }

    fn process_pending_favorite_sync_for_profile(&self, profile_id: &str) {
        let queued = match self
            .db_manager
            .list_favorite_sync_queue_for_profile(profile_id)
        {
            Ok(rows) => rows,
            Err(err) => {
                warn!(
                    "Failed to load pending favorite sync queue for profile {}: {}",
                    profile_id, err
                );
                return;
            }
        };
        for FavoriteSyncQueueEntry {
            entity_kind,
            entity_key,
            remote_profile_id,
            remote_item_id,
            desired_favorited,
            ..
        } in queued
        {
            if entity_kind != protocol::FavoriteEntityKind::Track {
                continue;
            }
            let _ = self.bus_producer.send(Message::Integration(
                IntegrationMessage::PushOpenSubsonicTrackFavoriteUpdate {
                    profile_id: remote_profile_id,
                    song_id: remote_item_id,
                    favorited: desired_favorited,
                    entity_key,
                },
            ));
        }
    }

    fn merge_remote_favorite_tracks(
        &self,
        profile_id: &str,
        tracks: &[protocol::LibraryTrack],
    ) -> Result<(), String> {
        let mut favorites = Vec::new();
        for track in tracks {
            if let Some(favorite) = Self::favorite_from_library_track(track) {
                favorites.push(favorite);
            }
        }
        let protected_queue_entries = self
            .db_manager
            .list_favorite_sync_queue_for_profile(profile_id)
            .map_err(|err| format!("Failed to load favorite queue for merge: {}", err))?;
        let protected_entity_keys: HashSet<String> = protected_queue_entries
            .iter()
            .filter(|entry| entry.entity_kind == protocol::FavoriteEntityKind::Track)
            .map(|entry| entry.entity_key.clone())
            .collect();
        self.db_manager
            .replace_remote_track_favorites_for_profile(
                profile_id,
                &favorites,
                &protected_entity_keys,
                Self::unix_now_ms(),
            )
            .map_err(|err| format!("Failed to merge remote favorite tracks: {}", err))?;
        self.publish_root_counts();
        self.publish_favorites_snapshot();
        Ok(())
    }

    fn handle_favorite_sync_result(
        &self,
        profile_id: &str,
        entity_key: &str,
        success: bool,
        error: Option<&str>,
    ) {
        if success {
            let _ = self.db_manager.remove_favorite_sync_queue_entry(
                profile_id,
                protocol::FavoriteEntityKind::Track,
                entity_key,
            );
        } else {
            let _ = self.db_manager.mark_favorite_sync_queue_failure(
                profile_id,
                protocol::FavoriteEntityKind::Track,
                entity_key,
                error.unwrap_or("favorite sync failed"),
                Self::unix_now_ms(),
            );
        }
    }

    fn publish_artist_detail(&self, artist: String) {
        match self.db_manager.get_library_artist_detail(&artist) {
            Ok((albums, tracks)) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::ArtistDetailResult {
                            artist,
                            albums,
                            tracks,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load artist detail: {}", err)),
        }
    }

    fn publish_album_tracks(&self, album: String, album_artist: String) {
        match self
            .db_manager
            .get_library_album_tracks(&album, &album_artist)
        {
            Ok(tracks) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::AlbumTracksResult {
                            album,
                            album_artist,
                            tracks,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load album tracks: {}", err)),
        }
    }

    fn publish_genre_tracks(&self, genre: String) {
        match self.db_manager.get_library_genre_tracks(&genre) {
            Ok(tracks) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::GenreTracksResult {
                            genre,
                            tracks,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load genre tracks: {}", err)),
        }
    }

    fn publish_decade_tracks(&self, decade: String) {
        match self.db_manager.get_library_decade_tracks(&decade) {
            Ok(tracks) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::DecadeTracksResult {
                            decade,
                            tracks,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load decade tracks: {}", err)),
        }
    }

    fn publish_library_page(
        &self,
        request_id: u64,
        view: protocol::LibraryViewQuery,
        offset: usize,
        limit: usize,
        _query: String,
    ) {
        let limit = limit.max(1);
        let result: Result<(usize, Vec<protocol::LibraryEntryPayload>), String> = match view {
            protocol::LibraryViewQuery::Tracks => {
                self.merged_local_and_remote_tracks().map(|rows| {
                    let total = rows.len();
                    let entries = rows
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(protocol::LibraryEntryPayload::Track)
                        .collect();
                    (total, entries)
                })
            }
            protocol::LibraryViewQuery::Artists => self
                .db_manager
                .get_library_artists_page(offset, limit)
                .map(|(rows, total)| {
                    (
                        total,
                        rows.into_iter()
                            .map(protocol::LibraryEntryPayload::Artist)
                            .collect(),
                    )
                })
                .map_err(|err| format!("Failed to load artists page: {}", err)),
            protocol::LibraryViewQuery::Albums => self
                .db_manager
                .get_library_albums_page(offset, limit)
                .map(|(rows, total)| {
                    (
                        total,
                        rows.into_iter()
                            .map(protocol::LibraryEntryPayload::Album)
                            .collect(),
                    )
                })
                .map_err(|err| format!("Failed to load albums page: {}", err)),
            protocol::LibraryViewQuery::Genres => self
                .db_manager
                .get_library_genres_page(offset, limit)
                .map(|(rows, total)| {
                    (
                        total,
                        rows.into_iter()
                            .map(protocol::LibraryEntryPayload::Genre)
                            .collect(),
                    )
                })
                .map_err(|err| format!("Failed to load genres page: {}", err)),
            protocol::LibraryViewQuery::Decades => self
                .db_manager
                .get_library_decades_page(offset, limit)
                .map(|(rows, total)| {
                    (
                        total,
                        rows.into_iter()
                            .map(protocol::LibraryEntryPayload::Decade)
                            .collect(),
                    )
                })
                .map_err(|err| format!("Failed to load decades page: {}", err)),
            protocol::LibraryViewQuery::FavoritesRoot => {
                self.publish_favorites_root_page(request_id, offset, limit);
                return;
            }
            protocol::LibraryViewQuery::FavoriteTracks => {
                self.publish_favorite_entities_page(
                    request_id,
                    protocol::FavoriteEntityKind::Track,
                    offset,
                    limit,
                );
                return;
            }
            protocol::LibraryViewQuery::FavoriteArtists => {
                self.publish_favorite_entities_page(
                    request_id,
                    protocol::FavoriteEntityKind::Artist,
                    offset,
                    limit,
                );
                return;
            }
            protocol::LibraryViewQuery::FavoriteAlbums => {
                self.publish_favorite_entities_page(
                    request_id,
                    protocol::FavoriteEntityKind::Album,
                    offset,
                    limit,
                );
                return;
            }
            protocol::LibraryViewQuery::GlobalSearch => self
                .db_manager
                .get_library_tracks()
                .map(|mut tracks| {
                    tracks.extend(self.all_remote_tracks());
                    let artists = self.db_manager.get_library_artists()?;
                    let albums = self.db_manager.get_library_albums()?;
                    let mut entries: Vec<protocol::LibraryEntryPayload> =
                        Vec::with_capacity(tracks.len() + artists.len() + albums.len());
                    entries.extend(tracks.into_iter().map(protocol::LibraryEntryPayload::Track));
                    entries.extend(
                        artists
                            .into_iter()
                            .map(protocol::LibraryEntryPayload::Artist),
                    );
                    entries.extend(albums.into_iter().map(protocol::LibraryEntryPayload::Album));
                    entries.sort_by(|left, right| {
                        let left_key = match left {
                            protocol::LibraryEntryPayload::Track(track) => {
                                track.title.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Artist(artist) => {
                                artist.artist.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Album(album) => {
                                album.album.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Genre(genre) => {
                                genre.genre.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Decade(decade) => {
                                decade.decade.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::FavoriteCategory(category) => {
                                category.title.to_ascii_lowercase()
                            }
                        };
                        let right_key = match right {
                            protocol::LibraryEntryPayload::Track(track) => {
                                track.title.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Artist(artist) => {
                                artist.artist.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Album(album) => {
                                album.album.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Genre(genre) => {
                                genre.genre.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::Decade(decade) => {
                                decade.decade.to_ascii_lowercase()
                            }
                            protocol::LibraryEntryPayload::FavoriteCategory(category) => {
                                category.title.to_ascii_lowercase()
                            }
                        };
                        let left_kind_rank = match left {
                            protocol::LibraryEntryPayload::Artist(_) => 0,
                            protocol::LibraryEntryPayload::Album(_) => 1,
                            protocol::LibraryEntryPayload::Track(_) => 2,
                            protocol::LibraryEntryPayload::Genre(_) => 3,
                            protocol::LibraryEntryPayload::Decade(_) => 4,
                            protocol::LibraryEntryPayload::FavoriteCategory(_) => 5,
                        };
                        let right_kind_rank = match right {
                            protocol::LibraryEntryPayload::Artist(_) => 0,
                            protocol::LibraryEntryPayload::Album(_) => 1,
                            protocol::LibraryEntryPayload::Track(_) => 2,
                            protocol::LibraryEntryPayload::Genre(_) => 3,
                            protocol::LibraryEntryPayload::Decade(_) => 4,
                            protocol::LibraryEntryPayload::FavoriteCategory(_) => 5,
                        };
                        left_key
                            .cmp(&right_key)
                            .then_with(|| left_kind_rank.cmp(&right_kind_rank))
                    });
                    let total = entries.len();
                    let rows = entries.into_iter().skip(offset).take(limit).collect();
                    Ok((rows, total))
                })
                .and_then(|result| result)
                .map(|(rows, total)| (total, rows))
                .map_err(|err| format!("Failed to load global search page: {}", err)),
            protocol::LibraryViewQuery::ArtistDetail { artist } => self
                .db_manager
                .get_library_artist_detail(&artist)
                .map(|(_albums, tracks)| {
                    let total = tracks.len();
                    let rows = tracks
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(protocol::LibraryEntryPayload::Track)
                        .collect();
                    (total, rows)
                })
                .map_err(|err| format!("Failed to load artist detail page: {}", err)),
            protocol::LibraryViewQuery::AlbumDetail {
                album,
                album_artist,
            } => self
                .db_manager
                .get_library_album_tracks(&album, &album_artist)
                .map(|tracks| {
                    let total = tracks.len();
                    let rows = tracks
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(protocol::LibraryEntryPayload::Track)
                        .collect();
                    (total, rows)
                })
                .map_err(|err| format!("Failed to load album detail page: {}", err)),
            protocol::LibraryViewQuery::GenreDetail { genre } => self
                .db_manager
                .get_library_genre_tracks(&genre)
                .map(|tracks| {
                    let total = tracks.len();
                    let rows = tracks
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(protocol::LibraryEntryPayload::Track)
                        .collect();
                    (total, rows)
                })
                .map_err(|err| format!("Failed to load genre detail page: {}", err)),
            protocol::LibraryViewQuery::DecadeDetail { decade } => self
                .db_manager
                .get_library_decade_tracks(&decade)
                .map(|tracks| {
                    let total = tracks.len();
                    let rows = tracks
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .map(protocol::LibraryEntryPayload::Track)
                        .collect();
                    (total, rows)
                })
                .map_err(|err| format!("Failed to load decade detail page: {}", err)),
        };

        match result {
            Ok((total, entries)) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::LibraryPageResult {
                            request_id,
                            total,
                            entries,
                        }));
            }
            Err(error_text) => self.send_scan_failed(error_text),
        }
    }

    fn resolve_selection_paths(
        &self,
        selections: Vec<protocol::LibrarySelectionSpec>,
    ) -> Result<Vec<PathBuf>, String> {
        let mut resolved_paths = Vec::new();
        let mut seen_paths = HashSet::new();

        for selection in selections {
            match selection {
                protocol::LibrarySelectionSpec::Track { path } => {
                    let dedupe_key = path.to_string_lossy().to_string();
                    if seen_paths.insert(dedupe_key) {
                        resolved_paths.push(path);
                    }
                }
                protocol::LibrarySelectionSpec::Artist { artist } => {
                    let (_albums, tracks) = self
                        .db_manager
                        .get_library_artist_detail(&artist)
                        .map_err(|err| format!("Failed to resolve artist '{}': {}", artist, err))?;
                    for track in tracks {
                        let dedupe_key = track.path.to_string_lossy().to_string();
                        if seen_paths.insert(dedupe_key) {
                            resolved_paths.push(track.path);
                        }
                    }
                }
                protocol::LibrarySelectionSpec::Album {
                    album,
                    album_artist,
                } => {
                    let tracks = self
                        .db_manager
                        .get_library_album_tracks(&album, &album_artist)
                        .map_err(|err| {
                            format!(
                                "Failed to resolve album '{} / {}': {}",
                                album, album_artist, err
                            )
                        })?;
                    for track in tracks {
                        let dedupe_key = track.path.to_string_lossy().to_string();
                        if seen_paths.insert(dedupe_key) {
                            resolved_paths.push(track.path);
                        }
                    }
                }
                protocol::LibrarySelectionSpec::Genre { genre } => {
                    let tracks = self
                        .db_manager
                        .get_library_genre_tracks(&genre)
                        .map_err(|err| format!("Failed to resolve genre '{}': {}", genre, err))?;
                    for track in tracks {
                        let dedupe_key = track.path.to_string_lossy().to_string();
                        if seen_paths.insert(dedupe_key) {
                            resolved_paths.push(track.path);
                        }
                    }
                }
                protocol::LibrarySelectionSpec::Decade { decade } => {
                    let tracks = self
                        .db_manager
                        .get_library_decade_tracks(&decade)
                        .map_err(|err| format!("Failed to resolve decade '{}': {}", decade, err))?;
                    for track in tracks {
                        let dedupe_key = track.path.to_string_lossy().to_string();
                        if seen_paths.insert(dedupe_key) {
                            resolved_paths.push(track.path);
                        }
                    }
                }
            }
        }

        Ok(resolved_paths)
    }

    fn add_selection_to_playlists(
        &self,
        selections: Vec<protocol::LibrarySelectionSpec>,
        playlist_ids: Vec<String>,
    ) {
        if selections.is_empty() {
            let _ = self
                .bus_producer
                .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(
                    "No library items selected".to_string(),
                )));
            return;
        }
        if playlist_ids.is_empty() {
            let _ = self
                .bus_producer
                .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(
                    "No target playlists selected".to_string(),
                )));
            return;
        }

        let paths = match self.resolve_selection_paths(selections) {
            Ok(paths) => paths,
            Err(err) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(err)));
                return;
            }
        };

        if paths.is_empty() {
            let _ = self
                .bus_producer
                .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(
                    "No tracks matched the selected library items".to_string(),
                )));
            return;
        }

        let _ = self.bus_producer.send(Message::Playlist(
            protocol::PlaylistMessage::AddTracksToPlaylists {
                playlist_ids: playlist_ids.clone(),
                paths: paths.clone(),
            },
        ));
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::AddToPlaylistsCompleted {
                playlist_count: playlist_ids.len(),
                track_count: paths.len(),
            }));
    }

    fn paste_selection_to_active_playlist(&self, selections: Vec<protocol::LibrarySelectionSpec>) {
        if selections.is_empty() {
            let _ = self
                .bus_producer
                .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(
                    "No library items selected".to_string(),
                )));
            return;
        }

        let paths = match self.resolve_selection_paths(selections) {
            Ok(paths) => paths,
            Err(err) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(err)));
                return;
            }
        };

        if paths.is_empty() {
            let _ = self
                .bus_producer
                .send(Message::Library(LibraryMessage::AddToPlaylistsFailed(
                    "No tracks matched the selected library items".to_string(),
                )));
            return;
        }

        let _ = self
            .bus_producer
            .send(Message::Playlist(protocol::PlaylistMessage::PasteTracks(
                paths,
            )));
    }

    fn remove_selection_from_library(&self, selections: Vec<protocol::LibrarySelectionSpec>) {
        if selections.is_empty() {
            let _ =
                self.bus_producer
                    .send(Message::Library(LibraryMessage::RemoveSelectionFailed(
                        "No library items selected".to_string(),
                    )));
            return;
        }

        let paths = match self.resolve_selection_paths(selections) {
            Ok(paths) => paths,
            Err(err) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::RemoveSelectionFailed(err)));
                return;
            }
        };

        if paths.is_empty() {
            let _ =
                self.bus_producer
                    .send(Message::Library(LibraryMessage::RemoveSelectionFailed(
                        "No tracks matched the selected library items".to_string(),
                    )));
            return;
        }

        match self.db_manager.delete_library_paths(&paths) {
            Ok(removed_tracks) => {
                let _ = self.bus_producer.send(Message::Library(
                    LibraryMessage::RemoveSelectionCompleted { removed_tracks },
                ));
            }
            Err(err) => {
                let _ = self.bus_producer.send(Message::Library(
                    LibraryMessage::RemoveSelectionFailed(format!(
                        "Failed to remove selected library items: {}",
                        err
                    )),
                ));
            }
        }
    }

    /// Starts the blocking event loop for library scans and query requests.
    pub fn run(&mut self) {
        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(message) => match message {
                    Message::Config(protocol::ConfigMessage::ConfigChanged(changes)) => {
                        for change in changes {
                            if let protocol::ConfigDeltaEntry::Library(library) = change {
                                self.library_folders = library.folders;
                            }
                        }
                    }
                    Message::Playlist(protocol::PlaylistMessage::PlaylistIndicesChanged {
                        is_playing,
                        ..
                    }) => {
                        self.playback_active = is_playing;
                    }
                    Message::Playback(protocol::PlaybackMessage::Play)
                    | Message::Playback(protocol::PlaybackMessage::TrackStarted(_)) => {
                        self.playback_active = true;
                    }
                    Message::Playback(protocol::PlaybackMessage::Pause)
                    | Message::Playback(protocol::PlaybackMessage::Stop) => {
                        self.playback_active = false;
                    }
                    Message::Library(LibraryMessage::RequestScan) => {
                        self.scan_library();
                    }
                    Message::Library(LibraryMessage::RequestRootCounts) => {
                        self.publish_root_counts();
                    }
                    Message::Library(LibraryMessage::RequestFavoritesSnapshot) => {
                        self.publish_favorites_snapshot();
                    }
                    Message::Library(LibraryMessage::RequestTracks) => {
                        self.publish_tracks();
                    }
                    Message::Library(LibraryMessage::RequestArtists) => {
                        self.publish_artists();
                    }
                    Message::Library(LibraryMessage::RequestAlbums) => {
                        self.publish_albums();
                    }
                    Message::Library(LibraryMessage::RequestGenres) => {
                        self.publish_genres();
                    }
                    Message::Library(LibraryMessage::RequestDecades) => {
                        self.publish_decades();
                    }
                    Message::Library(LibraryMessage::RequestGlobalSearchData) => {
                        self.publish_global_search_data();
                    }
                    Message::Library(LibraryMessage::RequestArtistDetail { artist }) => {
                        self.publish_artist_detail(artist);
                    }
                    Message::Library(LibraryMessage::RequestAlbumTracks {
                        album,
                        album_artist,
                    }) => {
                        self.publish_album_tracks(album, album_artist);
                    }
                    Message::Library(LibraryMessage::RequestGenreTracks { genre }) => {
                        self.publish_genre_tracks(genre);
                    }
                    Message::Library(LibraryMessage::RequestDecadeTracks { decade }) => {
                        self.publish_decade_tracks(decade);
                    }
                    Message::Integration(
                        IntegrationMessage::OpenSubsonicLibraryTracksUpdated { profile_id, tracks },
                    ) => {
                        if tracks.is_empty() {
                            self.remote_tracks_by_profile.remove(&profile_id);
                        } else {
                            self.remote_tracks_by_profile.insert(profile_id, tracks);
                        }
                        self.publish_root_counts();
                        self.publish_tracks();
                        self.publish_global_search_data();
                    }
                    Message::Integration(
                        IntegrationMessage::OpenSubsonicFavoriteTracksUpdated {
                            profile_id,
                            tracks,
                        },
                    ) => {
                        if let Err(error) = self.merge_remote_favorite_tracks(&profile_id, &tracks)
                        {
                            warn!(
                                "Failed merging OpenSubsonic favorite tracks for profile {}: {}",
                                profile_id, error
                            );
                        }
                        self.process_pending_favorite_sync_for_profile(&profile_id);
                    }
                    Message::Integration(
                        IntegrationMessage::OpenSubsonicTrackFavoriteUpdateResult {
                            profile_id,
                            entity_key,
                            favorited,
                            success,
                            error,
                            ..
                        },
                    ) => {
                        debug!(
                            "Favorite sync result profile={} key={} favorited={} success={}",
                            profile_id, entity_key, favorited, success
                        );
                        self.handle_favorite_sync_result(
                            &profile_id,
                            &entity_key,
                            success,
                            error.as_deref(),
                        );
                    }
                    Message::Integration(IntegrationMessage::ConnectBackendProfile {
                        profile_id,
                    })
                    | Message::Integration(IntegrationMessage::SyncBackendProfile { profile_id }) =>
                    {
                        self.process_pending_favorite_sync_for_profile(&profile_id);
                    }
                    Message::Library(LibraryMessage::DrainScanProgressQueue) => {}
                    Message::Library(LibraryMessage::RequestLibraryPage {
                        request_id,
                        view,
                        offset,
                        limit,
                        query,
                    }) => {
                        self.publish_library_page(request_id, view, offset, limit, query);
                    }
                    Message::Library(LibraryMessage::RequestEnrichment { .. }) => {}
                    Message::Library(LibraryMessage::ReplaceEnrichmentPrefetchQueue { .. }) => {}
                    Message::Library(LibraryMessage::EnrichmentPrefetchTick) => {}
                    Message::Library(LibraryMessage::LibraryViewportChanged { .. }) => {}
                    Message::Library(LibraryMessage::ScanProgress { .. }) => {}
                    Message::Library(LibraryMessage::MetadataBackfillProgress { .. }) => {}
                    Message::Library(LibraryMessage::AddSelectionToPlaylists {
                        selections,
                        playlist_ids,
                    }) => {
                        self.add_selection_to_playlists(selections, playlist_ids);
                    }
                    Message::Library(LibraryMessage::PasteSelectionToActivePlaylist {
                        selections,
                    }) => {
                        self.paste_selection_to_active_playlist(selections);
                    }
                    Message::Library(LibraryMessage::RemoveSelectionFromLibrary { selections }) => {
                        self.remove_selection_from_library(selections);
                    }
                    Message::Library(LibraryMessage::ToggleFavorite { entity, desired }) => {
                        if let Err(error) = self.apply_toggle_favorite(entity, desired) {
                            warn!("Failed to apply favorite toggle: {}", error);
                        }
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        "LibraryManager lagged on control bus, skipped {} message(s)",
                        skipped
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
