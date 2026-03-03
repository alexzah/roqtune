//! Filesystem discovery helpers for importable audio files and library folders.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use log::debug;

/// File extensions treated as importable audio tracks.
pub const SUPPORTED_AUDIO_EXTENSIONS: [&str; 7] =
    ["mp3", "wav", "ogg", "flac", "aac", "m4a", "mp4"];

/// Returns `true` when `path` has a supported audio extension.
pub fn is_supported_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            SUPPORTED_AUDIO_EXTENSIONS
                .iter()
                .any(|supported| ext.eq_ignore_ascii_case(supported))
        })
        .unwrap_or(false)
}

/// Recursively scans a folder and returns supported audio files in sorted order.
pub fn collect_audio_files_from_folder(folder_path: &Path) -> Vec<PathBuf> {
    let mut pending_directories = vec![folder_path.to_path_buf()];
    let mut tracks = Vec::new();

    while let Some(directory) = pending_directories.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(err) => {
                debug!("Failed to read directory {}: {}", directory.display(), err);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    debug!(
                        "Failed to read a directory entry in {}: {}",
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
                    debug!("Failed to inspect {}: {}", path.display(), err);
                    continue;
                }
            };

            if file_type.is_dir() {
                pending_directories.push(path);
                continue;
            }

            if file_type.is_file() && is_supported_audio_file(&path) {
                tracks.push(path);
            }
        }
    }

    tracks.sort_unstable();
    tracks
}

/// Collects supported audio files from mixed file/folder drag-and-drop paths.
pub fn collect_audio_files_from_dropped_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut tracks = BTreeSet::new();
    for path in paths {
        if path.is_file() {
            if is_supported_audio_file(path) {
                tracks.insert(path.clone());
            }
            continue;
        }
        if path.is_dir() {
            for track in collect_audio_files_from_folder(path) {
                tracks.insert(track);
            }
        }
    }
    tracks.into_iter().collect()
}

/// Collects library root folders from dropped paths, deduplicated and sorted.
pub fn collect_library_folders_from_dropped_paths(paths: &[PathBuf]) -> Vec<String> {
    let mut folders = BTreeSet::new();
    for path in paths {
        if path.is_dir() {
            folders.insert(path.to_string_lossy().to_string());
            continue;
        }
        if path.is_file() && is_supported_audio_file(path) {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    folders.insert(parent.to_string_lossy().to_string());
                }
            }
        }
    }
    folders.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{collect_audio_files_from_folder, is_supported_audio_file};

    fn unique_temp_directory(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "roqtune_{}_{}_{}",
            test_name,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn test_is_supported_audio_file_checks_known_extensions_case_insensitively() {
        assert!(is_supported_audio_file(Path::new("/tmp/track.mp3")));
        assert!(is_supported_audio_file(Path::new("/tmp/track.FLAC")));
        assert!(is_supported_audio_file(Path::new("/tmp/track.m4a")));
        assert!(!is_supported_audio_file(Path::new("/tmp/track.txt")));
        assert!(!is_supported_audio_file(Path::new("/tmp/track")));
    }

    #[test]
    fn test_collect_audio_files_from_folder_recurses_and_filters_non_audio_files() {
        let base = unique_temp_directory("import_scan");
        let nested = base.join("nested");
        let deep = nested.join("deep");
        std::fs::create_dir_all(&deep).expect("test directories should be created");
        std::fs::write(base.join("track_b.flac"), b"").expect("should write flac fixture");
        std::fs::write(base.join("ignore.txt"), b"").expect("should write text fixture");
        std::fs::write(nested.join("track_a.MP3"), b"").expect("should write mp3 fixture");
        std::fs::write(deep.join("track_c.wav"), b"").expect("should write wav fixture");

        let imported = collect_audio_files_from_folder(&base);
        let mut expected = vec![
            nested.join("track_a.MP3"),
            base.join("track_b.flac"),
            deep.join("track_c.wav"),
        ];
        expected.sort_unstable();
        assert_eq!(imported, expected);

        let _ = std::fs::remove_dir_all(&base);
    }
}
