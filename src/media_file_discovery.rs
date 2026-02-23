use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use log::debug;

pub const SUPPORTED_AUDIO_EXTENSIONS: [&str; 7] =
    ["mp3", "wav", "ogg", "flac", "aac", "m4a", "mp4"];

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
