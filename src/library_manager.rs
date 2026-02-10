//! Library indexing and query runtime component.
//!
//! This manager maintains a lightweight metadata index for Library mode,
//! handles manual scans, and serves pre-sorted query results over the bus.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use id3::{Tag, TagLike};
use log::{debug, error, info, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::db_manager::DbManager;
use crate::protocol::{self, LibraryMessage, Message};

const SUPPORTED_AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "wav", "ogg", "flac", "aac", "m4a", "mp4"];

struct LibraryTrackMetadata {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    year: String,
    track_number: String,
}

/// Coordinates library index scans and query responses.
pub struct LibraryManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    db_manager: DbManager,
    library_folders: Vec<String>,
}

impl LibraryManager {
    /// Creates a library manager bound to bus channels and storage backend.
    pub fn new(
        bus_consumer: Receiver<Message>,
        bus_producer: Sender<Message>,
        db_manager: DbManager,
    ) -> Self {
        Self {
            bus_consumer,
            bus_producer,
            db_manager,
            library_folders: Vec::new(),
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

    fn stable_library_song_id(path: &Path) -> String {
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
            year: String::new(),
            track_number: String::new(),
        };

        if let Ok(tag) = Tag::read_from_path(path) {
            if let Some(title) = tag.title().filter(|value| !value.trim().is_empty()) {
                metadata.title = title.to_string();
            }
            if let Some(artist) = tag.artist().filter(|value| !value.trim().is_empty()) {
                metadata.artist = artist.to_string();
            }
            if let Some(album) = tag.album().filter(|value| !value.trim().is_empty()) {
                metadata.album = album.to_string();
            }
            if let Some(album_artist) = tag.album_artist().filter(|value| !value.trim().is_empty())
            {
                metadata.album_artist = album_artist.to_string();
            }
            if let Some(year) = tag.year() {
                metadata.year = year.to_string();
            }
            if let Some(track_number) = tag.track() {
                metadata.track_number = track_number.to_string();
            }
            if !metadata.album_artist.is_empty() {
                return metadata;
            }
            metadata.album_artist = metadata.artist.clone();
            return metadata;
        }

        if let Ok(ape_tag) = ape::read_from_path(path) {
            if let Some(title) = ape_tag.item("title").and_then(|item| {
                let value: Result<&str, _> = item.try_into();
                value.ok()
            }) {
                if !title.trim().is_empty() {
                    metadata.title = title.to_string();
                }
            }
            if let Some(artist) = ape_tag.item("artist").and_then(|item| {
                let value: Result<&str, _> = item.try_into();
                value.ok()
            }) {
                if !artist.trim().is_empty() {
                    metadata.artist = artist.to_string();
                }
            }
            if let Some(album) = ape_tag.item("album").and_then(|item| {
                let value: Result<&str, _> = item.try_into();
                value.ok()
            }) {
                if !album.trim().is_empty() {
                    metadata.album = album.to_string();
                }
            }
            if let Some(album_artist) = ape_tag
                .item("album artist")
                .or_else(|| ape_tag.item("albumartist"))
                .and_then(|item| {
                    let value: Result<&str, _> = item.try_into();
                    value.ok()
                })
            {
                if !album_artist.trim().is_empty() {
                    metadata.album_artist = album_artist.to_string();
                }
            }
            if let Some(year) = ape_tag
                .item("year")
                .or_else(|| ape_tag.item("date"))
                .and_then(|item| {
                    let value: Result<&str, _> = item.try_into();
                    value.ok()
                })
            {
                if !year.trim().is_empty() {
                    metadata.year = year.to_string();
                }
            }
            if let Some(track_number) = ape_tag
                .item("track")
                .or_else(|| ape_tag.item("tracknumber"))
                .and_then(|item| {
                    let value: Result<&str, _> = item.try_into();
                    value.ok()
                })
            {
                if !track_number.trim().is_empty() {
                    metadata.track_number = track_number.to_string();
                }
            }
            if metadata.album_artist.is_empty() {
                metadata.album_artist = metadata.artist.clone();
            }
            return metadata;
        }

        if let Ok(flac_tag) = metaflac::Tag::read_from_path(path) {
            if let Some(title) = flac_tag.get_vorbis("title").and_then(|mut it| it.next()) {
                if !title.trim().is_empty() {
                    metadata.title = title.to_string();
                }
            }
            if let Some(artist) = flac_tag.get_vorbis("artist").and_then(|mut it| it.next()) {
                if !artist.trim().is_empty() {
                    metadata.artist = artist.to_string();
                }
            }
            if let Some(album) = flac_tag.get_vorbis("album").and_then(|mut it| it.next()) {
                if !album.trim().is_empty() {
                    metadata.album = album.to_string();
                }
            }
            if let Some(album_artist) = flac_tag
                .get_vorbis("albumartist")
                .and_then(|mut it| it.next())
                .or_else(|| {
                    flac_tag
                        .get_vorbis("album artist")
                        .and_then(|mut it| it.next())
                })
            {
                if !album_artist.trim().is_empty() {
                    metadata.album_artist = album_artist.to_string();
                }
            }
            if let Some(year) = flac_tag
                .get_vorbis("year")
                .and_then(|mut it| it.next())
                .or_else(|| flac_tag.get_vorbis("date").and_then(|mut it| it.next()))
            {
                if !year.trim().is_empty() {
                    metadata.year = year.to_string();
                }
            }
            if let Some(track_number) = flac_tag
                .get_vorbis("tracknumber")
                .and_then(|mut it| it.next())
                .or_else(|| flac_tag.get_vorbis("track").and_then(|mut it| it.next()))
            {
                if !track_number.trim().is_empty() {
                    metadata.track_number = track_number.to_string();
                }
            }
        }

        if metadata.album_artist.is_empty() {
            metadata.album_artist = metadata.artist.clone();
        }
        metadata
    }

    fn send_scan_failed(&self, error_text: String) {
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::ScanFailed(error_text)));
    }

    fn scan_library(&mut self) {
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::ScanStarted));

        let mut scanned_paths: HashSet<String> = HashSet::new();
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
            for file_path in files {
                let metadata = Self::read_library_track_metadata(&file_path);
                let path_string = file_path.to_string_lossy().to_string();
                let song_id = Self::stable_library_song_id(&file_path);
                let sort_title = Self::normalize_sort_key(&metadata.title, "unknown title");
                let sort_artist = Self::normalize_sort_key(&metadata.artist, "unknown artist");
                let sort_album = Self::normalize_sort_key(&metadata.album, "unknown album");
                let modified_unix_ms = std::fs::metadata(&file_path)
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|modified| {
                        modified
                            .duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|duration| duration.as_millis() as i64)
                    })
                    .unwrap_or_else(|| {
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|duration| duration.as_millis() as i64)
                            .unwrap_or(0)
                    });

                if let Err(err) = self.db_manager.upsert_library_track(
                    &song_id,
                    &path_string,
                    &metadata.title,
                    &metadata.artist,
                    &metadata.album,
                    &metadata.album_artist,
                    &metadata.year,
                    &metadata.track_number,
                    &sort_title,
                    &sort_artist,
                    &sort_album,
                    modified_unix_ms,
                ) {
                    error!(
                        "Library scan: failed to upsert track {}: {}",
                        file_path.display(),
                        err
                    );
                } else {
                    scanned_paths.insert(path_string);
                }
            }
        }

        if let Err(err) = self
            .db_manager
            .delete_library_paths_not_in_set(&scanned_paths)
        {
            self.send_scan_failed(format!("Failed to prune removed library files: {}", err));
            return;
        }

        info!(
            "Library scan completed: indexed {} track(s)",
            scanned_paths.len()
        );
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::ScanCompleted {
                indexed_tracks: scanned_paths.len(),
            }));
    }

    fn publish_songs(&self) {
        match self.db_manager.get_library_songs() {
            Ok(songs) => {
                let _ = self
                    .bus_producer
                    .send(Message::Library(LibraryMessage::SongsResult(songs)));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load songs: {}", err)),
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

    fn publish_artist_detail(&self, artist: String) {
        match self.db_manager.get_library_artist_detail(&artist) {
            Ok((albums, songs)) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::ArtistDetailResult {
                            artist,
                            albums,
                            songs,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load artist detail: {}", err)),
        }
    }

    fn publish_album_songs(&self, album: String, album_artist: String) {
        match self
            .db_manager
            .get_library_album_songs(&album, &album_artist)
        {
            Ok(songs) => {
                let _ =
                    self.bus_producer
                        .send(Message::Library(LibraryMessage::AlbumSongsResult {
                            album,
                            album_artist,
                            songs,
                        }));
            }
            Err(err) => self.send_scan_failed(format!("Failed to load album songs: {}", err)),
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
                    Message::Library(LibraryMessage::RequestScan) => {
                        self.scan_library();
                    }
                    Message::Library(LibraryMessage::RequestSongs) => {
                        self.publish_songs();
                    }
                    Message::Library(LibraryMessage::RequestArtists) => {
                        self.publish_artists();
                    }
                    Message::Library(LibraryMessage::RequestAlbums) => {
                        self.publish_albums();
                    }
                    Message::Library(LibraryMessage::RequestArtistDetail { artist }) => {
                        self.publish_artist_detail(artist);
                    }
                    Message::Library(LibraryMessage::RequestAlbumSongs {
                        album,
                        album_artist,
                    }) => {
                        self.publish_album_songs(album, album_artist);
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
