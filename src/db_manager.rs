//! SQLite-backed persistence for playlists, library index data, and playlist-scoped UI metadata.

use crate::protocol::{
    LibraryAlbum, LibraryArtist, LibraryDecade, LibraryEnrichmentAttemptKind,
    LibraryEnrichmentEntity, LibraryEnrichmentErrorKind, LibraryEnrichmentPayload,
    LibraryEnrichmentStatus, LibraryGenre, LibrarySong, PlaylistColumnWidthOverride, PlaylistInfo,
    RestoredTrack, TrackMetadataSummary,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use uuid::Uuid;

/// Database gateway for playlist and track persistence.
pub struct DbManager {
    conn: Connection,
}

/// Lightweight scan-side state used for change detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryScanState {
    pub modified_unix_ms: i64,
    pub file_size_bytes: i64,
    pub metadata_ready: bool,
}

/// Phase-A scan upsert payload.
#[derive(Debug, Clone)]
pub struct LibraryTrackScanStub {
    pub song_id: String,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub year: String,
    pub track_number: String,
    pub sort_title: String,
    pub sort_artist: String,
    pub sort_album: String,
    pub modified_unix_ms: i64,
    pub file_size_bytes: i64,
    pub metadata_ready: bool,
    pub last_scanned_unix_ms: i64,
}

/// Phase-B metadata backfill update payload.
#[derive(Debug, Clone)]
pub struct LibraryTrackMetadataUpdate {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub year: String,
    pub track_number: String,
    pub sort_title: String,
    pub sort_artist: String,
    pub sort_album: String,
    pub modified_unix_ms: i64,
    pub file_size_bytes: i64,
    pub metadata_ready: bool,
    pub last_scanned_unix_ms: i64,
}

impl DbManager {
    fn enrichment_entity_parts(entity: &LibraryEnrichmentEntity) -> (String, String) {
        match entity {
            LibraryEnrichmentEntity::Artist { artist } => ("artist".to_string(), artist.clone()),
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => (
                "album".to_string(),
                format!("{album}\u{001f}{album_artist}"),
            ),
        }
    }

    fn enrichment_status_to_str(status: LibraryEnrichmentStatus) -> &'static str {
        match status {
            LibraryEnrichmentStatus::Ready => "ready",
            LibraryEnrichmentStatus::NotFound => "not_found",
            LibraryEnrichmentStatus::Disabled => "disabled",
            LibraryEnrichmentStatus::Error => "error",
        }
    }

    fn enrichment_status_from_str(value: &str) -> LibraryEnrichmentStatus {
        match value {
            "ready" => LibraryEnrichmentStatus::Ready,
            "not_found" => LibraryEnrichmentStatus::NotFound,
            "disabled" => LibraryEnrichmentStatus::Disabled,
            "error" => LibraryEnrichmentStatus::Error,
            _ => LibraryEnrichmentStatus::Error,
        }
    }

    fn enrichment_error_kind_to_str(kind: Option<LibraryEnrichmentErrorKind>) -> &'static str {
        match kind {
            Some(LibraryEnrichmentErrorKind::Timeout) => "timeout",
            Some(LibraryEnrichmentErrorKind::RateLimited) => "rate_limited",
            Some(LibraryEnrichmentErrorKind::BudgetExhausted) => "budget_exhausted",
            Some(LibraryEnrichmentErrorKind::Hard) => "hard",
            None => "",
        }
    }

    fn enrichment_error_kind_from_str(value: &str) -> Option<LibraryEnrichmentErrorKind> {
        match value {
            "timeout" => Some(LibraryEnrichmentErrorKind::Timeout),
            "rate_limited" => Some(LibraryEnrichmentErrorKind::RateLimited),
            "budget_exhausted" => Some(LibraryEnrichmentErrorKind::BudgetExhausted),
            "hard" => Some(LibraryEnrichmentErrorKind::Hard),
            _ => None,
        }
    }

    fn enrichment_attempt_kind_to_str(kind: LibraryEnrichmentAttemptKind) -> &'static str {
        match kind {
            LibraryEnrichmentAttemptKind::Detail => "detail",
            LibraryEnrichmentAttemptKind::VisiblePrefetch => "visible_prefetch",
            LibraryEnrichmentAttemptKind::BackgroundWarm => "background_warm",
        }
    }

    fn enrichment_attempt_kind_from_str(value: &str) -> LibraryEnrichmentAttemptKind {
        match value {
            "detail" => LibraryEnrichmentAttemptKind::Detail,
            "background_warm" => LibraryEnrichmentAttemptKind::BackgroundWarm,
            _ => LibraryEnrichmentAttemptKind::VisiblePrefetch,
        }
    }

    fn enrichment_entity_from_parts(
        entity_type: &str,
        entity_key: &str,
    ) -> LibraryEnrichmentEntity {
        match entity_type {
            "artist" => LibraryEnrichmentEntity::Artist {
                artist: entity_key.to_string(),
            },
            "album" => {
                let mut parts = entity_key.splitn(2, '\u{001f}');
                let album = parts.next().unwrap_or_default().to_string();
                let album_artist = parts.next().unwrap_or_default().to_string();
                LibraryEnrichmentEntity::Album {
                    album,
                    album_artist,
                }
            }
            _ => LibraryEnrichmentEntity::Artist {
                artist: entity_key.to_string(),
            },
        }
    }

    fn normalize_library_sort_key(value: &str, fallback: &str) -> String {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return fallback.to_string();
        }
        trimmed.to_ascii_lowercase()
    }

    fn configure_connection_pragmas(conn: &Connection) {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        let _ = conn.pragma_update(None, "temp_store", "MEMORY");
        let _ = conn.pragma_update(None, "foreign_keys", "ON");
        let _ = conn.pragma_update(None, "busy_timeout", 5000i64);
    }

    /// Opens the on-disk database, initializes schema, and applies migrations.
    pub fn new() -> Result<Self, rusqlite::Error> {
        let data_dir = dirs::data_dir()
            .expect("Could not find data directory")
            .join("roqtune");

        if !data_dir.exists() {
            std::fs::create_dir_all(&data_dir).expect("Could not create data directory");
        }

        let db_path = data_dir.join("playlist.db");
        let conn = Connection::open(db_path)?;
        Self::configure_connection_pragmas(&conn);

        let db_manager = Self { conn };
        db_manager.initialize_schema()?;
        db_manager.migrate()?;
        Ok(db_manager)
    }

    #[cfg(test)]
    /// Creates an in-memory database instance for tests.
    pub fn new_in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::configure_connection_pragmas(&conn);
        let db_manager = Self { conn };
        db_manager.initialize_schema()?;
        db_manager.migrate()?;
        Ok(db_manager)
    }

    fn initialize_schema(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS playlists (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                column_order TEXT,
                column_width_overrides TEXT
            )",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS tracks (
                id TEXT PRIMARY KEY,
                playlist_id TEXT NOT NULL,
                path TEXT NOT NULL,
                position INTEGER NOT NULL,
                title TEXT,
                artist TEXT,
                album TEXT,
                date TEXT,
                genre TEXT,
                FOREIGN KEY(playlist_id) REFERENCES playlists(id)
            )",
            [],
        )?;

        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS library_tracks (
                song_id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                artist TEXT NOT NULL,
                album TEXT NOT NULL,
                album_artist TEXT NOT NULL,
                genre TEXT NOT NULL,
                year TEXT NOT NULL,
                track_number TEXT NOT NULL,
                sort_title TEXT NOT NULL,
                sort_artist TEXT NOT NULL,
                sort_album TEXT NOT NULL,
                modified_unix_ms INTEGER NOT NULL DEFAULT 0,
                file_size_bytes INTEGER NOT NULL DEFAULT 0,
                metadata_ready INTEGER NOT NULL DEFAULT 0,
                last_scanned_unix_ms INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_sort_title ON library_tracks(sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_sort_artist ON library_tracks(sort_artist, sort_album, sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_sort_album ON library_tracks(sort_album, sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_artist ON library_tracks(artist, sort_album, sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_album_artist ON library_tracks(album, album_artist, sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_genre ON library_tracks(genre, sort_artist, sort_album, sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_tracks_year ON library_tracks(year, sort_artist, sort_album, sort_title, path)",
            [],
        )?;
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS library_enrichment_cache (
                entity_type TEXT NOT NULL,
                entity_key TEXT NOT NULL,
                status TEXT NOT NULL,
                blurb TEXT NOT NULL,
                image_path TEXT,
                image_url TEXT,
                source_name TEXT NOT NULL,
                source_url TEXT NOT NULL,
                fetched_unix_ms INTEGER NOT NULL DEFAULT 0,
                expires_unix_ms INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                error_kind TEXT NOT NULL DEFAULT '',
                attempt_kind TEXT NOT NULL DEFAULT '',
                conclusive INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY(entity_type, entity_key)
            )",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_library_enrichment_cache_expires ON library_enrichment_cache(expires_unix_ms)",
            [],
        )?;
        Ok(())
    }

    fn migrate(&self) -> Result<(), rusqlite::Error> {
        // Check if we need to add playlist_id column to tracks (for existing databases)
        let mut stmt = self.conn.prepare("PRAGMA table_info(tracks)")?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut has_playlist_id = false;
        for col in columns {
            if col? == "playlist_id" {
                has_playlist_id = true;
                break;
            }
        }

        if !has_playlist_id {
            // This is a migration from the old schema
            self.conn
                .execute("ALTER TABLE tracks ADD COLUMN playlist_id TEXT", [])?;

            // Create default playlist
            let default_id = Uuid::new_v4().to_string();
            self.conn.execute(
                "INSERT INTO playlists (id, name) VALUES (?1, ?2)",
                params![default_id, "Default"],
            )?;

            // Assign all existing tracks to default playlist
            self.conn.execute(
                "UPDATE tracks SET playlist_id = ?1 WHERE playlist_id IS NULL",
                params![default_id],
            )?;
        }

        let mut playlist_stmt = self.conn.prepare("PRAGMA table_info(playlists)")?;
        let playlist_columns = playlist_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut has_column_order = false;
        let mut has_column_width_overrides = false;
        for col in playlist_columns {
            match col?.as_str() {
                "column_order" => has_column_order = true,
                "column_width_overrides" => has_column_width_overrides = true,
                _ => {}
            }
        }
        if !has_column_order {
            self.conn
                .execute("ALTER TABLE playlists ADD COLUMN column_order TEXT", [])?;
        }
        if !has_column_width_overrides {
            self.conn.execute(
                "ALTER TABLE playlists ADD COLUMN column_width_overrides TEXT",
                [],
            )?;
        }

        let mut library_stmt = self.conn.prepare("PRAGMA table_info(library_tracks)")?;
        let library_columns = library_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut has_genre = false;
        let mut has_file_size_bytes = false;
        let mut has_metadata_ready = false;
        let mut has_last_scanned_unix_ms = false;
        for col in library_columns {
            match col?.as_str() {
                "genre" => has_genre = true,
                "file_size_bytes" => has_file_size_bytes = true,
                "metadata_ready" => has_metadata_ready = true,
                "last_scanned_unix_ms" => has_last_scanned_unix_ms = true,
                _ => {}
            }
        }
        if !has_genre {
            self.conn.execute(
                "ALTER TABLE library_tracks ADD COLUMN genre TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_file_size_bytes {
            self.conn.execute(
                "ALTER TABLE library_tracks ADD COLUMN file_size_bytes INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has_metadata_ready {
            self.conn.execute(
                "ALTER TABLE library_tracks ADD COLUMN metadata_ready INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has_last_scanned_unix_ms {
            self.conn.execute(
                "ALTER TABLE library_tracks ADD COLUMN last_scanned_unix_ms INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }

        let mut enrichment_stmt = self
            .conn
            .prepare("PRAGMA table_info(library_enrichment_cache)")?;
        let enrichment_columns = enrichment_stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut has_error_kind = false;
        let mut has_attempt_kind = false;
        let mut has_conclusive = false;
        for col in enrichment_columns {
            match col?.as_str() {
                "error_kind" => has_error_kind = true,
                "attempt_kind" => has_attempt_kind = true,
                "conclusive" => has_conclusive = true,
                _ => {}
            }
        }
        if !has_error_kind {
            self.conn.execute(
                "ALTER TABLE library_enrichment_cache ADD COLUMN error_kind TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_attempt_kind {
            self.conn.execute(
                "ALTER TABLE library_enrichment_cache ADD COLUMN attempt_kind TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        if !has_conclusive {
            self.conn.execute(
                "ALTER TABLE library_enrichment_cache ADD COLUMN conclusive INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }

        // Ensure at least one playlist exists
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM playlists", [], |r| r.get(0))?;
        if count == 0 {
            let default_id = Uuid::new_v4().to_string();
            self.conn.execute(
                "INSERT INTO playlists (id, name) VALUES (?1, ?2)",
                params![default_id, "Default"],
            )?;
        }

        Ok(())
    }

    /// Inserts a playlist record with a caller-supplied id.
    pub fn create_playlist(&self, id: &str, name: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO playlists (id, name) VALUES (?1, ?2)",
            params![id, name],
        )?;
        Ok(())
    }

    /// Renames an existing playlist.
    pub fn rename_playlist(&self, id: &str, name: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE playlists SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    /// Returns all playlists currently stored in the database.
    pub fn get_all_playlists(&self) -> Result<Vec<PlaylistInfo>, rusqlite::Error> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM playlists")?;
        let playlist_iter = stmt.query_map([], |row| {
            Ok(PlaylistInfo {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;

        let mut playlists = Vec::new();
        for playlist in playlist_iter {
            playlists.push(playlist?);
        }
        Ok(playlists)
    }

    /// Persists one track row in the given playlist at the provided position.
    pub fn save_track(
        &self,
        id: &str,
        playlist_id: &str,
        path: &str,
        position: usize,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO tracks (id, playlist_id, path, position) VALUES (?1, ?2, ?3, ?4)",
            params![id, playlist_id, path, position as i64],
        )?;
        Ok(())
    }

    /// Persists many track rows for one playlist in one transaction.
    pub fn save_tracks_batch(
        &self,
        playlist_id: &str,
        tracks: &[(String, PathBuf)],
        base_position: usize,
    ) -> Result<(), rusqlite::Error> {
        if tracks.is_empty() {
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE TRANSACTION", [])?;
        let mut stmt = match self
            .conn
            .prepare("INSERT INTO tracks (id, playlist_id, path, position) VALUES (?1, ?2, ?3, ?4)")
        {
            Ok(stmt) => stmt,
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        };
        for (offset, (id, path)) in tracks.iter().enumerate() {
            if let Err(err) = stmt.execute(params![
                id,
                playlist_id,
                path.to_string_lossy().to_string(),
                (base_position + offset) as i64
            ]) {
                drop(stmt);
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        }
        drop(stmt);
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }

    /// Deletes one track by id.
    pub fn delete_track(&self, id: &str) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Deletes a playlist and all tracks that belong to it.
    pub fn delete_playlist(&self, id: &str) -> Result<(), rusqlite::Error> {
        // Delete tracks first due to foreign key (even if not enforced, it's good practice)
        self.conn
            .execute("DELETE FROM tracks WHERE playlist_id = ?1", params![id])?;
        self.conn
            .execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Loads tracks for one playlist ordered by stored position.
    pub fn get_tracks_for_playlist(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<RestoredTrack>, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path FROM tracks WHERE playlist_id = ?1 ORDER BY position ASC")?;
        let track_iter = stmt.query_map(params![playlist_id], |row| {
            Ok(RestoredTrack {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
            })
        })?;

        let mut tracks = Vec::new();
        for track in track_iter {
            tracks.push(track?);
        }
        Ok(tracks)
    }

    /// Rewrites positional ordering for the supplied track ids.
    pub fn update_positions(&self, ids: Vec<String>) -> Result<(), rusqlite::Error> {
        if ids.is_empty() {
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE TRANSACTION", [])?;
        let mut stmt = match self
            .conn
            .prepare("UPDATE tracks SET position = ?1 WHERE id = ?2")
        {
            Ok(stmt) => stmt,
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        };
        for (i, id) in ids.iter().enumerate() {
            if let Err(err) = stmt.execute(params![i as i64, id]) {
                drop(stmt);
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        }
        drop(stmt);
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }

    /// Persists a complete playlist column ordering payload.
    pub fn set_playlist_column_order(
        &self,
        playlist_id: &str,
        column_order: &[String],
    ) -> Result<(), rusqlite::Error> {
        let serialized_order = serde_json::to_string(column_order)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
        self.conn.execute(
            "UPDATE playlists SET column_order = ?1 WHERE id = ?2",
            params![serialized_order, playlist_id],
        )?;
        Ok(())
    }

    /// Loads the saved column ordering for a playlist, if present.
    pub fn get_playlist_column_order(
        &self,
        playlist_id: &str,
    ) -> Result<Option<Vec<String>>, rusqlite::Error> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT column_order FROM playlists WHERE id = ?1",
                params![playlist_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();

        if let Some(raw) = raw {
            if raw.trim().is_empty() {
                return Ok(None);
            }
            if let Ok(parsed) = serde_json::from_str::<Vec<String>>(&raw) {
                return Ok(Some(parsed));
            }
        }
        Ok(None)
    }

    /// Persists the full set of width overrides for a playlist.
    pub fn set_playlist_column_width_overrides(
        &self,
        playlist_id: &str,
        overrides: &[PlaylistColumnWidthOverride],
    ) -> Result<(), rusqlite::Error> {
        let serialized_overrides = if overrides.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(overrides)
                    .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
            )
        };
        self.conn.execute(
            "UPDATE playlists SET column_width_overrides = ?1 WHERE id = ?2",
            params![serialized_overrides, playlist_id],
        )?;
        Ok(())
    }

    /// Loads all persisted width overrides for a playlist, if present.
    pub fn get_playlist_column_width_overrides(
        &self,
        playlist_id: &str,
    ) -> Result<Option<Vec<PlaylistColumnWidthOverride>>, rusqlite::Error> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT column_width_overrides FROM playlists WHERE id = ?1",
                params![playlist_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();

        if let Some(raw) = raw {
            if raw.trim().is_empty() {
                return Ok(None);
            }
            if let Ok(parsed) = serde_json::from_str::<Vec<PlaylistColumnWidthOverride>>(&raw) {
                return Ok(Some(parsed));
            }
        }
        Ok(None)
    }

    /// Upserts one width override entry by column key.
    pub fn set_playlist_column_width_override(
        &self,
        playlist_id: &str,
        column_key: &str,
        width_px: u32,
    ) -> Result<(), rusqlite::Error> {
        let mut overrides = self
            .get_playlist_column_width_overrides(playlist_id)?
            .unwrap_or_default();
        if let Some(existing) = overrides
            .iter_mut()
            .find(|item| item.column_key == column_key)
        {
            existing.width_px = width_px;
        } else {
            overrides.push(PlaylistColumnWidthOverride {
                column_key: column_key.to_string(),
                width_px,
            });
        }
        self.set_playlist_column_width_overrides(playlist_id, &overrides)
    }

    /// Deletes one width override entry by column key.
    pub fn clear_playlist_column_width_override(
        &self,
        playlist_id: &str,
        column_key: &str,
    ) -> Result<(), rusqlite::Error> {
        let mut overrides = self
            .get_playlist_column_width_overrides(playlist_id)?
            .unwrap_or_default();
        overrides.retain(|item| item.column_key != column_key);
        self.set_playlist_column_width_overrides(playlist_id, &overrides)
    }

    /// Batch upserts many scan stubs in one transaction.
    pub fn upsert_library_track_scan_stub_batch(
        &self,
        stubs: &[LibraryTrackScanStub],
    ) -> Result<(), rusqlite::Error> {
        if stubs.is_empty() {
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE TRANSACTION", [])?;
        let mut stmt = match self.conn.prepare(
            "INSERT INTO library_tracks (
                song_id, path, title, artist, album, album_artist, genre, year, track_number,
                sort_title, sort_artist, sort_album, modified_unix_ms, file_size_bytes,
                metadata_ready, last_scanned_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(path) DO UPDATE SET
                song_id = excluded.song_id,
                title = excluded.title,
                artist = excluded.artist,
                album = excluded.album,
                album_artist = excluded.album_artist,
                genre = excluded.genre,
                year = excluded.year,
                track_number = excluded.track_number,
                sort_title = excluded.sort_title,
                sort_artist = excluded.sort_artist,
                sort_album = excluded.sort_album,
                modified_unix_ms = excluded.modified_unix_ms,
                file_size_bytes = excluded.file_size_bytes,
                metadata_ready = excluded.metadata_ready,
                last_scanned_unix_ms = excluded.last_scanned_unix_ms",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        };
        for stub in stubs {
            if let Err(err) = stmt.execute(params![
                stub.song_id,
                stub.path,
                stub.title,
                stub.artist,
                stub.album,
                stub.album_artist,
                stub.genre,
                stub.year,
                stub.track_number,
                stub.sort_title,
                stub.sort_artist,
                stub.sort_album,
                stub.modified_unix_ms,
                stub.file_size_bytes,
                i64::from(stub.metadata_ready),
                stub.last_scanned_unix_ms,
            ]) {
                drop(stmt);
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        }
        drop(stmt);
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }

    /// Loads lightweight scan state for all indexed library tracks.
    pub fn get_library_scan_states_by_path(
        &self,
    ) -> Result<HashMap<String, LibraryScanState>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT path, modified_unix_ms, file_size_bytes, metadata_ready FROM library_tracks",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                LibraryScanState {
                    modified_unix_ms: row.get(1)?,
                    file_size_bytes: row.get(2)?,
                    metadata_ready: row.get::<_, i64>(3)? != 0,
                },
            ))
        })?;
        let mut map = HashMap::new();
        for item in iter {
            let (path, state) = item?;
            map.insert(path, state);
        }
        Ok(map)
    }

    /// Batch-updates rich metadata for scanned tracks.
    pub fn update_library_track_metadata_batch(
        &self,
        updates: &[LibraryTrackMetadataUpdate],
    ) -> Result<(), rusqlite::Error> {
        if updates.is_empty() {
            return Ok(());
        }
        self.conn.execute("BEGIN IMMEDIATE TRANSACTION", [])?;
        let mut stmt = match self.conn.prepare(
            "UPDATE library_tracks
             SET title = ?1,
                 artist = ?2,
                 album = ?3,
                 album_artist = ?4,
                 genre = ?5,
                 year = ?6,
                 track_number = ?7,
                 sort_title = ?8,
                 sort_artist = ?9,
                 sort_album = ?10,
                 modified_unix_ms = ?11,
                 file_size_bytes = ?12,
                 metadata_ready = ?13,
                 last_scanned_unix_ms = ?14
             WHERE path = ?15",
        ) {
            Ok(stmt) => stmt,
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        };
        for update in updates {
            if let Err(err) = stmt.execute(params![
                update.title,
                update.artist,
                update.album,
                update.album_artist,
                update.genre,
                update.year,
                update.track_number,
                update.sort_title,
                update.sort_artist,
                update.sort_album,
                update.modified_unix_ms,
                update.file_size_bytes,
                i64::from(update.metadata_ready),
                update.last_scanned_unix_ms,
                update.path,
            ]) {
                drop(stmt);
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        }
        drop(stmt);
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }

    /// Updates metadata columns for one existing indexed library track path.
    pub fn update_library_track_metadata_by_path(
        &self,
        path: &str,
        summary: &TrackMetadataSummary,
    ) -> Result<bool, rusqlite::Error> {
        let sort_title = Self::normalize_library_sort_key(&summary.title, "unknown title");
        let sort_artist = Self::normalize_library_sort_key(&summary.artist, "unknown artist");
        let sort_album = Self::normalize_library_sort_key(&summary.album, "unknown album");
        let modified_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);

        let updated = self.conn.execute(
            "UPDATE library_tracks
             SET title = ?1,
                 artist = ?2,
                 album = ?3,
                 album_artist = ?4,
                 genre = ?5,
                 year = ?6,
                 track_number = ?7,
                 sort_title = ?8,
                 sort_artist = ?9,
                 sort_album = ?10,
                 modified_unix_ms = ?11,
                 metadata_ready = 1,
                 last_scanned_unix_ms = ?12
             WHERE path = ?13",
            params![
                summary.title,
                summary.artist,
                summary.album,
                summary.album_artist,
                summary.genre,
                summary.year,
                summary.track_number,
                sort_title,
                sort_artist,
                sort_album,
                modified_unix_ms,
                modified_unix_ms,
                path
            ],
        )?;
        Ok(updated > 0)
    }

    /// Deletes indexed library rows that are no longer present in scanned paths.
    pub fn delete_library_paths_not_in_set(
        &self,
        keep_paths: &HashSet<String>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute("BEGIN IMMEDIATE TRANSACTION", [])?;
        if let Err(err) = self.conn.execute(
            "CREATE TEMP TABLE IF NOT EXISTS tmp_seen_library_paths (path TEXT PRIMARY KEY)",
            [],
        ) {
            let _ = self.conn.execute("ROLLBACK", []);
            return Err(err);
        }
        if let Err(err) = self.conn.execute("DELETE FROM tmp_seen_library_paths", []) {
            let _ = self.conn.execute("ROLLBACK", []);
            return Err(err);
        }
        let mut insert_stmt = match self
            .conn
            .prepare("INSERT OR IGNORE INTO tmp_seen_library_paths (path) VALUES (?1)")
        {
            Ok(stmt) => stmt,
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        };
        for path in keep_paths {
            if let Err(err) = insert_stmt.execute(params![path]) {
                drop(insert_stmt);
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        }
        drop(insert_stmt);
        if let Err(err) = self.conn.execute(
            "DELETE FROM library_tracks
             WHERE path NOT IN (SELECT path FROM tmp_seen_library_paths)",
            [],
        ) {
            let _ = self.conn.execute("ROLLBACK", []);
            return Err(err);
        }
        if let Err(err) = self.conn.execute("DROP TABLE tmp_seen_library_paths", []) {
            let _ = self.conn.execute("ROLLBACK", []);
            return Err(err);
        }
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }

    /// Deletes indexed library rows for the provided concrete file paths.
    pub fn delete_library_paths(&self, paths: &[PathBuf]) -> Result<usize, rusqlite::Error> {
        if paths.is_empty() {
            return Ok(0);
        }

        self.conn.execute("BEGIN IMMEDIATE TRANSACTION", [])?;
        let mut stmt = match self
            .conn
            .prepare("DELETE FROM library_tracks WHERE path = ?1")
        {
            Ok(stmt) => stmt,
            Err(err) => {
                let _ = self.conn.execute("ROLLBACK", []);
                return Err(err);
            }
        };

        let mut deleted = 0usize;
        let mut seen_paths = HashSet::new();
        for path in paths {
            let key = path.to_string_lossy().to_string();
            if !seen_paths.insert(key.clone()) {
                continue;
            }
            match stmt.execute(params![key]) {
                Ok(changed) => {
                    deleted = deleted.saturating_add(changed);
                }
                Err(err) => {
                    drop(stmt);
                    let _ = self.conn.execute("ROLLBACK", []);
                    return Err(err);
                }
            }
        }

        drop(stmt);
        self.conn.execute("COMMIT", [])?;
        Ok(deleted)
    }

    /// Loads all tracks in library sorted alphabetically by title.
    pub fn get_library_songs(&self) -> Result<Vec<LibrarySong>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT song_id, path, title, artist, album, album_artist, genre, year, track_number
             FROM library_tracks
             ORDER BY sort_title ASC, path ASC",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok(LibrarySong {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                album_artist: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                track_number: row.get(8)?,
            })
        })?;
        let mut tracks = Vec::new();
        for item in iter {
            tracks.push(item?);
        }
        Ok(tracks)
    }

    /// Loads one tracks page and total row count.
    pub fn get_library_songs_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibrarySong>, usize), rusqlite::Error> {
        let total = self.get_library_songs_count()?;
        let mut stmt = self.conn.prepare(
            "SELECT song_id, path, title, artist, album, album_artist, genre, year, track_number
             FROM library_tracks
             ORDER BY sort_title ASC, path ASC
             LIMIT ?1 OFFSET ?2",
        )?;
        let iter = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(LibrarySong {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                album_artist: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                track_number: row.get(8)?,
            })
        })?;
        let mut rows = Vec::new();
        for item in iter {
            rows.push(item?);
        }
        Ok((rows, total))
    }

    /// Returns total indexed song count.
    pub fn get_library_songs_count(&self) -> Result<usize, rusqlite::Error> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM library_tracks", [], |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }

    /// Loads all unique artists with album/song counts.
    pub fn get_library_artists(&self) -> Result<Vec<LibraryArtist>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT artist, COUNT(DISTINCT album || '|' || album_artist) AS album_count, COUNT(*) AS song_count
             FROM library_tracks
             GROUP BY artist
             ORDER BY sort_artist ASC, artist ASC",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok(LibraryArtist {
                artist: row.get(0)?,
                album_count: row.get::<_, i64>(1)?.max(0) as u32,
                song_count: row.get::<_, i64>(2)?.max(0) as u32,
            })
        })?;
        let mut artists = Vec::new();
        for item in iter {
            artists.push(item?);
        }
        Ok(artists)
    }

    /// Loads one artists page and total row count.
    pub fn get_library_artists_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibraryArtist>, usize), rusqlite::Error> {
        let total = self.get_library_artists_count()?;
        let mut stmt = self.conn.prepare(
            "SELECT artist, COUNT(DISTINCT album || '|' || album_artist) AS album_count, COUNT(*) AS song_count
             FROM library_tracks
             GROUP BY artist
             ORDER BY sort_artist ASC, artist ASC
             LIMIT ?1 OFFSET ?2",
        )?;
        let iter = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(LibraryArtist {
                artist: row.get(0)?,
                album_count: row.get::<_, i64>(1)?.max(0) as u32,
                song_count: row.get::<_, i64>(2)?.max(0) as u32,
            })
        })?;
        let mut rows = Vec::new();
        for item in iter {
            rows.push(item?);
        }
        Ok((rows, total))
    }

    /// Returns total artist aggregate row count.
    pub fn get_library_artists_count(&self) -> Result<usize, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (SELECT artist FROM library_tracks GROUP BY artist)",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Loads all unique albums with song counts.
    pub fn get_library_albums(&self) -> Result<Vec<LibraryAlbum>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT album, album_artist, COUNT(*) AS song_count, MIN(path) AS representative_track_path
             FROM library_tracks
             GROUP BY album, album_artist
             ORDER BY sort_album ASC, album ASC, album_artist ASC",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok(LibraryAlbum {
                album: row.get(0)?,
                album_artist: row.get(1)?,
                song_count: row.get::<_, i64>(2)?.max(0) as u32,
                representative_track_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            })
        })?;
        let mut albums = Vec::new();
        for item in iter {
            albums.push(item?);
        }
        Ok(albums)
    }

    /// Loads one albums page and total row count.
    pub fn get_library_albums_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibraryAlbum>, usize), rusqlite::Error> {
        let total = self.get_library_albums_count()?;
        let mut stmt = self.conn.prepare(
            "SELECT album, album_artist, COUNT(*) AS song_count, MIN(path) AS representative_track_path
             FROM library_tracks
             GROUP BY album, album_artist
             ORDER BY sort_album ASC, album ASC, album_artist ASC
             LIMIT ?1 OFFSET ?2",
        )?;
        let iter = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(LibraryAlbum {
                album: row.get(0)?,
                album_artist: row.get(1)?,
                song_count: row.get::<_, i64>(2)?.max(0) as u32,
                representative_track_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            })
        })?;
        let mut rows = Vec::new();
        for item in iter {
            rows.push(item?);
        }
        Ok((rows, total))
    }

    /// Returns total album aggregate row count.
    pub fn get_library_albums_count(&self) -> Result<usize, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (SELECT album, album_artist FROM library_tracks GROUP BY album, album_artist)",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Loads all unique genres with song counts.
    pub fn get_library_genres(&self) -> Result<Vec<LibraryGenre>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT
                CASE
                    WHEN TRIM(genre) = '' THEN 'Unknown Genre'
                    ELSE TRIM(genre)
                END AS display_genre,
                COUNT(*) AS song_count
             FROM library_tracks
             GROUP BY display_genre
             ORDER BY LOWER(display_genre) ASC",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok(LibraryGenre {
                genre: row.get(0)?,
                song_count: row.get::<_, i64>(1)?.max(0) as u32,
            })
        })?;
        let mut genres = Vec::new();
        for item in iter {
            genres.push(item?);
        }
        Ok(genres)
    }

    /// Loads one genres page and total row count.
    pub fn get_library_genres_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibraryGenre>, usize), rusqlite::Error> {
        let total = self.get_library_genres_count()?;
        let mut stmt = self.conn.prepare(
            "SELECT
                CASE
                    WHEN TRIM(genre) = '' THEN 'Unknown Genre'
                    ELSE TRIM(genre)
                END AS display_genre,
                COUNT(*) AS song_count
             FROM library_tracks
             GROUP BY display_genre
             ORDER BY LOWER(display_genre) ASC
             LIMIT ?1 OFFSET ?2",
        )?;
        let iter = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(LibraryGenre {
                genre: row.get(0)?,
                song_count: row.get::<_, i64>(1)?.max(0) as u32,
            })
        })?;
        let mut rows = Vec::new();
        for item in iter {
            rows.push(item?);
        }
        Ok((rows, total))
    }

    /// Returns total genre aggregate row count.
    pub fn get_library_genres_count(&self) -> Result<usize, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT CASE WHEN TRIM(genre) = '' THEN 'Unknown Genre' ELSE TRIM(genre) END AS display_genre
                FROM library_tracks GROUP BY display_genre
            )",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Loads all unique decades with song counts.
    pub fn get_library_decades(&self) -> Result<Vec<LibraryDecade>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT
                CASE
                    WHEN SUBSTR(TRIM(year), 1, 3) GLOB '[0-9][0-9][0-9]'
                        THEN SUBSTR(TRIM(year), 1, 3) || '0s'
                    ELSE 'Unknown Decade'
                END AS display_decade,
                COUNT(*) AS song_count
             FROM library_tracks
             GROUP BY display_decade
             ORDER BY display_decade ASC",
        )?;
        let iter = stmt.query_map([], |row| {
            Ok(LibraryDecade {
                decade: row.get(0)?,
                song_count: row.get::<_, i64>(1)?.max(0) as u32,
            })
        })?;
        let mut decades = Vec::new();
        for item in iter {
            decades.push(item?);
        }
        Ok(decades)
    }

    /// Loads one decades page and total row count.
    pub fn get_library_decades_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LibraryDecade>, usize), rusqlite::Error> {
        let total = self.get_library_decades_count()?;
        let mut stmt = self.conn.prepare(
            "SELECT
                CASE
                    WHEN SUBSTR(TRIM(year), 1, 3) GLOB '[0-9][0-9][0-9]'
                        THEN SUBSTR(TRIM(year), 1, 3) || '0s'
                    ELSE 'Unknown Decade'
                END AS display_decade,
                COUNT(*) AS song_count
             FROM library_tracks
             GROUP BY display_decade
             ORDER BY display_decade ASC
             LIMIT ?1 OFFSET ?2",
        )?;
        let iter = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(LibraryDecade {
                decade: row.get(0)?,
                song_count: row.get::<_, i64>(1)?.max(0) as u32,
            })
        })?;
        let mut rows = Vec::new();
        for item in iter {
            rows.push(item?);
        }
        Ok((rows, total))
    }

    /// Returns total decade aggregate row count.
    pub fn get_library_decades_count(&self) -> Result<usize, rusqlite::Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT CASE
                    WHEN SUBSTR(TRIM(year), 1, 3) GLOB '[0-9][0-9][0-9]'
                        THEN SUBSTR(TRIM(year), 1, 3) || '0s'
                    ELSE 'Unknown Decade'
                END AS display_decade
                FROM library_tracks GROUP BY display_decade
            )",
            [],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Loads tracks for one album+album-artist pair sorted by track number then title.
    pub fn get_library_album_songs(
        &self,
        album: &str,
        album_artist: &str,
    ) -> Result<Vec<LibrarySong>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT song_id, path, title, artist, album, album_artist, genre, year, track_number
             FROM library_tracks
             WHERE album = ?1 AND album_artist = ?2
             ORDER BY CAST(track_number AS INTEGER) ASC, sort_title ASC, path ASC",
        )?;
        let iter = stmt.query_map(params![album, album_artist], |row| {
            Ok(LibrarySong {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                album_artist: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                track_number: row.get(8)?,
            })
        })?;
        let mut tracks = Vec::new();
        for item in iter {
            tracks.push(item?);
        }
        Ok(tracks)
    }

    /// Loads artist detail (albums and tracks) for one artist.
    pub fn get_library_artist_detail(
        &self,
        artist: &str,
    ) -> Result<(Vec<LibraryAlbum>, Vec<LibrarySong>), rusqlite::Error> {
        let mut album_stmt = self.conn.prepare(
            "SELECT album, album_artist, COUNT(*) AS song_count, MIN(path) AS representative_track_path
             FROM library_tracks
             WHERE artist = ?1 OR album_artist = ?1
             GROUP BY album, album_artist
             ORDER BY sort_album ASC, album ASC",
        )?;
        let album_iter = album_stmt.query_map(params![artist], |row| {
            Ok(LibraryAlbum {
                album: row.get(0)?,
                album_artist: row.get(1)?,
                song_count: row.get::<_, i64>(2)?.max(0) as u32,
                representative_track_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            })
        })?;
        let mut albums = Vec::new();
        for item in album_iter {
            albums.push(item?);
        }

        let mut song_stmt = self.conn.prepare(
            "SELECT song_id, path, title, artist, album, album_artist, genre, year, track_number
             FROM library_tracks
             WHERE artist = ?1 OR album_artist = ?1
             ORDER BY sort_album ASC, CAST(track_number AS INTEGER) ASC, sort_title ASC, path ASC",
        )?;
        let song_iter = song_stmt.query_map(params![artist], |row| {
            Ok(LibrarySong {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                album_artist: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                track_number: row.get(8)?,
            })
        })?;
        let mut tracks = Vec::new();
        for item in song_iter {
            tracks.push(item?);
        }
        Ok((albums, tracks))
    }

    /// Loads tracks for one normalized genre label.
    pub fn get_library_genre_songs(
        &self,
        genre: &str,
    ) -> Result<Vec<LibrarySong>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT song_id, path, title, artist, album, album_artist, genre, year, track_number
             FROM library_tracks
             WHERE CASE
                 WHEN TRIM(genre) = '' THEN 'Unknown Genre'
                 ELSE TRIM(genre)
             END = ?1
             ORDER BY sort_artist ASC, sort_album ASC, CAST(track_number AS INTEGER) ASC, sort_title ASC, path ASC",
        )?;
        let iter = stmt.query_map(params![genre], |row| {
            Ok(LibrarySong {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                album_artist: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                track_number: row.get(8)?,
            })
        })?;
        let mut tracks = Vec::new();
        for item in iter {
            tracks.push(item?);
        }
        Ok(tracks)
    }

    /// Loads tracks for one normalized decade label.
    pub fn get_library_decade_songs(
        &self,
        decade: &str,
    ) -> Result<Vec<LibrarySong>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT song_id, path, title, artist, album, album_artist, genre, year, track_number
             FROM library_tracks
             WHERE CASE
                 WHEN SUBSTR(TRIM(year), 1, 3) GLOB '[0-9][0-9][0-9]'
                     THEN SUBSTR(TRIM(year), 1, 3) || '0s'
                 ELSE 'Unknown Decade'
             END = ?1
             ORDER BY year ASC, sort_artist ASC, sort_album ASC, CAST(track_number AS INTEGER) ASC, sort_title ASC, path ASC",
        )?;
        let iter = stmt.query_map(params![decade], |row| {
            Ok(LibrarySong {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                title: row.get(2)?,
                artist: row.get(3)?,
                album: row.get(4)?,
                album_artist: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                track_number: row.get(8)?,
            })
        })?;
        let mut tracks = Vec::new();
        for item in iter {
            tracks.push(item?);
        }
        Ok(tracks)
    }

    /// Returns a fresh enrichment cache entry for the supplied entity when present.
    pub fn get_library_enrichment_cache(
        &self,
        entity: &LibraryEnrichmentEntity,
        now_unix_ms: i64,
    ) -> Result<Option<LibraryEnrichmentPayload>, rusqlite::Error> {
        let (entity_type, entity_key) = Self::enrichment_entity_parts(entity);
        let result = self
            .conn
            .query_row(
                "SELECT entity_type, entity_key, status, blurb, image_path, source_name, source_url,
                        expires_unix_ms, error_kind, attempt_kind
                 FROM library_enrichment_cache
                 WHERE entity_type = ?1 AND entity_key = ?2",
                params![entity_type, entity_key],
                |row| {
                    let row_entity_type: String = row.get(0)?;
                    let row_entity_key: String = row.get(1)?;
                    let status: String = row.get(2)?;
                    let blurb: String = row.get(3)?;
                    let image_path: Option<String> = row.get(4)?;
                    let source_name: String = row.get(5)?;
                    let source_url: String = row.get(6)?;
                    let expires_unix_ms: i64 = row.get(7)?;
                    let error_kind: String = row.get(8)?;
                    let attempt_kind: String = row.get(9)?;
                    Ok((
                        row_entity_type,
                        row_entity_key,
                        status,
                        blurb,
                        image_path,
                        source_name,
                        source_url,
                        expires_unix_ms,
                        error_kind,
                        attempt_kind,
                    ))
                },
            )
            .optional()?;

        let Some((
            row_entity_type,
            row_entity_key,
            status,
            blurb,
            image_path,
            source_name,
            source_url,
            expires_unix_ms,
            error_kind,
            attempt_kind,
        )) = result
        else {
            return Ok(None);
        };

        if expires_unix_ms <= now_unix_ms {
            return Ok(None);
        }

        Ok(Some(LibraryEnrichmentPayload {
            entity: Self::enrichment_entity_from_parts(&row_entity_type, &row_entity_key),
            status: Self::enrichment_status_from_str(&status),
            blurb,
            image_path: image_path.map(PathBuf::from),
            source_name,
            source_url,
            error_kind: Self::enrichment_error_kind_from_str(&error_kind),
            attempt_kind: Self::enrichment_attempt_kind_from_str(&attempt_kind),
        }))
    }

    /// Inserts or updates one enrichment cache row.
    pub fn upsert_library_enrichment_cache(
        &self,
        payload: &LibraryEnrichmentPayload,
        image_url: Option<&str>,
        fetched_unix_ms: i64,
        expires_unix_ms: i64,
        last_error: Option<&str>,
        conclusive: bool,
    ) -> Result<(), rusqlite::Error> {
        let (entity_type, entity_key) = Self::enrichment_entity_parts(&payload.entity);
        self.conn.execute(
            "INSERT INTO library_enrichment_cache (
                entity_type, entity_key, status, blurb, image_path, image_url,
                source_name, source_url, fetched_unix_ms, expires_unix_ms, last_error,
                error_kind, attempt_kind, conclusive
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            ON CONFLICT(entity_type, entity_key) DO UPDATE SET
                status = excluded.status,
                blurb = excluded.blurb,
                image_path = excluded.image_path,
                image_url = excluded.image_url,
                source_name = excluded.source_name,
                source_url = excluded.source_url,
                fetched_unix_ms = excluded.fetched_unix_ms,
                expires_unix_ms = excluded.expires_unix_ms,
                last_error = excluded.last_error,
                error_kind = excluded.error_kind,
                attempt_kind = excluded.attempt_kind,
                conclusive = excluded.conclusive",
            params![
                entity_type,
                entity_key,
                Self::enrichment_status_to_str(payload.status),
                payload.blurb,
                payload
                    .image_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                image_url,
                payload.source_name,
                payload.source_url,
                fetched_unix_ms,
                expires_unix_ms,
                last_error.unwrap_or_default(),
                Self::enrichment_error_kind_to_str(payload.error_kind),
                Self::enrichment_attempt_kind_to_str(payload.attempt_kind),
                i64::from(conclusive),
            ],
        )?;
        Ok(())
    }

    /// Removes all expired enrichment cache rows.
    pub fn prune_expired_library_enrichment_cache(
        &self,
        now_unix_ms: i64,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "DELETE FROM library_enrichment_cache WHERE expires_unix_ms <= ?1",
            params![now_unix_ms],
        )?;
        Ok(())
    }

    /// Clears cached image path references matching one on-disk path.
    pub fn clear_library_enrichment_image_path(
        &self,
        image_path: &str,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE library_enrichment_cache SET image_path = NULL WHERE image_path = ?1",
            params![image_path],
        )?;
        Ok(())
    }

    /// Deletes all enrichment cache rows and returns number of deleted records.
    pub fn clear_library_enrichment_cache(&self) -> Result<usize, rusqlite::Error> {
        let deleted_rows = self
            .conn
            .execute("DELETE FROM library_enrichment_cache", [])?;
        Ok(deleted_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::DbManager;
    use crate::protocol::PlaylistColumnWidthOverride;

    #[test]
    fn test_set_and_get_playlist_column_order_round_trip() {
        let db = DbManager::new_in_memory().expect("in-memory db should initialize");
        let playlists = db
            .get_all_playlists()
            .expect("playlists should be queryable after init");
        let playlist = playlists
            .first()
            .expect("default playlist should exist after init");

        let saved_order = vec![
            "{artist}".to_string(),
            "{title}".to_string(),
            "custom:Album & Year|{album} ({year})".to_string(),
        ];
        db.set_playlist_column_order(&playlist.id, &saved_order)
            .expect("column order should persist");

        let loaded_order = db
            .get_playlist_column_order(&playlist.id)
            .expect("column order query should succeed");
        assert_eq!(loaded_order, Some(saved_order));
    }

    #[test]
    fn test_set_and_get_playlist_column_width_overrides_round_trip() {
        let db = DbManager::new_in_memory().expect("in-memory db should initialize");
        let playlists = db
            .get_all_playlists()
            .expect("playlists should be queryable after init");
        let playlist = playlists
            .first()
            .expect("default playlist should exist after init");

        let saved = vec![
            PlaylistColumnWidthOverride {
                column_key: "{title}".to_string(),
                width_px: 210,
            },
            PlaylistColumnWidthOverride {
                column_key: "{artist}".to_string(),
                width_px: 180,
            },
        ];
        db.set_playlist_column_width_overrides(&playlist.id, &saved)
            .expect("column width overrides should persist");

        let loaded = db
            .get_playlist_column_width_overrides(&playlist.id)
            .expect("column width overrides query should succeed");
        assert_eq!(loaded, Some(saved));
    }

    #[test]
    fn test_upsert_and_clear_playlist_column_width_override() {
        let db = DbManager::new_in_memory().expect("in-memory db should initialize");
        let playlists = db
            .get_all_playlists()
            .expect("playlists should be queryable after init");
        let playlist = playlists
            .first()
            .expect("default playlist should exist after init");

        db.set_playlist_column_width_override(&playlist.id, "{title}", 190)
            .expect("title width override should be saved");
        db.set_playlist_column_width_override(&playlist.id, "{artist}", 170)
            .expect("artist width override should be saved");
        db.set_playlist_column_width_override(&playlist.id, "{title}", 200)
            .expect("title width override should update");

        let loaded = db
            .get_playlist_column_width_overrides(&playlist.id)
            .expect("column width overrides query should succeed")
            .expect("overrides should exist");
        assert_eq!(loaded.len(), 2);
        assert!(loaded
            .iter()
            .any(|item| item.column_key == "{title}" && item.width_px == 200));

        db.clear_playlist_column_width_override(&playlist.id, "{artist}")
            .expect("artist override should be removed");
        let loaded_after_clear = db
            .get_playlist_column_width_overrides(&playlist.id)
            .expect("column width overrides query should succeed")
            .expect("title override should remain");
        assert_eq!(loaded_after_clear.len(), 1);
        assert_eq!(loaded_after_clear[0].column_key, "{title}");
    }
}
