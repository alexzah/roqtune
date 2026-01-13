use crate::protocol::{DetailedMetadata, PlaylistInfo, RestoredTrack};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use uuid::Uuid;

pub struct DbManager {
    conn: Connection,
}

impl DbManager {
    pub fn new() -> Result<Self, rusqlite::Error> {
        let data_dir = dirs::data_dir()
            .expect("Could not find data directory")
            .join("music_player");

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

    fn initialize_schema(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS playlists (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL
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

    pub fn create_playlist(&self, id: &str, name: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO playlists (id, name) VALUES (?1, ?2)",
            params![id, name],
        )?;
        Ok(())
    }

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

    pub fn update_metadata(
        &self,
        id: &str,
        metadata: &DetailedMetadata,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE tracks SET title = ?1, artist = ?2, album = ?3, date = ?4, genre = ?5 WHERE id = ?6",
            params![
                metadata.title,
                metadata.artist,
                metadata.album,
                metadata.date,
                metadata.genre,
                id
            ],
        )?;
        Ok(())
    }

    pub fn delete_track(&self, id: &str) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get_tracks_for_playlist(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<RestoredTrack>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, artist, album, date, genre FROM tracks WHERE playlist_id = ?1 ORDER BY position ASC",
        )?;
        let track_iter = stmt.query_map(params![playlist_id], |row| {
            Ok(RestoredTrack {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                metadata: DetailedMetadata {
                    title: row.get(2).unwrap_or_default(),
                    artist: row.get(3).unwrap_or_default(),
                    album: row.get(4).unwrap_or_default(),
                    date: row.get(5).unwrap_or_default(),
                    genre: row.get(6).unwrap_or_default(),
                },
            })
        })?;

        let mut tracks = Vec::new();
        for track in track_iter {
            tracks.push(track?);
        }
        Ok(tracks)
    }

    pub fn get_track_metadata(&self, id: &str) -> Result<DetailedMetadata, rusqlite::Error> {
        self.conn.query_row(
            "SELECT title, artist, album, date, genre FROM tracks WHERE id = ?1",
            params![id],
            |row| {
                Ok(DetailedMetadata {
                    title: row.get(0).unwrap_or_default(),
                    artist: row.get(1).unwrap_or_default(),
                    album: row.get(2).unwrap_or_default(),
                    date: row.get(3).unwrap_or_default(),
                    genre: row.get(4).unwrap_or_default(),
                })
            },
        )
    }

    pub fn update_positions(&self, ids: Vec<String>) -> Result<(), rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("UPDATE tracks SET position = ?1 WHERE id = ?2")?;
        for (i, id) in ids.iter().enumerate() {
            stmt.execute(params![i as i64, id])?;
        }
        Ok(())
    }
}
