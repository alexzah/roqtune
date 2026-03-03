//! Shared helpers for estimating technical metadata from audio files.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// File-level properties derived from library/tag parsing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LibraryAudioProperties {
    pub duration_ms: u64,
    pub audio_bitrate_kbps: u32,
    pub overall_bitrate_kbps: u32,
    pub metadata_size_bytes: u64,
}

/// Read duration/bitrate metadata from Lofty, preferring container/library properties.
pub(crate) fn read_library_audio_properties(path: &Path) -> Option<LibraryAudioProperties> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::TagExt;

    let tagged = lofty::read_from_path(path).ok()?;
    let properties = tagged.properties();
    let metadata_size_bytes = tagged.tags().iter().map(|tag| tag.len() as u64).sum();
    Some(LibraryAudioProperties {
        duration_ms: properties.duration().as_millis() as u64,
        audio_bitrate_kbps: properties.audio_bitrate().unwrap_or(0),
        overall_bitrate_kbps: properties.overall_bitrate().unwrap_or(0),
        metadata_size_bytes,
    })
}

/// Estimate average bitrate in kbps.
///
/// Order:
/// 1. Library/tag-derived audio bitrate.
/// 2. Library/tag-derived overall bitrate.
/// 3. Implied bitrate from audio payload size (file size minus metadata/tag blocks).
/// 4. Implied bitrate from full file size as a last-resort fallback.
pub(crate) fn estimate_bitrate_kbps(
    path: &Path,
    duration_ms: u64,
    library: Option<&LibraryAudioProperties>,
) -> u32 {
    if let Some(properties) = library {
        if properties.audio_bitrate_kbps > 0 {
            return properties.audio_bitrate_kbps;
        }
        if properties.overall_bitrate_kbps > 0 {
            return properties.overall_bitrate_kbps;
        }
    }

    if let Some(audio_size_bytes) = audio_payload_size_bytes(path, library) {
        if let Some(kbps) = implied_bitrate_kbps(audio_size_bytes, duration_ms) {
            return kbps;
        }
    }

    std::fs::metadata(path)
        .ok()
        .and_then(|meta| implied_bitrate_kbps(meta.len(), duration_ms))
        .unwrap_or(0)
}

fn implied_bitrate_kbps(size_bytes: u64, duration_ms: u64) -> Option<u32> {
    if size_bytes == 0 || duration_ms == 0 {
        return None;
    }
    let duration_seconds = duration_ms as f64 / 1000.0;
    if !duration_seconds.is_finite() || duration_seconds <= f64::EPSILON {
        return None;
    }
    let bits_per_second = (size_bytes as f64 * 8.0) / duration_seconds;
    if !bits_per_second.is_finite() || bits_per_second <= 0.0 {
        return None;
    }
    let kbps = (bits_per_second / 1000.0).round();
    if !kbps.is_finite() || kbps <= 0.0 {
        None
    } else {
        Some(kbps as u32)
    }
}

fn audio_payload_size_bytes(path: &Path, library: Option<&LibraryAudioProperties>) -> Option<u64> {
    let file_size = std::fs::metadata(path).ok()?.len();
    let metadata_size = library
        .map(|properties| properties.metadata_size_bytes)
        .unwrap_or_else(|| legacy_metadata_size_bytes(path).unwrap_or(0));
    let audio_data_size = file_size.saturating_sub(metadata_size);
    (audio_data_size > 0).then_some(audio_data_size)
}

fn legacy_metadata_size_bytes(path: &Path) -> Option<u64> {
    let mut total_size = 0u64;
    let mut file = File::open(path).ok()?;
    let file_size = file.metadata().ok()?.len();
    if file_size == 0 {
        return Some(0);
    }

    let mut header = [0u8; 10];
    if file.read_exact(&mut header).is_ok() && &header[0..3] == b"ID3" {
        let size = ((header[6] as u32 & 0x7F) << 21)
            | ((header[7] as u32 & 0x7F) << 14)
            | ((header[8] as u32 & 0x7F) << 7)
            | (header[9] as u32 & 0x7F);
        total_size = total_size.saturating_add((size + 10) as u64);
    }

    if file_size > 128 {
        let _ = file.seek(SeekFrom::End(-128));
        let mut id3v1 = [0u8; 3];
        if file.read_exact(&mut id3v1).is_ok() && &id3v1 == b"TAG" {
            total_size = total_size.saturating_add(128);
        }
    }

    Some(total_size)
}

#[cfg(test)]
mod tests {
    use super::{estimate_bitrate_kbps, LibraryAudioProperties};
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("roqtune_technical_metadata_{name}_{nanos}.bin"))
    }

    fn syncsafe(size: u32) -> [u8; 4] {
        [
            ((size >> 21) & 0x7F) as u8,
            ((size >> 14) & 0x7F) as u8,
            ((size >> 7) & 0x7F) as u8,
            (size & 0x7F) as u8,
        ]
    }

    #[test]
    fn estimate_bitrate_prefers_library_audio_bitrate() {
        let bitrate = estimate_bitrate_kbps(
            PathBuf::from("/nonexistent").as_path(),
            120_000,
            Some(&LibraryAudioProperties {
                duration_ms: 120_000,
                audio_bitrate_kbps: 777,
                overall_bitrate_kbps: 999,
                metadata_size_bytes: 0,
            }),
        );
        assert_eq!(bitrate, 777);
    }

    #[test]
    fn estimate_bitrate_falls_back_to_library_overall_bitrate() {
        let bitrate = estimate_bitrate_kbps(
            PathBuf::from("/nonexistent").as_path(),
            120_000,
            Some(&LibraryAudioProperties {
                duration_ms: 120_000,
                audio_bitrate_kbps: 0,
                overall_bitrate_kbps: 512,
                metadata_size_bytes: 0,
            }),
        );
        assert_eq!(bitrate, 512);
    }

    #[test]
    fn estimate_bitrate_fallback_subtracts_legacy_id3v2_block() {
        let path = temp_path("id3v2_payload_size");
        let id3_payload_size = 1_000_000u32;
        let audio_payload_size = 2_000usize;
        let mut bytes = Vec::with_capacity(10 + id3_payload_size as usize + audio_payload_size);
        bytes.extend_from_slice(b"ID3");
        bytes.extend_from_slice(&[4, 0, 0]);
        bytes.extend_from_slice(&syncsafe(id3_payload_size));
        bytes.resize(10 + id3_payload_size as usize, 0);
        bytes.resize(10 + id3_payload_size as usize + audio_payload_size, 0xAB);
        let mut file = fs::File::create(&path).expect("create temp file");
        file.write_all(&bytes).expect("write temp file");
        file.flush().expect("flush temp file");

        let bitrate = estimate_bitrate_kbps(path.as_path(), 2_000, None);
        let _ = fs::remove_file(path);
        assert_eq!(bitrate, 8);
    }
}
