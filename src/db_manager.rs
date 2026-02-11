//! SQLite-backed persistence for playlists, library index data, and playlist-scoped UI metadata.

use crate::protocol::{
    LibraryAlbum, LibraryArtist, LibraryDecade, LibraryGenre, LibrarySong,
    PlaylistColumnWidthOverride, PlaylistInfo, RestoredTrack, TrackMetadataSummary,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::{collections::HashSet, path::PathBuf};
use uuid::Uuid;

/// Database gateway for playlist and track persistence.
pub struct DbManager {
    conn: Connection,
}

impl DbManager {
    fn normalize_library_sort_key(value: &str, fallback: &str) -> String {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return fallback.to_string();
        }
        trimmed.to_ascii_lowercase()
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

        let db_manager = Self { conn };
        db_manager.initialize_schema()?;
        db_manager.migrate()?;
        Ok(db_manager)
    }

    #[cfg(test)]
    /// Creates an in-memory database instance for tests.
    pub fn new_in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
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
                modified_unix_ms INTEGER NOT NULL DEFAULT 0
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
        for col in library_columns {
            if col? == "genre" {
                has_genre = true;
                break;
            }
        }
        if !has_genre {
            self.conn.execute(
                "ALTER TABLE library_tracks ADD COLUMN genre TEXT NOT NULL DEFAULT ''",
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
        let mut stmt = self
            .conn
            .prepare("UPDATE tracks SET position = ?1 WHERE id = ?2")?;
        for (i, id) in ids.iter().enumerate() {
            stmt.execute(params![i as i64, id])?;
        }
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

    /// Inserts or updates one indexed library track entry.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_library_track(
        &self,
        song_id: &str,
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        album_artist: &str,
        genre: &str,
        year: &str,
        track_number: &str,
        sort_title: &str,
        sort_artist: &str,
        sort_album: &str,
        modified_unix_ms: i64,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO library_tracks (
                song_id, path, title, artist, album, album_artist, genre, year, track_number,
                sort_title, sort_artist, sort_album, modified_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
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
                modified_unix_ms = excluded.modified_unix_ms",
            params![
                song_id,
                path,
                title,
                artist,
                album,
                album_artist,
                genre,
                year,
                track_number,
                sort_title,
                sort_artist,
                sort_album,
                modified_unix_ms,
            ],
        )?;
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
                 modified_unix_ms = ?11
             WHERE path = ?12",
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
                path
            ],
        )?;
        Ok(updated > 0)
    }

    /// Returns all indexed library paths.
    pub fn get_all_library_paths(&self) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self.conn.prepare("SELECT path FROM library_tracks")?;
        let iter = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut paths = Vec::new();
        for item in iter {
            paths.push(item?);
        }
        Ok(paths)
    }

    /// Deletes indexed library rows that are no longer present in scanned paths.
    pub fn delete_library_paths_not_in_set(
        &self,
        keep_paths: &HashSet<String>,
    ) -> Result<(), rusqlite::Error> {
        let existing_paths = self.get_all_library_paths()?;
        for path in existing_paths {
            if !keep_paths.contains(&path) {
                self.conn
                    .execute("DELETE FROM library_tracks WHERE path = ?1", params![path])?;
            }
        }
        Ok(())
    }

    /// Loads all songs in library sorted alphabetically by title.
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
        let mut songs = Vec::new();
        for item in iter {
            songs.push(item?);
        }
        Ok(songs)
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

    /// Loads songs for one album+album-artist pair sorted by track number then title.
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
        let mut songs = Vec::new();
        for item in iter {
            songs.push(item?);
        }
        Ok(songs)
    }

    /// Loads artist detail (albums and songs) for one artist.
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
        let mut songs = Vec::new();
        for item in song_iter {
            songs.push(item?);
        }
        Ok((albums, songs))
    }

    /// Loads songs for one normalized genre label.
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
        let mut songs = Vec::new();
        for item in iter {
            songs.push(item?);
        }
        Ok(songs)
    }

    /// Loads songs for one normalized decade label.
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
        let mut songs = Vec::new();
        for item in iter {
            songs.push(item?);
        }
        Ok(songs)
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
