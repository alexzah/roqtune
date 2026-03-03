//! OpenSubsonic backend adapter implementation.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

use crate::backends::{BackendPlaylist, BackendProfileAuth, BackendTrack, MediaBackendAdapter};

const API_VERSION: &str = "1.16.1";
const CLIENT_ID: &str = "roqtune";

/// OpenSubsonic adapter backed by `ureq`.
pub struct OpenSubsonicAdapter {
    http_client: ureq::Agent,
}

impl OpenSubsonicAdapter {
    /// Creates a new OpenSubsonic adapter.
    pub fn new() -> Self {
        let http_client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(15))
            .timeout_write(Duration::from_secs(15))
            .build();
        Self { http_client }
    }

    fn make_salt() -> String {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        bytes.iter().map(|value| format!("{value:02x}")).collect()
    }

    fn auth_params(profile: &BackendProfileAuth) -> Vec<(String, String)> {
        let salt = Self::make_salt();
        let token = format!(
            "{:x}",
            md5::compute(format!("{}{}", profile.password, salt))
        );
        vec![
            ("u".to_string(), profile.username.clone()),
            ("t".to_string(), token),
            ("s".to_string(), salt),
            ("f".to_string(), "json".to_string()),
            ("v".to_string(), API_VERSION.to_string()),
            ("c".to_string(), CLIENT_ID.to_string()),
        ]
    }

    fn endpoint_base(endpoint: &str) -> String {
        endpoint.trim().trim_end_matches('/').to_string()
    }

    fn api_url(profile: &BackendProfileAuth, method: &str, params: &[(String, String)]) -> String {
        let mut query_parts: Vec<String> = Self::auth_params(profile)
            .into_iter()
            .map(|(key, value)| format!("{key}={}", urlencoding::encode(&value)))
            .collect();
        query_parts.extend(
            params
                .iter()
                .map(|(key, value)| format!("{key}={}", urlencoding::encode(value))),
        );
        format!(
            "{}/rest/{}.view?{}",
            Self::endpoint_base(&profile.endpoint),
            method,
            query_parts.join("&")
        )
    }

    fn request_json(
        &self,
        profile: &BackendProfileAuth,
        method: &str,
        params: &[(String, String)],
    ) -> Result<Value, String> {
        let url = Self::api_url(profile, method, params);
        let response = self
            .http_client
            .get(&url)
            .call()
            .map_err(|err| format!("OpenSubsonic request failed ({method}): {err}"))?;
        let parsed: Value = response
            .into_json()
            .map_err(|err| format!("OpenSubsonic response parse failed ({method}): {err}"))?;
        let status = parsed
            .get("subsonic-response")
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if status != "ok" {
            let error_message = parsed
                .get("subsonic-response")
                .and_then(|value| value.get("error"))
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("OpenSubsonic returned an error");
            return Err(error_message.to_string());
        }
        Ok(parsed)
    }

    fn array_or_single(value: Option<&Value>) -> Vec<&Value> {
        match value {
            Some(Value::Array(items)) => items.iter().collect(),
            Some(item @ Value::Object(_)) => vec![item],
            _ => Vec::new(),
        }
    }

    fn parse_track(song: &Value) -> Option<BackendTrack> {
        let item_id = song.get("id")?.as_str()?.to_string();
        let title = song
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Unknown Title")
            .to_string();
        let artist = song
            .get("artist")
            .and_then(Value::as_str)
            .unwrap_or("Unknown Artist")
            .to_string();
        let album = song
            .get("album")
            .and_then(Value::as_str)
            .unwrap_or("Unknown Album")
            .to_string();
        let genre = song
            .get("genre")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let year = song
            .get("year")
            .map(|value| {
                value
                    .as_i64()
                    .map(|number| number.to_string())
                    .or_else(|| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let track_number = song
            .get("track")
            .map(|value| {
                value
                    .as_i64()
                    .map(|number| number.to_string())
                    .or_else(|| value.as_str().map(ToOwned::to_owned))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let format_hint = song
            .get("suffix")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_ascii_lowercase());
        Some(BackendTrack {
            item_id,
            title,
            artist,
            album,
            genre,
            year,
            track_number,
            format_hint,
        })
    }

    fn fetch_albums_page(
        &self,
        profile: &BackendProfileAuth,
        offset: usize,
        page_size: usize,
    ) -> Result<Vec<String>, String> {
        let payload = self.request_json(
            profile,
            "getAlbumList2",
            &[
                ("type".to_string(), "alphabeticalByName".to_string()),
                ("size".to_string(), page_size.to_string()),
                ("offset".to_string(), offset.to_string()),
            ],
        )?;
        let albums = Self::array_or_single(
            payload
                .get("subsonic-response")
                .and_then(|value| value.get("albumList2"))
                .and_then(|value| value.get("album")),
        );
        Ok(albums
            .into_iter()
            .filter_map(|album| {
                album
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect())
    }

    fn fetch_album_tracks(
        &self,
        profile: &BackendProfileAuth,
        album_id: &str,
    ) -> Result<Vec<BackendTrack>, String> {
        let payload = self.request_json(
            profile,
            "getAlbum",
            &[("id".to_string(), album_id.to_string())],
        )?;
        let songs = Self::array_or_single(
            payload
                .get("subsonic-response")
                .and_then(|value| value.get("album"))
                .and_then(|value| value.get("song")),
        );
        Ok(songs.into_iter().filter_map(Self::parse_track).collect())
    }

    fn fetch_playlist_tracks(
        &self,
        profile: &BackendProfileAuth,
        playlist_id: &str,
    ) -> Result<Vec<BackendTrack>, String> {
        let payload = self.request_json(
            profile,
            "getPlaylist",
            &[("id".to_string(), playlist_id.to_string())],
        )?;
        let entries = Self::array_or_single(
            payload
                .get("subsonic-response")
                .and_then(|value| value.get("playlist"))
                .and_then(|value| value.get("entry")),
        );
        Ok(entries.into_iter().filter_map(Self::parse_track).collect())
    }

    fn fetch_starred_tracks(
        &self,
        profile: &BackendProfileAuth,
    ) -> Result<Vec<BackendTrack>, String> {
        let payload = self.request_json(profile, "getStarred2", &[])?;
        let songs = Self::array_or_single(
            payload
                .get("subsonic-response")
                .and_then(|value| value.get("starred2"))
                .and_then(|value| value.get("song")),
        );
        let mut seen_song_ids = HashSet::new();
        let mut tracks = Vec::new();
        for song in songs {
            if let Some(track) = Self::parse_track(song) {
                if seen_song_ids.insert(track.item_id.clone()) {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }
}

impl Default for OpenSubsonicAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaBackendAdapter for OpenSubsonicAdapter {
    fn test_connection(&self, profile: &BackendProfileAuth) -> Result<(), String> {
        let _ = self.request_json(profile, "ping", &[])?;
        Ok(())
    }

    fn fetch_library_tracks(
        &self,
        profile: &BackendProfileAuth,
    ) -> Result<Vec<BackendTrack>, String> {
        const PAGE_SIZE: usize = 300;
        let mut offset = 0usize;
        let mut album_ids: Vec<String> = Vec::new();
        loop {
            let next_page = self.fetch_albums_page(profile, offset, PAGE_SIZE)?;
            if next_page.is_empty() {
                break;
            }
            offset = offset.saturating_add(next_page.len());
            let reached_end = next_page.len() < PAGE_SIZE;
            album_ids.extend(next_page);
            if reached_end {
                break;
            }
        }

        let mut seen_song_ids = HashSet::new();
        let mut tracks = Vec::new();
        for album_id in album_ids {
            let album_tracks = self.fetch_album_tracks(profile, &album_id)?;
            for track in album_tracks {
                if seen_song_ids.insert(track.item_id.clone()) {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }

    fn fetch_favorite_tracks(
        &self,
        profile: &BackendProfileAuth,
    ) -> Result<Vec<BackendTrack>, String> {
        self.fetch_starred_tracks(profile)
    }

    fn fetch_playlists(
        &self,
        profile: &BackendProfileAuth,
    ) -> Result<Vec<BackendPlaylist>, String> {
        let payload = self.request_json(profile, "getPlaylists", &[])?;
        let playlists = Self::array_or_single(
            payload
                .get("subsonic-response")
                .and_then(|value| value.get("playlists"))
                .and_then(|value| value.get("playlist")),
        );

        let mut result = Vec::new();
        for playlist in playlists {
            let Some(remote_playlist_id) = playlist
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
            else {
                continue;
            };
            let name = playlist
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Remote Playlist")
                .to_string();
            let tracks = self.fetch_playlist_tracks(profile, &remote_playlist_id)?;
            result.push(BackendPlaylist {
                remote_playlist_id,
                name,
                tracks,
            });
        }
        Ok(result)
    }

    fn create_playlist(
        &self,
        profile: &BackendProfileAuth,
        name: &str,
        song_ids: &[String],
    ) -> Result<String, String> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err("playlist name cannot be empty".to_string());
        }

        let mut params = vec![("name".to_string(), trimmed_name.to_string())];
        for song_id in song_ids {
            params.push(("songId".to_string(), song_id.clone()));
        }

        let payload = self.request_json(profile, "createPlaylist", &params)?;
        payload
            .get("subsonic-response")
            .and_then(|value| value.get("playlist"))
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| "OpenSubsonic createPlaylist response missing playlist id".to_string())
    }

    fn set_track_favorite(
        &self,
        profile: &BackendProfileAuth,
        song_id: &str,
        favorited: bool,
    ) -> Result<(), String> {
        let trimmed_song_id = song_id.trim();
        if trimmed_song_id.is_empty() {
            return Err("song id cannot be empty".to_string());
        }
        let method = if favorited { "star" } else { "unstar" };
        let _ = self.request_json(
            profile,
            method,
            &[("id".to_string(), trimmed_song_id.to_string())],
        )?;
        Ok(())
    }

    fn replace_playlist_tracks(
        &self,
        profile: &BackendProfileAuth,
        remote_playlist_id: &str,
        song_ids: &[String],
    ) -> Result<(), String> {
        let existing = self.fetch_playlist_tracks(profile, remote_playlist_id)?;
        let mut params = Vec::new();
        params.push(("playlistId".to_string(), remote_playlist_id.to_string()));
        for index in 0..existing.len() {
            params.push(("songIndexToRemove".to_string(), index.to_string()));
        }
        for song_id in song_ids {
            params.push(("songIdToAdd".to_string(), song_id.clone()));
        }
        let _ = self.request_json(profile, "updatePlaylist", &params)?;
        Ok(())
    }
}
