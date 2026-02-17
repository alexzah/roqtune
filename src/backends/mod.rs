//! Backend adapter abstractions and concrete implementations.

pub mod opensubsonic;

/// Remote track payload returned by backend adapters.
#[derive(Debug, Clone)]
pub struct BackendTrack {
    pub item_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: String,
    pub track_number: String,
    pub format_hint: Option<String>,
}

/// Remote playlist payload returned by backend adapters.
#[derive(Debug, Clone)]
pub struct BackendPlaylist {
    pub remote_playlist_id: String,
    pub name: String,
    pub tracks: Vec<BackendTrack>,
}

/// Connection and sync profile used by backend adapters.
#[derive(Debug, Clone)]
pub struct BackendProfileAuth {
    pub profile_id: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
}

/// Interface implemented by concrete media backend adapters.
pub trait MediaBackendAdapter: Send + Sync {
    fn test_connection(&self, profile: &BackendProfileAuth) -> Result<(), String>;
    fn fetch_library_tracks(
        &self,
        profile: &BackendProfileAuth,
    ) -> Result<Vec<BackendTrack>, String>;
    fn fetch_playlists(&self, profile: &BackendProfileAuth)
        -> Result<Vec<BackendPlaylist>, String>;
    fn create_playlist(
        &self,
        profile: &BackendProfileAuth,
        name: &str,
        song_ids: &[String],
    ) -> Result<String, String>;
    fn replace_playlist_tracks(
        &self,
        profile: &BackendProfileAuth,
        remote_playlist_id: &str,
        song_ids: &[String],
    ) -> Result<(), String>;
}
