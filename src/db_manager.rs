use crate::protocol::{DetailedMetadata, RestoredTrack};
use rusqlite::{params, Connection};
use std::path::PathBuf;

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
        Ok(db_manager)
    }

    fn initialize_schema(&self) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS tracks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                position INTEGER NOT NULL,
                title TEXT,
                artist TEXT,
                album TEXT,
                date TEXT,
                genre TEXT
            )",
            [],
        )?;
        Ok(())
    }

    pub fn save_track(&self, id: &str, path: &str, position: usize) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO tracks (id, path, position) VALUES (?1, ?2, ?3)",
            params![id, path, position as i64],
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

    pub fn get_all_tracks(&self) -> Result<Vec<RestoredTrack>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, title, artist, album, date, genre FROM tracks ORDER BY position ASC",
        )?;
        let track_iter = stmt.query_map([], |row| {
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
