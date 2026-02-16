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
    DbManager, LibraryScanState, LibraryTrackMetadataUpdate, LibraryTrackScanStub,
};
use crate::metadata_tags;
use crate::protocol::{self, LibraryMessage, Message};

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
}

impl LibraryManager {
    /// Creates a library manager bound to bus channels and storage backend.
    pub fn new(
        bus_consumer: Receiver<Message>,
        bus_producer: Sender<Message>,
        db_manager: DbManager,
        scan_progress_tx: SyncSender<LibraryMessage>,
    ) -> Self {
        Self {
            bus_consumer,
            bus_producer,
            db_manager,
            library_folders: Vec::new(),
            scan_progress_tx,
            playback_active: false,
        }
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
        match self.db_manager.get_library_tracks() {
            Ok(tracks) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::TracksResult(tracks)));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load tracks: {}", err)),
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
        let tracks = match self.db_manager.get_library_tracks() {
            Ok(tracks) => tracks,
            Err(err) => {
                self.send_scan_failed(format!("Failed to load tracks: {}", err));
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
            Ok(count) => count,
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

        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::RootCountsResult {
                tracks,
                artists,
                albums,
                genres,
                decades,
            }));
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
            protocol::LibraryViewQuery::Tracks => self
                .db_manager
                .get_library_tracks_page(offset, limit)
                .map(|(rows, total)| {
                    (
                        total,
                        rows.into_iter()
                            .map(protocol::LibraryEntryPayload::Track)
                            .collect(),
                    )
                })
                .map_err(|err| format!("Failed to load tracks page: {}", err)),
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
            protocol::LibraryViewQuery::GlobalSearch => self
                .db_manager
                .get_library_tracks()
                .and_then(|tracks| {
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
                        };
                        let left_kind_rank = match left {
                            protocol::LibraryEntryPayload::Artist(_) => 0,
                            protocol::LibraryEntryPayload::Album(_) => 1,
                            protocol::LibraryEntryPayload::Track(_) => 2,
                            protocol::LibraryEntryPayload::Genre(_) => 3,
                            protocol::LibraryEntryPayload::Decade(_) => 4,
                        };
                        let right_kind_rank = match right {
                            protocol::LibraryEntryPayload::Artist(_) => 0,
                            protocol::LibraryEntryPayload::Album(_) => 1,
                            protocol::LibraryEntryPayload::Track(_) => 2,
                            protocol::LibraryEntryPayload::Genre(_) => 3,
                            protocol::LibraryEntryPayload::Decade(_) => 4,
                        };
                        left_key
                            .cmp(&right_key)
                            .then_with(|| left_kind_rank.cmp(&right_kind_rank))
                    });
                    let total = entries.len();
                    let rows = entries.into_iter().skip(offset).take(limit).collect();
                    Ok((rows, total))
                })
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
                    Message::Config(protocol::ConfigMessage::ConfigChanged(config)) => {
                        self.library_folders = config.library.folders;
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
                    Message::Library(LibraryMessage::RemoveSelectionFromLibrary { selections }) => {
                        self.remove_selection_from_library(selections);
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
