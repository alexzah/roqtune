//! Track-source URI helpers for remote backend items.

use std::path::Path;

/// Decoded OpenSubsonic track locator encoded in a synthetic track path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSubsonicTrackLocator {
    pub profile_id: String,
    pub song_id: String,
    pub endpoint: String,
    pub username: String,
}

fn strip_trailing_slash(endpoint: &str) -> String {
    endpoint.trim().trim_end_matches('/').to_string()
}

/// Encodes an OpenSubsonic track locator as a synthetic path URI.
pub fn encode_opensubsonic_track_uri(
    profile_id: &str,
    song_id: &str,
    endpoint: &str,
    username: &str,
) -> String {
    format!(
        "rtq://open_subsonic/{}/{}?endpoint={}&username={}",
        urlencoding::encode(profile_id),
        urlencoding::encode(song_id),
        urlencoding::encode(&strip_trailing_slash(endpoint)),
        urlencoding::encode(username.trim())
    )
}

/// Returns true if the provided path encodes a synthetic remote track URI.
pub fn is_remote_track_path(path: &Path) -> bool {
    parse_opensubsonic_track_uri(path).is_some()
}

/// Parses a synthetic OpenSubsonic track URI from a path.
pub fn parse_opensubsonic_track_uri(path: &Path) -> Option<OpenSubsonicTrackLocator> {
    let raw = path.to_str()?;
    let rest = raw.strip_prefix("rtq://open_subsonic/")?;
    let (path_part, query_part) = rest.split_once('?')?;
    let mut parts = path_part.splitn(2, '/');
    let profile_id = urlencoding::decode(parts.next()?).ok()?.to_string();
    let song_id = urlencoding::decode(parts.next()?).ok()?.to_string();
    if profile_id.trim().is_empty() || song_id.trim().is_empty() {
        return None;
    }

    let mut endpoint = String::new();
    let mut username = String::new();
    for key_value in query_part.split('&') {
        let (key, value) = key_value.split_once('=')?;
        let decoded = urlencoding::decode(value).ok()?.to_string();
        match key {
            "endpoint" => endpoint = strip_trailing_slash(&decoded),
            "username" => username = decoded.trim().to_string(),
            _ => {}
        }
    }
    if endpoint.is_empty() || username.is_empty() {
        return None;
    }

    Some(OpenSubsonicTrackLocator {
        profile_id,
        song_id,
        endpoint,
        username,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        encode_opensubsonic_track_uri, is_remote_track_path, parse_opensubsonic_track_uri,
    };
    use std::path::PathBuf;

    #[test]
    fn test_encode_decode_round_trip() {
        let uri = encode_opensubsonic_track_uri(
            "home-profile",
            "track-001",
            "https://music.example.com/",
            "alice",
        );
        let decoded = parse_opensubsonic_track_uri(PathBuf::from(uri).as_path())
            .expect("encoded uri should decode");
        assert_eq!(decoded.profile_id, "home-profile");
        assert_eq!(decoded.song_id, "track-001");
        assert_eq!(decoded.endpoint, "https://music.example.com");
        assert_eq!(decoded.username, "alice");
    }

    #[test]
    fn test_remote_track_path_detection() {
        let uri = encode_opensubsonic_track_uri(
            "home-profile",
            "track-001",
            "https://music.example.com",
            "alice",
        );
        assert!(is_remote_track_path(PathBuf::from(uri).as_path()));
        assert!(!is_remote_track_path(
            PathBuf::from("/music/local.flac").as_path()
        ));
    }
}
