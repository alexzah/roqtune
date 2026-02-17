//! Track-source URI helpers for remote backend items.

use std::path::Path;

/// Decoded OpenSubsonic track locator encoded in a synthetic track path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSubsonicTrackLocator {
    pub profile_id: String,
    pub song_id: String,
    pub endpoint: String,
    pub username: String,
    pub format_hint: Option<String>,
}

fn strip_trailing_slash(endpoint: &str) -> String {
    endpoint.trim().trim_end_matches('/').to_string()
}

fn strip_opensubsonic_prefix(raw: &str) -> Option<&str> {
    const PREFIXES: [&str; 2] = ["rtq://open_subsonic/", "rtq://opensubsonic/"];
    for prefix in PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    let lower = raw.to_ascii_lowercase();
    for prefix in PREFIXES {
        if lower.starts_with(prefix) {
            return Some(&raw[prefix.len()..]);
        }
    }
    None
}

/// Encodes an OpenSubsonic track locator as a synthetic path URI.
pub fn encode_opensubsonic_track_uri(
    profile_id: &str,
    song_id: &str,
    endpoint: &str,
    username: &str,
    format_hint: Option<&str>,
) -> String {
    let mut query = format!(
        "endpoint={}&username={}",
        urlencoding::encode(&strip_trailing_slash(endpoint)),
        urlencoding::encode(username.trim())
    );
    if let Some(format_hint) = format_hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        query.push_str("&format=");
        query.push_str(urlencoding::encode(format_hint).as_ref());
    }
    format!(
        "rtq://open_subsonic/{}/{}?{}",
        urlencoding::encode(profile_id),
        urlencoding::encode(song_id),
        query
    )
}

/// Returns true if the provided path encodes a synthetic remote track URI.
pub fn is_remote_track_path(path: &Path) -> bool {
    path.to_str().and_then(strip_opensubsonic_prefix).is_some()
}

/// Parses a synthetic OpenSubsonic track URI from a path.
pub fn parse_opensubsonic_track_uri(path: &Path) -> Option<OpenSubsonicTrackLocator> {
    let raw = path.to_str()?;
    let rest = strip_opensubsonic_prefix(raw)?;
    let (path_part, query_part) = rest.split_once('?').unwrap_or((rest, ""));
    let mut parts = path_part.splitn(2, '/');
    let profile_id = urlencoding::decode(parts.next()?).ok()?.to_string();
    let song_id = urlencoding::decode(parts.next()?).ok()?.to_string();
    if profile_id.trim().is_empty() || song_id.trim().is_empty() {
        return None;
    }

    let mut endpoint = String::new();
    let mut username = String::new();
    let mut format_hint = None;
    for key_value in query_part.split('&') {
        if key_value.trim().is_empty() {
            continue;
        }
        let (raw_key, raw_value) = key_value.split_once('=').unwrap_or((key_value, ""));
        let key = raw_key.trim().to_ascii_lowercase();
        let decoded = urlencoding::decode(raw_value)
            .map(|value| value.to_string())
            .unwrap_or_else(|_| raw_value.to_string());
        match key.as_str() {
            "endpoint" | "url" | "server" => endpoint = strip_trailing_slash(&decoded),
            "username" | "user" | "u" => username = decoded.trim().to_string(),
            "format" | "suffix" => {
                let normalized = decoded.trim().to_ascii_lowercase();
                if !normalized.is_empty() {
                    format_hint = Some(normalized);
                }
            }
            _ => {}
        }
    }

    Some(OpenSubsonicTrackLocator {
        profile_id,
        song_id,
        endpoint,
        username,
        format_hint,
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
            Some("flac"),
        );
        let decoded = parse_opensubsonic_track_uri(PathBuf::from(uri).as_path())
            .expect("encoded uri should decode");
        assert_eq!(decoded.profile_id, "home-profile");
        assert_eq!(decoded.song_id, "track-001");
        assert_eq!(decoded.endpoint, "https://music.example.com");
        assert_eq!(decoded.username, "alice");
        assert_eq!(decoded.format_hint.as_deref(), Some("flac"));
    }

    #[test]
    fn test_remote_track_path_detection() {
        let uri = encode_opensubsonic_track_uri(
            "home-profile",
            "track-001",
            "https://music.example.com",
            "alice",
            None,
        );
        assert!(is_remote_track_path(PathBuf::from(uri).as_path()));
        assert!(!is_remote_track_path(
            PathBuf::from("/music/local.flac").as_path()
        ));
    }

    #[test]
    fn test_decode_accepts_legacy_uppercase_query_keys() {
        let uri = "rtq://open_subsonic/home-profile/track-001?ENDPOINT=https%3A%2F%2Fmusic.example.com&USERNAME=alice&FORMAT=FLAC";
        let decoded = parse_opensubsonic_track_uri(PathBuf::from(uri).as_path())
            .expect("legacy uri should decode");
        assert_eq!(decoded.profile_id, "home-profile");
        assert_eq!(decoded.song_id, "track-001");
        assert_eq!(decoded.endpoint, "https://music.example.com");
        assert_eq!(decoded.username, "alice");
        assert_eq!(decoded.format_hint.as_deref(), Some("flac"));
    }

    #[test]
    fn test_decode_accepts_opensubsonic_host_variant() {
        let uri = "rtq://opensubsonic/home-profile/track-001?endpoint=https%3A%2F%2Fmusic.example.com&username=alice";
        let decoded = parse_opensubsonic_track_uri(PathBuf::from(uri).as_path())
            .expect("opensubsonic host variant should decode");
        assert_eq!(decoded.profile_id, "home-profile");
        assert_eq!(decoded.song_id, "track-001");
        assert_eq!(decoded.endpoint, "https://music.example.com");
        assert_eq!(decoded.username, "alice");
    }
}
