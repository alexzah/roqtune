//! Metadata read/write runtime component.
//!
//! This manager serves track Properties payloads and persists edited metadata
//! values back to audio files, then synchronizes library index rows when present.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Cursor, Seek};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use log::{debug, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use lofty::aac::AacFile;
use lofty::ape::ApeFile;
use lofty::config::WriteOptions;
use lofty::config::{ParseOptions, ParsingMode};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
use lofty::iff::wav::WavFile;
use lofty::mp4::Mp4File;
use lofty::mpeg::MpegFile;
use lofty::ogg::{OpusFile, VorbisFile};
use lofty::picture::{Picture, PictureInformation, PictureType};
use lofty::prelude::Accessor;
use lofty::read_from_path;
use lofty::tag::{ItemKey, MergeTag, SplitTag, Tag, TagType};
use lofty::wavpack::WavPackFile;
use lofty::TextEncoding;

use crate::config::LoudnessStandard;
use crate::db_manager::DbManager;
use crate::image_pipeline::{self, ManagedImageKind};
use crate::metadata::replaygain_analyzer;
use crate::metadata_tags;
use crate::protocol::{
    Message, MetadataEditorField, MetadataMessage, PropertiesEmbeddedImageSlot,
    PropertiesExternalImage, PropertiesImageDelete, PropertiesImageOverwrite,
    PropertiesMediaInfoField, TrackMetadataSummary,
};

const COMMON_FIELD_SPECS: [(&str, &str); 17] = [
    ("common:title", "Title"),
    ("common:artist", "Artist"),
    ("common:album", "Album"),
    ("common:album_artist", "Album Artist"),
    ("common:track_number", "Track Number"),
    ("common:track_total", "Track Total"),
    ("common:disc_number", "Disc Number"),
    ("common:disc_total", "Disc Total"),
    ("common:year", "Year"),
    ("common:date", "Date"),
    ("common:genre", "Genre"),
    ("common:composer", "Composer"),
    ("common:comment", "Comment"),
    ("common:bpm", "BPM"),
    ("common:isrc", "ISRC"),
    ("common:publisher", "Publisher"),
    ("common:copyright", "Copyright"),
];

const COMMON_IMAGE_SLOT_SPECS: [(u8, &str); 8] = [
    (3, "Front Cover"),
    (4, "Back Cover"),
    (6, "Media / Disc"),
    (5, "Leaflet"),
    (8, "Artist"),
    (19, "Band Logo"),
    (1, "Icon"),
    (0, "Other"),
];

type TrackPropertiesPayload = (
    String,
    Vec<MetadataEditorField>,
    Vec<PropertiesMediaInfoField>,
    Vec<PropertiesEmbeddedImageSlot>,
    Vec<PropertiesExternalImage>,
);
type ReplayGainScanTargetsPayload = (
    Vec<ReplayGainScanTarget>,
    HashMap<ReplayGainAlbumKey, Vec<PathBuf>>,
);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReplayGainAlbumKey {
    album: String,
    album_artist: String,
}

#[derive(Debug, Clone, Copy)]
struct ReplayGainScanValues {
    gain_db: f32,
    peak: f32,
}

#[derive(Debug, Clone)]
struct ReplayGainScanTarget {
    path: PathBuf,
    album_key: ReplayGainAlbumKey,
    has_existing_tags: bool,
}

#[derive(Debug, Clone)]
struct ReplayGainScanProgress {
    processed: usize,
    total_tracks: usize,
    updated: usize,
    skipped: usize,
    failed: usize,
    current_track_label: String,
}

struct MediaInfoFieldInput<'a> {
    path: &'a Path,
    extension: String,
    file_size_bytes: u64,
    modified_text: String,
    duration_ms: u64,
    sample_rate_hz: u32,
    channels: u16,
    bit_depth: u16,
    audio_bitrate_kbps: u32,
    overall_bitrate_kbps: u32,
    primary_tag_type: String,
    embedded_artwork_count: usize,
    external_cover_image_count: usize,
}

/// Coordinates metadata properties loading/saving for one-file workflows.
pub struct MetadataManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    db_manager: DbManager,
}

impl MetadataManager {
    /// Creates a metadata manager bound to bus channels and storage backend.
    pub fn new(
        bus_consumer: Receiver<Message>,
        bus_producer: Sender<Message>,
        db_manager: DbManager,
    ) -> Self {
        Self {
            bus_consumer,
            bus_producer,
            db_manager,
        }
    }

    fn key_technical_name(tag: &Tag, key: ItemKey) -> String {
        key.map_key(tag.tag_type())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{key:?}"))
    }

    fn get_common_value(tag: Option<&Tag>, field_id: &str) -> String {
        let Some(tag) = tag else {
            return String::new();
        };

        match field_id {
            "common:title" => tag
                .title()
                .map(|value| value.into_owned())
                .unwrap_or_default(),
            "common:artist" => tag
                .artist()
                .map(|value| value.into_owned())
                .unwrap_or_default(),
            "common:album" => tag
                .album()
                .map(|value| value.into_owned())
                .unwrap_or_default(),
            "common:album_artist" => tag
                .get_string(ItemKey::AlbumArtist)
                .or_else(|| tag.get_string(ItemKey::TrackArtist))
                .unwrap_or_default()
                .to_string(),
            "common:track_number" => tag
                .get_string(ItemKey::TrackNumber)
                .map(str::to_string)
                .or_else(|| tag.track().map(|value| value.to_string()))
                .unwrap_or_default(),
            "common:track_total" => tag
                .get_string(ItemKey::TrackTotal)
                .map(str::to_string)
                .or_else(|| tag.track_total().map(|value| value.to_string()))
                .unwrap_or_default(),
            "common:disc_number" => tag
                .get_string(ItemKey::DiscNumber)
                .map(str::to_string)
                .or_else(|| tag.disk().map(|value| value.to_string()))
                .unwrap_or_default(),
            "common:disc_total" => tag
                .get_string(ItemKey::DiscTotal)
                .map(str::to_string)
                .or_else(|| tag.disk_total().map(|value| value.to_string()))
                .unwrap_or_default(),
            "common:year" => tag
                .get_string(ItemKey::Year)
                .unwrap_or_default()
                .to_string(),
            "common:date" => tag
                .get_string(ItemKey::RecordingDate)
                .unwrap_or_default()
                .to_string(),
            "common:genre" => tag
                .genre()
                .map(|value| value.into_owned())
                .unwrap_or_default(),
            "common:composer" => tag
                .get_string(ItemKey::Composer)
                .unwrap_or_default()
                .to_string(),
            "common:comment" => tag
                .comment()
                .map(|value| value.into_owned())
                .or_else(|| tag.get_string(ItemKey::Comment).map(str::to_string))
                .unwrap_or_default(),
            "common:bpm" => {
                if let Some(value) = tag.get_string(ItemKey::Bpm) {
                    value.to_string()
                } else {
                    tag.get_string(ItemKey::IntegerBpm)
                        .unwrap_or_default()
                        .to_string()
                }
            }
            "common:isrc" => tag
                .get_string(ItemKey::Isrc)
                .unwrap_or_default()
                .to_string(),
            "common:publisher" => tag
                .get_string(ItemKey::Publisher)
                .unwrap_or_default()
                .to_string(),
            "common:copyright" => tag
                .get_string(ItemKey::CopyrightMessage)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        }
    }

    fn common_item_key_for_tag_type(field_id: &str, tag_type: TagType) -> Option<ItemKey> {
        match field_id {
            "common:title" => Some(ItemKey::TrackTitle),
            "common:artist" => Some(ItemKey::TrackArtist),
            "common:album" => Some(ItemKey::AlbumTitle),
            "common:album_artist" => Some(ItemKey::AlbumArtist),
            "common:track_number" => Some(ItemKey::TrackNumber),
            "common:track_total" => Some(ItemKey::TrackTotal),
            "common:disc_number" => Some(ItemKey::DiscNumber),
            "common:disc_total" => Some(ItemKey::DiscTotal),
            "common:year" => Some(ItemKey::Year),
            "common:date" => Some(ItemKey::RecordingDate),
            "common:genre" => Some(ItemKey::Genre),
            "common:composer" => Some(ItemKey::Composer),
            "common:comment" => Some(ItemKey::Comment),
            "common:bpm" => {
                if ItemKey::Bpm.map_key(tag_type).is_some() {
                    Some(ItemKey::Bpm)
                } else {
                    Some(ItemKey::IntegerBpm)
                }
            }
            "common:isrc" => Some(ItemKey::Isrc),
            "common:publisher" => Some(ItemKey::Publisher),
            "common:copyright" => Some(ItemKey::CopyrightMessage),
            _ => None,
        }
    }

    fn common_item_key(field_id: &str, tag: &Tag) -> Option<ItemKey> {
        Self::common_item_key_for_tag_type(field_id, tag.tag_type())
    }

    fn common_item_keys(tag: &Tag) -> HashSet<ItemKey> {
        COMMON_FIELD_SPECS
            .iter()
            .filter_map(|(id, _)| Self::common_item_key(id, tag))
            .collect()
    }

    fn collect_extra_field_values(tag: &Tag, key: ItemKey) -> String {
        let mut values: Vec<String> = tag.get_strings(key).map(str::to_string).collect();
        values.extend(tag.get_locators(key).map(str::to_string));
        values.join("; ")
    }

    fn get_common_fallback_value(
        metadata: Option<&metadata_tags::CommonTrackMetadata>,
        field_id: &str,
    ) -> String {
        let Some(metadata) = metadata else {
            return String::new();
        };

        match field_id {
            "common:title" => metadata.title.clone(),
            "common:artist" => metadata.artist.clone(),
            "common:album" => metadata.album.clone(),
            "common:album_artist" => metadata.album_artist.clone(),
            "common:track_number" => metadata.track_number.clone(),
            "common:year" => metadata.year.clone(),
            "common:date" => metadata.date.clone(),
            "common:genre" => metadata.genre.clone(),
            _ => String::new(),
        }
    }

    fn get_common_value_from_sources(
        tag: Option<&Tag>,
        fallback: Option<&metadata_tags::CommonTrackMetadata>,
        field_id: &str,
    ) -> String {
        let primary = Self::get_common_value(tag, field_id);
        if !primary.trim().is_empty() {
            return primary;
        }
        Self::get_common_fallback_value(fallback, field_id)
    }

    fn format_rate_hz_text(rate_hz: u32) -> String {
        if rate_hz >= 1000 {
            let khz = rate_hz as f32 / 1000.0;
            if khz == khz.round() {
                format!("{} kHz", khz as u32)
            } else {
                format!("{khz:.1} kHz")
            }
        } else if rate_hz > 0 {
            format!("{rate_hz} Hz")
        } else {
            "Unknown".to_string()
        }
    }

    fn format_duration_text(duration_ms: u64) -> String {
        if duration_ms == 0 {
            return "Unknown".to_string();
        }
        let total_seconds = duration_ms / 1000;
        let hours = total_seconds / 3600;
        let minutes = (total_seconds % 3600) / 60;
        let seconds = total_seconds % 60;
        if hours > 0 {
            format!("{hours}:{minutes:02}:{seconds:02}")
        } else {
            format!("{minutes}:{seconds:02}")
        }
    }

    fn format_file_size_text(file_size_bytes: u64) -> String {
        if file_size_bytes == 0 {
            return "Unknown".to_string();
        }
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * 1024.0;
        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        let bytes = file_size_bytes as f64;
        if bytes >= GB {
            format!("{:.2} GB ({file_size_bytes} bytes)", bytes / GB)
        } else if bytes >= MB {
            format!("{:.2} MB ({file_size_bytes} bytes)", bytes / MB)
        } else if bytes >= KB {
            format!("{:.2} KB ({file_size_bytes} bytes)", bytes / KB)
        } else {
            format!("{file_size_bytes} bytes")
        }
    }

    fn format_modified_local_text(path: &Path) -> String {
        let Ok(metadata) = std::fs::metadata(path) else {
            return "Unknown".to_string();
        };
        let Ok(modified) = metadata.modified() else {
            return "Unknown".to_string();
        };
        let modified_local: DateTime<Local> = DateTime::from(modified);
        modified_local.format("%Y-%m-%d %H:%M:%S %Z").to_string()
    }

    fn picture_type_label_from_code(code: u8) -> String {
        match code {
            0 => "Other".to_string(),
            1 => "Icon".to_string(),
            2 => "Other Icon".to_string(),
            3 => "Front Cover".to_string(),
            4 => "Back Cover".to_string(),
            5 => "Leaflet".to_string(),
            6 => "Media / Disc".to_string(),
            7 => "Lead Artist".to_string(),
            8 => "Artist".to_string(),
            9 => "Conductor".to_string(),
            10 => "Band".to_string(),
            11 => "Composer".to_string(),
            12 => "Lyricist".to_string(),
            13 => "Recording Location".to_string(),
            14 => "During Recording".to_string(),
            15 => "During Performance".to_string(),
            16 => "Screen Capture".to_string(),
            17 => "Bright Fish".to_string(),
            18 => "Illustration".to_string(),
            19 => "Band Logo".to_string(),
            20 => "Publisher Logo".to_string(),
            _ => format!("Picture Type {code}"),
        }
    }

    fn picture_details(picture: &Picture) -> String {
        let mut parts = Vec::new();
        if let Some(mime_type) = picture.mime_type() {
            parts.push(mime_type.as_str().to_string());
        }
        if let Ok(info) = PictureInformation::from_picture(picture) {
            if info.width > 0 && info.height > 0 {
                parts.push(format!("{}x{}", info.width, info.height));
            }
        }
        parts.push(format!("{} bytes", picture.data().len()));
        if let Some(description) = picture.description().map(str::trim) {
            if !description.is_empty() {
                parts.push(format!("\"{description}\""));
            }
        }
        parts.join(" | ")
    }

    fn external_image_type_label(file_name: &str) -> String {
        let stem = Path::new(file_name)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .trim();
        if stem.eq_ignore_ascii_case("cover") || stem.eq_ignore_ascii_case("front") {
            "Front Cover".to_string()
        } else if stem.eq_ignore_ascii_case("folder") {
            "Folder Cover".to_string()
        } else if stem.eq_ignore_ascii_case("album") {
            "Album Cover".to_string()
        } else if stem.eq_ignore_ascii_case("art") {
            "Artwork".to_string()
        } else if stem.is_empty() {
            "External Image".to_string()
        } else {
            format!("{stem} (external)")
        }
    }

    fn external_image_details(path: &Path, file_name: &str) -> String {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return format!("{file_name} | Unreadable"),
        };
        let mut cursor = Cursor::new(bytes.as_slice());
        if let Ok(picture) = Picture::from_reader(&mut cursor) {
            return format!("{file_name} | {}", Self::picture_details(&picture));
        }
        if let Ok((width, height)) = image::image_dimensions(path) {
            return format!("{file_name} | {width}x{height} | {} bytes", bytes.len());
        }
        format!("{file_name} | {} bytes", bytes.len())
    }

    fn image_preview_cache_path(
        track_path: &Path,
        picture_type_code: u8,
        picture: &Picture,
    ) -> Option<PathBuf> {
        let source_key = format!(
            "properties:{}:{}",
            track_path.to_string_lossy(),
            picture_type_code
        );
        image_pipeline::normalize_and_cache_original_bytes(
            ManagedImageKind::CoverArt,
            source_key.as_str(),
            picture.data(),
        )
    }

    fn external_cover_image_paths(track_path: &Path) -> Vec<PathBuf> {
        let Some(parent) = track_path.parent() else {
            return Vec::new();
        };
        let names = ["cover", "front", "folder", "album", "art"];
        let extensions = ["jpg", "jpeg", "png", "webp"];
        let mut found_files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(file_stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                    continue;
                };
                if !names
                    .iter()
                    .any(|candidate| file_stem.eq_ignore_ascii_case(candidate))
                {
                    continue;
                }
                if extensions
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
                {
                    found_files.push(path);
                }
            }
        }
        found_files.sort();
        found_files
    }

    fn collect_embedded_image_slots(
        track_path: &Path,
        primary_tag: Option<&Tag>,
        tags: &[Tag],
        fallback_cover_art: Option<&[u8]>,
    ) -> (usize, Vec<PropertiesEmbeddedImageSlot>) {
        let mut first_picture_by_code: HashMap<u8, Picture> = HashMap::new();
        let mut embedded_count = 0usize;
        if let Some(tag) = primary_tag {
            for picture in tag.pictures() {
                embedded_count = embedded_count.saturating_add(1);
                let picture_code = picture.pic_type().as_u8();
                first_picture_by_code
                    .entry(picture_code)
                    .or_insert_with(|| picture.clone());
            }
        }
        for tag in tags {
            if primary_tag.is_some_and(|primary| std::ptr::eq(primary, tag)) {
                continue;
            }
            for picture in tag.pictures() {
                embedded_count = embedded_count.saturating_add(1);
                let picture_code = picture.pic_type().as_u8();
                first_picture_by_code
                    .entry(picture_code)
                    .or_insert_with(|| picture.clone());
            }
        }
        if embedded_count == 0 {
            if let Some(bytes) = fallback_cover_art {
                let mut cursor = Cursor::new(bytes);
                if let Ok(mut picture) = Picture::from_reader(&mut cursor) {
                    picture.set_pic_type(PictureType::Other);
                    first_picture_by_code.insert(PictureType::Other.as_u8(), picture);
                    embedded_count = 1;
                }
            }
        }

        let mut slots = Vec::new();
        let common_codes: HashSet<u8> = COMMON_IMAGE_SLOT_SPECS
            .iter()
            .map(|(picture_type_code, _)| *picture_type_code)
            .collect();
        for (picture_type_code, label) in COMMON_IMAGE_SLOT_SPECS {
            if let Some(picture) = first_picture_by_code.get(&picture_type_code) {
                slots.push(PropertiesEmbeddedImageSlot {
                    picture_type_code,
                    label: label.to_string(),
                    image_path: Self::image_preview_cache_path(
                        track_path,
                        picture_type_code,
                        picture,
                    ),
                    has_image: true,
                    details: Self::picture_details(picture),
                    common: true,
                });
            } else {
                slots.push(PropertiesEmbeddedImageSlot {
                    picture_type_code,
                    label: label.to_string(),
                    image_path: None,
                    has_image: false,
                    details: "No embedded image".to_string(),
                    common: true,
                });
            }
        }

        let mut extra_codes: Vec<u8> = first_picture_by_code
            .keys()
            .copied()
            .filter(|code| !common_codes.contains(code))
            .collect();
        extra_codes.sort_unstable();
        for picture_type_code in extra_codes {
            let Some(picture) = first_picture_by_code.get(&picture_type_code) else {
                continue;
            };
            slots.push(PropertiesEmbeddedImageSlot {
                picture_type_code,
                label: Self::picture_type_label_from_code(picture_type_code),
                image_path: Self::image_preview_cache_path(track_path, picture_type_code, picture),
                has_image: true,
                details: Self::picture_details(picture),
                common: false,
            });
        }

        (embedded_count, slots)
    }

    fn build_media_info_fields(input: MediaInfoFieldInput<'_>) -> Vec<PropertiesMediaInfoField> {
        let MediaInfoFieldInput {
            path,
            extension,
            file_size_bytes,
            modified_text,
            duration_ms,
            sample_rate_hz,
            channels,
            bit_depth,
            audio_bitrate_kbps,
            overall_bitrate_kbps,
            primary_tag_type,
            embedded_artwork_count,
            external_cover_image_count,
        } = input;
        let channel_text = if channels == 0 {
            "Unknown".to_string()
        } else {
            channels.to_string()
        };
        let bit_depth_text = if bit_depth == 0 {
            "Unknown".to_string()
        } else {
            bit_depth.to_string()
        };
        let audio_bitrate_text = if audio_bitrate_kbps == 0 {
            "Unknown".to_string()
        } else {
            format!("{audio_bitrate_kbps} kbps")
        };
        let overall_bitrate_text = if overall_bitrate_kbps == 0 {
            "Unknown".to_string()
        } else {
            format!("{overall_bitrate_kbps} kbps")
        };
        vec![
            PropertiesMediaInfoField {
                id: "file_path".to_string(),
                label: "File path".to_string(),
                value: path.to_string_lossy().to_string(),
            },
            PropertiesMediaInfoField {
                id: "file_name".to_string(),
                label: "File name".to_string(),
                value: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_string(),
            },
            PropertiesMediaInfoField {
                id: "file_extension_container".to_string(),
                label: "File extension / container".to_string(),
                value: extension,
            },
            PropertiesMediaInfoField {
                id: "file_size".to_string(),
                label: "File size".to_string(),
                value: Self::format_file_size_text(file_size_bytes),
            },
            PropertiesMediaInfoField {
                id: "last_modified".to_string(),
                label: "Last modified (local)".to_string(),
                value: modified_text,
            },
            PropertiesMediaInfoField {
                id: "duration".to_string(),
                label: "Duration".to_string(),
                value: Self::format_duration_text(duration_ms),
            },
            PropertiesMediaInfoField {
                id: "sample_rate".to_string(),
                label: "Sample rate".to_string(),
                value: Self::format_rate_hz_text(sample_rate_hz),
            },
            PropertiesMediaInfoField {
                id: "channels".to_string(),
                label: "Channels".to_string(),
                value: channel_text,
            },
            PropertiesMediaInfoField {
                id: "bit_depth".to_string(),
                label: "Bit depth".to_string(),
                value: bit_depth_text,
            },
            PropertiesMediaInfoField {
                id: "audio_bitrate".to_string(),
                label: "Audio bitrate".to_string(),
                value: audio_bitrate_text,
            },
            PropertiesMediaInfoField {
                id: "overall_bitrate".to_string(),
                label: "Overall bitrate".to_string(),
                value: overall_bitrate_text,
            },
            PropertiesMediaInfoField {
                id: "primary_tag_type".to_string(),
                label: "Primary tag type".to_string(),
                value: primary_tag_type,
            },
            PropertiesMediaInfoField {
                id: "embedded_artwork_count".to_string(),
                label: "Embedded artwork count".to_string(),
                value: embedded_artwork_count.to_string(),
            },
            PropertiesMediaInfoField {
                id: "external_cover_count".to_string(),
                label: "External cover image count".to_string(),
                value: external_cover_image_count.to_string(),
            },
        ]
    }

    fn read_properties_payload(path: &Path) -> Result<TrackPropertiesPayload, String> {
        let tagged_file = match read_from_path(path) {
            Ok(tagged_file) => Some(tagged_file),
            Err(error) => {
                warn!(
                    "MetadataManager: lofty failed to read {}, using common metadata fallback: {}",
                    path.display(),
                    error
                );
                None
            }
        };
        let fallback_tagged_file = if tagged_file.is_none() {
            metadata_tags::read_tagged_file_for_metadata(path, true)
        } else {
            None
        };
        let tag_payload = tagged_file.as_ref().or(fallback_tagged_file.as_ref());
        let source_tag = tag_payload
            .as_ref()
            .and_then(|tagged| tagged.primary_tag().or_else(|| tagged.first_tag()));
        let source_tags = tag_payload
            .as_ref()
            .map(|tagged| tagged.tags())
            .unwrap_or(&[]);
        let common_fallback = metadata_tags::read_common_track_metadata(path);

        let title = Self::get_common_value_from_sources(
            source_tag,
            common_fallback.as_ref(),
            "common:title",
        );
        let display_name = if !title.trim().is_empty() {
            title
        } else {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_string()
        };

        let mut fields: Vec<MetadataEditorField> = COMMON_FIELD_SPECS
            .iter()
            .map(|(id, field_name)| MetadataEditorField {
                id: (*id).to_string(),
                field_name: (*field_name).to_string(),
                value: Self::get_common_value_from_sources(
                    source_tag,
                    common_fallback.as_ref(),
                    id,
                ),
                common: true,
            })
            .collect();

        if let Some(tag) = source_tag {
            let common_keys = Self::common_item_keys(tag);
            let mut seen_keys = HashSet::new();
            let mut extras = Vec::new();

            for item in tag.items() {
                let key = item.key();
                if common_keys.contains(&key) || !seen_keys.insert(key) {
                    continue;
                }
                let technical = Self::key_technical_name(tag, key);
                let value = Self::collect_extra_field_values(tag, key);
                if value.trim().is_empty() {
                    continue;
                }
                extras.push(MetadataEditorField {
                    id: format!("key:{technical}"),
                    field_name: technical,
                    value,
                    common: false,
                });
            }

            extras.sort_by(|left, right| {
                left.field_name
                    .to_ascii_lowercase()
                    .cmp(&right.field_name.to_ascii_lowercase())
            });
            fields.extend(extras);
        }

        let external_image_paths = Self::external_cover_image_paths(path);
        let external_images = external_image_paths
            .iter()
            .map(|external_path| {
                let file_name = external_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_string();
                PropertiesExternalImage {
                    label: Self::external_image_type_label(&file_name),
                    details: Self::external_image_details(external_path, &file_name),
                    file_name,
                    path: external_path.clone(),
                }
            })
            .collect::<Vec<_>>();

        let has_any_tag_pictures = source_tags.iter().any(|tag| !tag.pictures().is_empty());
        let fallback_cover_art = if has_any_tag_pictures {
            None
        } else {
            metadata_tags::read_embedded_cover_art(path)
        };
        let (embedded_artwork_count, embedded_image_slots) = Self::collect_embedded_image_slots(
            path,
            source_tag,
            source_tags,
            fallback_cover_art.as_deref(),
        );
        let file_size_bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        let modified_unix_secs = Self::format_modified_local_text(path);
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_uppercase())
            .unwrap_or_else(|| "Unknown".to_string());
        let (
            duration_ms,
            sample_rate_hz,
            channels,
            bit_depth,
            audio_bitrate_kbps,
            overall_bitrate_kbps,
            primary_tag_type,
        ) = if let Some(tagged) = tagged_file.as_ref() {
            let properties = tagged.properties();
            (
                properties.duration().as_millis() as u64,
                properties.sample_rate().unwrap_or(0),
                properties.channels().map(u16::from).unwrap_or(0),
                properties.bit_depth().map(u16::from).unwrap_or(0),
                properties.audio_bitrate().unwrap_or(0),
                properties.overall_bitrate().unwrap_or(0),
                format!("{:?}", tagged.primary_tag_type()),
            )
        } else if let Some(tagged) = tag_payload {
            (0, 0, 0, 0, 0, 0, format!("{:?}", tagged.primary_tag_type()))
        } else {
            (0, 0, 0, 0, 0, 0, "Unknown".to_string())
        };
        let media_info_fields = Self::build_media_info_fields(MediaInfoFieldInput {
            path,
            extension,
            file_size_bytes,
            modified_text: modified_unix_secs,
            duration_ms,
            sample_rate_hz,
            channels,
            bit_depth,
            audio_bitrate_kbps,
            overall_bitrate_kbps,
            primary_tag_type,
            embedded_artwork_count,
            external_cover_image_count: external_image_paths.len(),
        });

        Ok((
            display_name,
            fields,
            media_info_fields,
            embedded_image_slots,
            external_images,
        ))
    }

    fn apply_common_field(tag: &mut Tag, field_id: &str, value: &str) {
        let trimmed = value.trim();
        let is_empty = trimmed.is_empty();

        match field_id {
            "common:title" => {
                if is_empty {
                    tag.remove_title();
                } else {
                    tag.set_title(trimmed.to_string());
                }
            }
            "common:artist" => {
                if is_empty {
                    tag.remove_artist();
                } else {
                    tag.set_artist(trimmed.to_string());
                }
            }
            "common:album" => {
                if is_empty {
                    tag.remove_album();
                } else {
                    tag.set_album(trimmed.to_string());
                }
            }
            _ => {
                if let Some(key) = Self::common_item_key(field_id, tag) {
                    if field_id == "common:bpm" {
                        tag.remove_key(ItemKey::Bpm);
                        tag.remove_key(ItemKey::IntegerBpm);
                    } else if field_id == "common:year" {
                        tag.remove_key(ItemKey::Year);
                    } else if field_id == "common:date" {
                        tag.remove_key(ItemKey::RecordingDate);
                    } else {
                        tag.remove_key(key);
                    }

                    if !is_empty {
                        tag.insert_text(key, trimmed.to_string());
                    }
                }
            }
        }
    }

    fn build_summary(path: &Path, tag: Option<&Tag>) -> TrackMetadataSummary {
        let title = Self::get_common_value(tag, "common:title");
        let fallback_title = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();
        let date = Self::get_common_value(tag, "common:date");
        let year = {
            let direct = Self::get_common_value(tag, "common:year");
            if !direct.trim().is_empty() {
                direct
            } else if date.len() >= 4 {
                date[0..4].to_string()
            } else {
                String::new()
            }
        };

        TrackMetadataSummary {
            title: if title.trim().is_empty() {
                fallback_title
            } else {
                title
            },
            artist: Self::get_common_value(tag, "common:artist"),
            album: Self::get_common_value(tag, "common:album"),
            album_artist: Self::get_common_value(tag, "common:album_artist"),
            date,
            genre: Self::get_common_value(tag, "common:genre"),
            year,
            track_number: Self::get_common_value(tag, "common:track_number"),
        }
    }

    fn collect_editable_field_values(tag: &Tag) -> HashMap<String, String> {
        let mut values: HashMap<String, String> = COMMON_FIELD_SPECS
            .iter()
            .map(|(id, _)| ((*id).to_string(), Self::get_common_value(Some(tag), id)))
            .collect();

        let common_keys = Self::common_item_keys(tag);
        let mut seen_keys = HashSet::new();
        for item in tag.items() {
            let key = item.key();
            if common_keys.contains(&key) || !seen_keys.insert(key) {
                continue;
            }
            let technical = Self::key_technical_name(tag, key);
            let value = Self::collect_extra_field_values(tag, key);
            if value.trim().is_empty() {
                continue;
            }
            values.insert(format!("key:{technical}"), value);
        }

        values
    }

    fn apply_metadata_fields_to_tag(tag: &mut Tag, metadata_fields: &[MetadataEditorField]) {
        let current_field_values = Self::collect_editable_field_values(tag);
        let common_keys = Self::common_item_keys(tag);

        for (field_id, _) in COMMON_FIELD_SPECS {
            let Some(field) = metadata_fields.iter().find(|field| field.id == field_id) else {
                continue;
            };
            if current_field_values.get(field_id) == Some(&field.value) {
                continue;
            }
            Self::apply_common_field(tag, field_id, &field.value);
        }

        for field in metadata_fields {
            if field.common || !field.id.starts_with("key:") {
                continue;
            }
            if current_field_values.get(&field.id) == Some(&field.value) {
                continue;
            }
            let technical_name = &field.id["key:".len()..];
            let Some(item_key) = ItemKey::from_key(tag.tag_type(), technical_name) else {
                continue;
            };
            if common_keys.contains(&item_key) {
                continue;
            }
            if field.value.trim().is_empty() {
                tag.remove_key(item_key);
            } else {
                tag.insert_text(item_key, field.value.trim().to_string());
            }
        }
    }

    fn id3v2_frame_id(frame_id: &str) -> FrameId<'static> {
        FrameId::Valid(Cow::Owned(frame_id.to_string()))
    }

    fn set_id3v2_item_text(tag: &mut Id3v2Tag, item_key: ItemKey, value: &str) {
        let Some(mapped_key) = item_key.map_key(TagType::Id3v2) else {
            return;
        };
        if mapped_key.len() == 4 && mapped_key.starts_with('T') {
            tag.insert(Frame::Text(TextInformationFrame::new(
                FrameId::Valid(Cow::Owned(mapped_key.to_string())),
                TextEncoding::UTF8,
                value.to_string(),
            )));
        } else if mapped_key.len() != 4 {
            tag.insert_user_text(mapped_key.to_string(), value.to_string());
        }
    }

    fn remove_id3v2_item(tag: &mut Id3v2Tag, item_key: ItemKey) {
        let Some(mapped_key) = item_key.map_key(TagType::Id3v2) else {
            return;
        };
        if mapped_key.len() == 4 && mapped_key.starts_with('T') {
            let frame_id = Self::id3v2_frame_id(mapped_key);
            let _ = tag.remove(&frame_id);
        } else if mapped_key.len() != 4 {
            let _ = tag.remove_user_text(mapped_key);
        }
    }

    fn apply_common_field_id3v2(tag: &mut Id3v2Tag, field_id: &str, value: &str) {
        let trimmed = value.trim();
        let is_empty = trimmed.is_empty();
        match field_id {
            "common:title" => {
                if is_empty {
                    tag.remove_title();
                } else {
                    tag.set_title(trimmed.to_string());
                }
            }
            "common:artist" => {
                if is_empty {
                    tag.remove_artist();
                } else {
                    tag.set_artist(trimmed.to_string());
                }
            }
            "common:album" => {
                if is_empty {
                    tag.remove_album();
                } else {
                    tag.set_album(trimmed.to_string());
                }
            }
            "common:comment" => {
                if is_empty {
                    tag.remove_comment();
                } else {
                    tag.set_comment(trimmed.to_string());
                }
            }
            _ => {
                let Some(item_key) = Self::common_item_key_for_tag_type(field_id, TagType::Id3v2)
                else {
                    return;
                };
                if field_id == "common:bpm" {
                    Self::remove_id3v2_item(tag, ItemKey::Bpm);
                    Self::remove_id3v2_item(tag, ItemKey::IntegerBpm);
                } else if field_id == "common:year" {
                    Self::remove_id3v2_item(tag, ItemKey::Year);
                } else if field_id == "common:date" {
                    Self::remove_id3v2_item(tag, ItemKey::RecordingDate);
                } else {
                    Self::remove_id3v2_item(tag, item_key);
                }
                if !is_empty {
                    Self::set_id3v2_item_text(tag, item_key, trimmed);
                }
            }
        }
    }

    fn apply_metadata_fields_to_id3v2(tag: &mut Id3v2Tag, metadata_fields: &[MetadataEditorField]) {
        let generic_tag = Tag::from(tag.clone());
        let current_field_values = Self::collect_editable_field_values(&generic_tag);
        let common_keys: HashSet<ItemKey> = COMMON_FIELD_SPECS
            .iter()
            .filter_map(|(id, _)| Self::common_item_key_for_tag_type(id, TagType::Id3v2))
            .collect();

        for (field_id, _) in COMMON_FIELD_SPECS {
            let Some(field) = metadata_fields.iter().find(|field| field.id == field_id) else {
                continue;
            };
            if current_field_values.get(field_id) == Some(&field.value) {
                continue;
            }
            Self::apply_common_field_id3v2(tag, field_id, &field.value);
        }

        for field in metadata_fields {
            if field.common || !field.id.starts_with("key:") {
                continue;
            }
            if current_field_values.get(&field.id) == Some(&field.value) {
                continue;
            }
            let technical_name = &field.id["key:".len()..];
            let Some(item_key) = ItemKey::from_key(TagType::Id3v2, technical_name) else {
                continue;
            };
            if common_keys.contains(&item_key) {
                continue;
            }
            if field.value.trim().is_empty() {
                Self::remove_id3v2_item(tag, item_key);
            } else {
                Self::set_id3v2_item_text(tag, item_key, field.value.trim());
            }
        }
    }

    fn apply_image_edits_to_tag(
        tag: &mut Tag,
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) {
        for picture_type_code in prepared_image_deletes {
            let picture_type = PictureType::from_u8(*picture_type_code);
            tag.remove_picture_type(picture_type);
        }

        for (picture_type_code, picture) in prepared_image_overwrites {
            let picture_type = PictureType::from_u8(*picture_type_code);
            tag.remove_picture_type(picture_type);
            tag.push_picture(picture.clone());
        }

        tag.remove_empty();
    }

    fn apply_properties_edits_to_split_tag<T>(
        source_tag: T,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> T
    where
        T: SplitTag,
        T::Remainder: MergeTag<Merged = T>,
    {
        let (remainder, mut generic_tag) = source_tag.split_tag();
        Self::apply_metadata_fields_to_tag(&mut generic_tag, metadata_fields);
        Self::apply_image_edits_to_tag(
            &mut generic_tag,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        remainder.merge_tag(generic_tag)
    }

    fn prepare_image_overwrites(
        image_overwrites: &[PropertiesImageOverwrite],
    ) -> Result<Vec<(u8, Picture)>, String> {
        let mut overwrites_by_type: HashMap<u8, Picture> = HashMap::new();
        for overwrite in image_overwrites {
            if !overwrite.source_path.is_file() {
                return Err(format!(
                    "Selected image does not exist or is not a file: {}",
                    overwrite.source_path.display()
                ));
            }
            let bytes = std::fs::read(&overwrite.source_path).map_err(|error| {
                format!(
                    "Failed to read selected image {}: {}",
                    overwrite.source_path.display(),
                    error
                )
            })?;
            let mut cursor = Cursor::new(bytes);
            let mut picture = Picture::from_reader(&mut cursor).map_err(|error| {
                format!(
                    "Selected file is not a supported image {}: {}",
                    overwrite.source_path.display(),
                    error
                )
            })?;
            picture.set_pic_type(PictureType::from_u8(overwrite.picture_type_code));
            overwrites_by_type.insert(overwrite.picture_type_code, picture);
        }

        let mut prepared: Vec<(u8, Picture)> = overwrites_by_type.into_iter().collect();
        prepared.sort_by_key(|(picture_type_code, _)| *picture_type_code);
        Ok(prepared)
    }

    fn prepare_image_deletes(image_deletes: &[PropertiesImageDelete]) -> Vec<u8> {
        let mut delete_codes: Vec<u8> = image_deletes
            .iter()
            .map(|delete| delete.picture_type_code)
            .collect();
        delete_codes.sort_unstable();
        delete_codes.dedup();
        delete_codes
    }

    fn replaygain_album_key_from_fields(
        album: &str,
        album_artist: &str,
        fallback_path: &Path,
    ) -> ReplayGainAlbumKey {
        let album_key = album.trim().to_string();
        let album_artist_key = album_artist.trim().to_string();
        if album_key.is_empty() && album_artist_key.is_empty() {
            return ReplayGainAlbumKey {
                album: format!("path:{}", fallback_path.to_string_lossy()),
                album_artist: String::new(),
            };
        }
        ReplayGainAlbumKey {
            album: album_key,
            album_artist: album_artist_key,
        }
    }

    fn build_replaygain_scan_targets_from_request(
        request_targets: Vec<crate::protocol::ReplayGainScanTarget>,
        request_album_references: Vec<crate::protocol::ReplayGainAlbumReference>,
    ) -> Result<ReplayGainScanTargetsPayload, String> {
        let mut targets: Vec<ReplayGainScanTarget> = Vec::with_capacity(request_targets.len());
        let mut seen_paths = HashSet::new();
        let mut selected_paths_by_album: HashMap<ReplayGainAlbumKey, Vec<PathBuf>> = HashMap::new();
        for request_target in request_targets {
            if !request_target.path.is_file() {
                continue;
            }
            let dedupe_key = request_target.path.to_string_lossy().to_string();
            if !seen_paths.insert(dedupe_key) {
                continue;
            }
            let album_key = Self::replaygain_album_key_from_fields(
                &request_target.album,
                &request_target.album_artist,
                request_target.path.as_path(),
            );
            let has_existing_tags =
                metadata_tags::read_replay_gain_metadata(request_target.path.as_path()).is_some();
            targets.push(ReplayGainScanTarget {
                path: request_target.path.clone(),
                album_key: album_key.clone(),
                has_existing_tags,
            });
            selected_paths_by_album
                .entry(album_key)
                .or_default()
                .push(request_target.path);
        }
        if targets.is_empty() {
            return Err("No local files matched the selected tracks".to_string());
        }

        let mut album_reference_paths: HashMap<ReplayGainAlbumKey, Vec<PathBuf>> = HashMap::new();
        for album_reference in request_album_references {
            let album_key = Self::replaygain_album_key_from_fields(
                &album_reference.album,
                &album_reference.album_artist,
                Path::new(""),
            );
            let mut refs = album_reference
                .paths
                .into_iter()
                .filter(|path| path.is_file())
                .collect::<Vec<_>>();
            if refs.is_empty() {
                continue;
            }
            refs.sort_unstable();
            refs.dedup();
            album_reference_paths
                .entry(album_key)
                .or_default()
                .extend(refs);
        }

        for refs in album_reference_paths.values_mut() {
            refs.sort_unstable();
            refs.dedup();
        }

        for (album_key, selected_album_paths) in selected_paths_by_album {
            let refs = album_reference_paths.entry(album_key).or_default();
            if refs.is_empty() {
                *refs = selected_album_paths;
                refs.sort_unstable();
                refs.dedup();
            }
        }

        Ok((targets, album_reference_paths))
    }

    fn analyze_replaygain_for_track(
        path: &Path,
        loudness_standard: LoudnessStandard,
    ) -> Result<ReplayGainScanValues, String> {
        let values = replaygain_analyzer::analyze_track_values(path, loudness_standard).map_err(
            |error| {
                format!(
                    "ReplayGain track analysis failed for {}: {}",
                    path.display(),
                    error
                )
            },
        )?;
        Ok(ReplayGainScanValues {
            gain_db: values.gain_db,
            peak: values.peak,
        })
    }

    fn analyze_replaygain_for_album(
        paths: &[PathBuf],
        loudness_standard: LoudnessStandard,
    ) -> Result<ReplayGainScanValues, String> {
        let values = replaygain_analyzer::analyze_album_values(paths, loudness_standard)
            .map_err(|error| format!("ReplayGain album analysis failed: {}", error))?;
        Ok(ReplayGainScanValues {
            gain_db: values.gain_db,
            peak: values.peak,
        })
    }

    fn replaygain_tag_type_for_path(path: &Path) -> TagType {
        match FileType::from_path(path) {
            Some(FileType::Mpeg) | Some(FileType::Aac) | Some(FileType::Wav) => TagType::Id3v2,
            Some(FileType::Flac) | Some(FileType::Vorbis) | Some(FileType::Opus) => {
                TagType::VorbisComments
            }
            Some(FileType::Mp4) => TagType::Mp4Ilst,
            Some(FileType::Ape) | Some(FileType::WavPack) => TagType::Ape,
            _ => read_from_path(path)
                .map(|tagged_file| tagged_file.primary_tag_type())
                .unwrap_or(TagType::Id3v2),
        }
    }

    fn replaygain_field(
        tag_type: TagType,
        item_key: ItemKey,
        fallback_name: &'static str,
        value: String,
    ) -> MetadataEditorField {
        let mapped_key = item_key.map_key(tag_type).unwrap_or(fallback_name);
        MetadataEditorField {
            id: format!("key:{mapped_key}"),
            field_name: mapped_key.to_string(),
            value,
            common: false,
        }
    }

    fn replaygain_metadata_fields_for_path(
        path: &Path,
        track_values: ReplayGainScanValues,
        album_values: ReplayGainScanValues,
    ) -> Vec<MetadataEditorField> {
        let tag_type = Self::replaygain_tag_type_for_path(path);
        vec![
            Self::replaygain_field(
                tag_type,
                ItemKey::ReplayGainTrackGain,
                "REPLAYGAIN_TRACK_GAIN",
                format!("{:+.2} dB", track_values.gain_db),
            ),
            Self::replaygain_field(
                tag_type,
                ItemKey::ReplayGainTrackPeak,
                "REPLAYGAIN_TRACK_PEAK",
                format!("{:.6}", track_values.peak),
            ),
            Self::replaygain_field(
                tag_type,
                ItemKey::ReplayGainAlbumGain,
                "REPLAYGAIN_ALBUM_GAIN",
                format!("{:+.2} dB", album_values.gain_db),
            ),
            Self::replaygain_field(
                tag_type,
                ItemKey::ReplayGainAlbumPeak,
                "REPLAYGAIN_ALBUM_PEAK",
                format!("{:.6}", album_values.peak),
            ),
        ]
    }

    fn save_replaygain_tags_for_path(
        db_manager: &DbManager,
        path: &Path,
        track_values: ReplayGainScanValues,
        album_values: ReplayGainScanValues,
    ) -> Result<(), String> {
        let metadata_fields =
            Self::replaygain_metadata_fields_for_path(path, track_values, album_values);
        Self::save_track_properties_with_db(db_manager, path, &metadata_fields, &[], &[])
            .map(|_| ())
    }

    fn replaygain_scan_track_label(path: &Path) -> String {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    }

    fn publish_replaygain_scan_progress(
        bus_producer: &Sender<Message>,
        request_id: u64,
        progress: ReplayGainScanProgress,
    ) {
        let _ = bus_producer.send(Message::Metadata(MetadataMessage::ReplayGainScanProgress {
            request_id,
            processed: progress.processed,
            total_tracks: progress.total_tracks,
            updated: progress.updated,
            skipped: progress.skipped,
            failed: progress.failed,
            current_track_label: progress.current_track_label,
        }));
    }

    fn scan_replaygain_for_paths(
        bus_producer: &Sender<Message>,
        db_manager: &DbManager,
        request_id: u64,
        request_targets: Vec<crate::protocol::ReplayGainScanTarget>,
        request_album_references: Vec<crate::protocol::ReplayGainAlbumReference>,
        overwrite_existing: bool,
        loudness_standard: LoudnessStandard,
    ) {
        let (targets, album_reference_paths) =
            match Self::build_replaygain_scan_targets_from_request(
                request_targets,
                request_album_references,
            ) {
                Ok(payload) => payload,
                Err(error) => {
                    let _ = bus_producer.send(Message::Metadata(
                        MetadataMessage::ReplayGainScanFailed { request_id, error },
                    ));
                    return;
                }
            };
        if targets.is_empty() {
            let _ = bus_producer.send(Message::Metadata(MetadataMessage::ReplayGainScanFailed {
                request_id,
                error: "No tracks selected for ReplayGain scan".to_string(),
            }));
            return;
        }

        let total_tracks = targets.len();
        let _ = bus_producer.send(Message::Metadata(MetadataMessage::ReplayGainScanStarted {
            request_id,
            total_tracks,
        }));

        let mut processed = 0usize;
        let mut updated = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut album_scan_cache: HashMap<
            ReplayGainAlbumKey,
            Result<ReplayGainScanValues, String>,
        > = HashMap::new();
        let publish_progress = |processed: usize,
                                updated: usize,
                                skipped: usize,
                                failed: usize,
                                current_track_label: String| {
            Self::publish_replaygain_scan_progress(
                bus_producer,
                request_id,
                ReplayGainScanProgress {
                    processed,
                    total_tracks,
                    updated,
                    skipped,
                    failed,
                    current_track_label,
                },
            );
        };

        for target in targets {
            let current_track_label = Self::replaygain_scan_track_label(target.path.as_path());
            publish_progress(
                processed,
                updated,
                skipped,
                failed,
                current_track_label.clone(),
            );
            if target.has_existing_tags && !overwrite_existing {
                skipped = skipped.saturating_add(1);
                processed = processed.saturating_add(1);
                publish_progress(
                    processed,
                    updated,
                    skipped,
                    failed,
                    current_track_label.clone(),
                );
                continue;
            }

            let track_values = match Self::analyze_replaygain_for_track(
                target.path.as_path(),
                loudness_standard,
            ) {
                Ok(values) => values,
                Err(error) => {
                    failed = failed.saturating_add(1);
                    processed = processed.saturating_add(1);
                    warn!(
                        "ReplayGain track analysis failed for {}: {}",
                        target.path.display(),
                        error
                    );
                    publish_progress(
                        processed,
                        updated,
                        skipped,
                        failed,
                        current_track_label.clone(),
                    );
                    continue;
                }
            };

            if !album_scan_cache.contains_key(&target.album_key) {
                let album_paths = album_reference_paths
                    .get(&target.album_key)
                    .cloned()
                    .unwrap_or_else(|| vec![target.path.clone()]);
                album_scan_cache.insert(
                    target.album_key.clone(),
                    Self::analyze_replaygain_for_album(&album_paths, loudness_standard),
                );
            }

            let album_values = match album_scan_cache.get(&target.album_key) {
                Some(Ok(values)) => *values,
                Some(Err(error)) => {
                    failed = failed.saturating_add(1);
                    processed = processed.saturating_add(1);
                    warn!(
                        "ReplayGain album analysis failed for {}: {}",
                        target.path.display(),
                        error
                    );
                    publish_progress(
                        processed,
                        updated,
                        skipped,
                        failed,
                        current_track_label.clone(),
                    );
                    continue;
                }
                None => {
                    failed = failed.saturating_add(1);
                    processed = processed.saturating_add(1);
                    publish_progress(
                        processed,
                        updated,
                        skipped,
                        failed,
                        current_track_label.clone(),
                    );
                    continue;
                }
            };

            match Self::save_replaygain_tags_for_path(
                db_manager,
                target.path.as_path(),
                track_values,
                album_values,
            ) {
                Ok(()) => {
                    updated = updated.saturating_add(1);
                }
                Err(error) => {
                    failed = failed.saturating_add(1);
                    warn!(
                        "ReplayGain tag write failed for {}: {}",
                        target.path.display(),
                        error
                    );
                }
            }

            processed = processed.saturating_add(1);
            publish_progress(processed, updated, skipped, failed, current_track_label);
        }

        let _ = bus_producer.send(Message::Metadata(
            MetadataMessage::ReplayGainScanCompleted {
                request_id,
                total_tracks,
                updated,
                skipped,
                failed,
            },
        ));
    }

    fn spawn_replaygain_scan_for_paths(
        &self,
        request_id: u64,
        targets: Vec<crate::protocol::ReplayGainScanTarget>,
        album_references: Vec<crate::protocol::ReplayGainAlbumReference>,
        overwrite_existing: bool,
        loudness_standard: LoudnessStandard,
    ) {
        let bus_producer = self.bus_producer.clone();
        let spawn_result = std::thread::Builder::new()
            .name(format!("replaygain-scan-{request_id}"))
            .spawn(move || {
                let db_manager = match DbManager::new() {
                    Ok(db_manager) => db_manager,
                    Err(error) => {
                        let _ = bus_producer.send(Message::Metadata(
                            MetadataMessage::ReplayGainScanFailed {
                                request_id,
                                error: format!("Failed to initialize metadata database: {}", error),
                            },
                        ));
                        return;
                    }
                };
                Self::scan_replaygain_for_paths(
                    &bus_producer,
                    &db_manager,
                    request_id,
                    targets,
                    album_references,
                    overwrite_existing,
                    loudness_standard,
                );
            });

        if let Err(error) = spawn_result {
            let _ =
                self.bus_producer
                    .send(Message::Metadata(MetadataMessage::ReplayGainScanFailed {
                        request_id,
                        error: format!("Failed to start ReplayGain scan worker: {}", error),
                    }));
        }
    }

    fn save_track_properties(
        &self,
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        image_overwrites: &[PropertiesImageOverwrite],
        image_deletes: &[PropertiesImageDelete],
    ) -> Result<(TrackMetadataSummary, Option<String>), String> {
        Self::save_track_properties_with_db(
            &self.db_manager,
            path,
            metadata_fields,
            image_overwrites,
            image_deletes,
        )
    }

    fn save_track_properties_with_db(
        db_manager: &DbManager,
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        image_overwrites: &[PropertiesImageOverwrite],
        image_deletes: &[PropertiesImageDelete],
    ) -> Result<(TrackMetadataSummary, Option<String>), String> {
        if !path.is_file() {
            return Err(format!(
                "Track path is not a local file; saving properties is unavailable: {}",
                path.display()
            ));
        }
        let prepared_image_overwrites = Self::prepare_image_overwrites(image_overwrites)?;
        let prepared_image_deletes = Self::prepare_image_deletes(image_deletes);
        match Self::save_track_properties_lossless(
            path,
            metadata_fields,
            &prepared_image_overwrites,
            &prepared_image_deletes,
        ) {
            Ok(true) => {
                return Self::finalize_saved_track_properties(db_manager, path);
            }
            Ok(false) => {}
            Err(error) => {
                warn!(
                    "Lossless metadata save failed for {}; falling back to tolerant write: {}",
                    path.display(),
                    error
                );
            }
        }

        let mut tagged_file = match read_from_path(path) {
            Ok(tagged_file) => tagged_file,
            Err(primary_error) => metadata_tags::read_tagged_file_for_metadata(path, true)
                .ok_or_else(|| format!("Failed to read tags: {primary_error}"))?,
        };
        let tag_type = tagged_file.primary_tag_type();
        if tagged_file.tag(tag_type).is_none() {
            tagged_file.insert_tag(Tag::new(tag_type));
        }

        let tag = tagged_file
            .tag_mut(tag_type)
            .ok_or_else(|| format!("No writable tag available for {:?}", tag_type))?;
        Self::apply_metadata_fields_to_tag(tag, metadata_fields);
        Self::apply_image_edits_to_tag(tag, &prepared_image_overwrites, &prepared_image_deletes);
        tagged_file
            .save_to_path(path, WriteOptions::default())
            .map_err(|error| format!("Failed to write tags: {error}"))?;
        Self::finalize_saved_track_properties(db_manager, path)
    }

    fn save_track_properties_lossless(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<bool, String> {
        let Some(file_type) = FileType::from_path(path) else {
            return Ok(false);
        };

        match file_type {
            FileType::Mpeg => {
                Self::save_lossless_id3v2_properties_for_mpeg(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Aac => {
                Self::save_lossless_id3v2_properties_for_aac(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Wav => {
                Self::save_lossless_id3v2_properties_for_wav(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Flac => {
                Self::save_lossless_vorbis_properties_for_flac(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Vorbis => {
                Self::save_lossless_vorbis_properties_for_ogg_vorbis(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Opus => {
                Self::save_lossless_vorbis_properties_for_opus(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Mp4 => {
                Self::save_lossless_ilst_properties_for_mp4(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::Ape => {
                Self::save_lossless_ape_properties_for_ape_file(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            FileType::WavPack => {
                Self::save_lossless_ape_properties_for_wavpack(
                    path,
                    metadata_fields,
                    prepared_image_overwrites,
                    prepared_image_deletes,
                )?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn lossless_write_parse_options() -> ParseOptions {
        ParseOptions::new()
            .parsing_mode(ParsingMode::Relaxed)
            .max_junk_bytes(64 * 1024)
    }

    fn save_lossless_id3v2_properties_for_mpeg(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open MPEG for tag update: {error}"))?;
        let mut mpeg_file = MpegFile::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse MPEG tags: {error}"))?;
        let mut id3v2 = mpeg_file
            .id3v2_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        Self::apply_metadata_fields_to_id3v2(&mut id3v2, metadata_fields);
        for picture_type_code in prepared_image_deletes {
            id3v2.remove_picture_type(PictureType::from_u8(*picture_type_code));
        }
        for (picture_type_code, picture) in prepared_image_overwrites {
            let picture_type = PictureType::from_u8(*picture_type_code);
            id3v2.remove_picture_type(picture_type);
            id3v2.insert_picture(picture.clone());
        }
        mpeg_file.set_id3v2(id3v2);
        file.rewind()
            .map_err(|error| format!("Failed to rewind MPEG before write: {error}"))?;
        mpeg_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write MPEG tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_id3v2_properties_for_aac(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open AAC for tag update: {error}"))?;
        let mut aac_file = AacFile::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse AAC tags: {error}"))?;
        let mut id3v2 = aac_file.id3v2_mut().map(std::mem::take).unwrap_or_default();
        Self::apply_metadata_fields_to_id3v2(&mut id3v2, metadata_fields);
        for picture_type_code in prepared_image_deletes {
            id3v2.remove_picture_type(PictureType::from_u8(*picture_type_code));
        }
        for (picture_type_code, picture) in prepared_image_overwrites {
            let picture_type = PictureType::from_u8(*picture_type_code);
            id3v2.remove_picture_type(picture_type);
            id3v2.insert_picture(picture.clone());
        }
        aac_file.set_id3v2(id3v2);
        file.rewind()
            .map_err(|error| format!("Failed to rewind AAC before write: {error}"))?;
        aac_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write AAC tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_id3v2_properties_for_wav(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open WAV for tag update: {error}"))?;
        let mut wav_file = WavFile::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse WAV tags: {error}"))?;
        let mut id3v2 = wav_file.id3v2_mut().map(std::mem::take).unwrap_or_default();
        Self::apply_metadata_fields_to_id3v2(&mut id3v2, metadata_fields);
        for picture_type_code in prepared_image_deletes {
            id3v2.remove_picture_type(PictureType::from_u8(*picture_type_code));
        }
        for (picture_type_code, picture) in prepared_image_overwrites {
            let picture_type = PictureType::from_u8(*picture_type_code);
            id3v2.remove_picture_type(picture_type);
            id3v2.insert_picture(picture.clone());
        }
        wav_file.set_id3v2(id3v2);
        file.rewind()
            .map_err(|error| format!("Failed to rewind WAV before write: {error}"))?;
        wav_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write WAV tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_vorbis_properties_for_flac(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open FLAC for tag update: {error}"))?;
        let mut flac_file = FlacFile::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse FLAC tags: {error}"))?;
        let source_tag = flac_file
            .vorbis_comments_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let updated_tag = Self::apply_properties_edits_to_split_tag(
            source_tag,
            metadata_fields,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        flac_file.set_vorbis_comments(updated_tag);
        file.rewind()
            .map_err(|error| format!("Failed to rewind FLAC before write: {error}"))?;
        flac_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write FLAC tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_vorbis_properties_for_ogg_vorbis(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open OGG Vorbis for tag update: {error}"))?;
        let mut vorbis_file =
            VorbisFile::read_from(&mut file, Self::lossless_write_parse_options())
                .map_err(|error| format!("Failed to parse OGG Vorbis tags: {error}"))?;
        let source_tag = std::mem::take(vorbis_file.vorbis_comments_mut());
        let updated_tag = Self::apply_properties_edits_to_split_tag(
            source_tag,
            metadata_fields,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        *vorbis_file.vorbis_comments_mut() = updated_tag;
        file.rewind()
            .map_err(|error| format!("Failed to rewind OGG Vorbis before write: {error}"))?;
        vorbis_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write OGG Vorbis tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_vorbis_properties_for_opus(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open Opus for tag update: {error}"))?;
        let mut opus_file = OpusFile::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse Opus tags: {error}"))?;
        let source_tag = std::mem::take(opus_file.vorbis_comments_mut());
        let updated_tag = Self::apply_properties_edits_to_split_tag(
            source_tag,
            metadata_fields,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        *opus_file.vorbis_comments_mut() = updated_tag;
        file.rewind()
            .map_err(|error| format!("Failed to rewind Opus before write: {error}"))?;
        opus_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write Opus tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_ilst_properties_for_mp4(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open MP4 for tag update: {error}"))?;
        let mut mp4_file = Mp4File::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse MP4 tags: {error}"))?;
        let source_tag = mp4_file.ilst_mut().map(std::mem::take).unwrap_or_default();
        let updated_tag = Self::apply_properties_edits_to_split_tag(
            source_tag,
            metadata_fields,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        mp4_file.set_ilst(updated_tag);
        file.rewind()
            .map_err(|error| format!("Failed to rewind MP4 before write: {error}"))?;
        mp4_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write MP4 tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_ape_properties_for_ape_file(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open APE for tag update: {error}"))?;
        let mut ape_file = ApeFile::read_from(&mut file, Self::lossless_write_parse_options())
            .map_err(|error| format!("Failed to parse APE tags: {error}"))?;
        let source_tag = ape_file.ape_mut().map(std::mem::take).unwrap_or_default();
        let updated_tag = Self::apply_properties_edits_to_split_tag(
            source_tag,
            metadata_fields,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        ape_file.set_ape(updated_tag);
        file.rewind()
            .map_err(|error| format!("Failed to rewind APE before write: {error}"))?;
        ape_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write APE tags: {error}"))?;
        Ok(())
    }

    fn save_lossless_ape_properties_for_wavpack(
        path: &Path,
        metadata_fields: &[MetadataEditorField],
        prepared_image_overwrites: &[(u8, Picture)],
        prepared_image_deletes: &[u8],
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("Failed to open WavPack for tag update: {error}"))?;
        let mut wavpack_file =
            WavPackFile::read_from(&mut file, Self::lossless_write_parse_options())
                .map_err(|error| format!("Failed to parse WavPack tags: {error}"))?;
        let source_tag = wavpack_file
            .ape_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let updated_tag = Self::apply_properties_edits_to_split_tag(
            source_tag,
            metadata_fields,
            prepared_image_overwrites,
            prepared_image_deletes,
        );
        wavpack_file.set_ape(updated_tag);
        file.rewind()
            .map_err(|error| format!("Failed to rewind WavPack before write: {error}"))?;
        wavpack_file
            .save_to(&mut file, WriteOptions::default())
            .map_err(|error| format!("Failed to write WavPack tags: {error}"))?;
        Ok(())
    }

    fn finalize_saved_track_properties(
        db_manager: &DbManager,
        path: &Path,
    ) -> Result<(TrackMetadataSummary, Option<String>), String> {
        let refreshed = match read_from_path(path) {
            Ok(tagged_file) => tagged_file,
            Err(primary_error) => metadata_tags::read_tagged_file_for_metadata(path, true)
                .ok_or_else(|| format!("Failed to refresh tags: {primary_error}"))?,
        };
        let refreshed_tag = refreshed.primary_tag().or_else(|| refreshed.first_tag());
        let summary = Self::build_summary(path, refreshed_tag);

        let db_sync_warning = match db_manager
            .update_library_track_metadata_by_path(path.to_string_lossy().as_ref(), &summary)
        {
            Ok(_) => None,
            Err(error) => {
                warn!(
                    "MetadataManager: metadata saved but library index sync failed for {}: {}",
                    path.display(),
                    error
                );
                Some(format!(
                    "Metadata saved, but library index sync failed: {}. Consider running a rescan.",
                    error
                ))
            }
        };

        Ok((summary, db_sync_warning))
    }

    /// Starts the blocking event loop for metadata properties operations.
    pub fn run(&mut self) {
        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(Message::Metadata(MetadataMessage::RequestTrackProperties {
                    request_id,
                    path,
                })) => {
                    debug!(
                        "MetadataManager: loading properties request_id={} path={}",
                        request_id,
                        path.display()
                    );
                    match Self::read_properties_payload(&path) {
                        Ok((
                            display_name,
                            metadata_fields,
                            media_info_fields,
                            embedded_image_slots,
                            external_images,
                        )) => {
                            let _ = self.bus_producer.send(Message::Metadata(
                                MetadataMessage::TrackPropertiesLoaded {
                                    request_id,
                                    path,
                                    display_name,
                                    metadata_fields,
                                    media_info_fields,
                                    embedded_image_slots,
                                    external_images,
                                },
                            ));
                        }
                        Err(error) => {
                            let _ = self.bus_producer.send(Message::Metadata(
                                MetadataMessage::TrackPropertiesLoadFailed {
                                    request_id,
                                    path,
                                    error,
                                },
                            ));
                        }
                    }
                }
                Ok(Message::Metadata(MetadataMessage::SaveTrackProperties {
                    request_id,
                    path,
                    metadata_fields,
                    image_overwrites,
                    image_deletes,
                })) => {
                    debug!(
                        "MetadataManager: saving properties request_id={} path={}",
                        request_id,
                        path.display()
                    );
                    match self.save_track_properties(
                        &path,
                        &metadata_fields,
                        &image_overwrites,
                        &image_deletes,
                    ) {
                        Ok((summary, db_sync_warning)) => {
                            let _ = self.bus_producer.send(Message::Metadata(
                                MetadataMessage::TrackPropertiesSaved {
                                    request_id,
                                    path,
                                    summary,
                                    db_sync_warning,
                                },
                            ));
                        }
                        Err(error) => {
                            let _ = self.bus_producer.send(Message::Metadata(
                                MetadataMessage::TrackPropertiesSaveFailed {
                                    request_id,
                                    path,
                                    error,
                                },
                            ));
                        }
                    }
                }
                Ok(Message::Metadata(MetadataMessage::ScanReplayGainForPaths {
                    request_id,
                    targets,
                    album_references,
                    overwrite_existing,
                    loudness_standard,
                })) => {
                    self.spawn_replaygain_scan_for_paths(
                        request_id,
                        targets,
                        album_references,
                        overwrite_existing,
                        loudness_standard,
                    );
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(
                        "MetadataManager lagged on control bus, skipped {} message(s)",
                        skipped
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MetadataManager;
    use crate::protocol::{MetadataEditorField, PropertiesImageDelete, PropertiesImageOverwrite};
    use lofty::aac::AacFile;
    use lofty::ape::ApeItem;
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::flac::FlacFile;
    use lofty::id3::v1::Id3v1Tag;
    use lofty::iff::wav::WavFile;
    use lofty::mp4::{Atom, AtomData, AtomIdent, Mp4File};
    use lofty::mpeg::MpegFile;
    use lofty::ogg::{OpusFile, VorbisFile};
    use lofty::picture::{Picture, PictureType};
    use lofty::prelude::{Accessor, TagExt};
    use lofty::read_from_path;
    use lofty::tag::{ItemKey, ItemValue, Tag, TagType};
    use lofty::wavpack::WavPackFile;
    use std::borrow::Cow;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::{Cursor, Seek};
    use std::path::{Path, PathBuf};

    fn tiny_png_bytes() -> &'static [u8] {
        &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 15, 4,
            0, 9, 251, 3, 253, 167, 137, 129, 119, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ]
    }

    fn make_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "roqtune-metadata-manager-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("failed to write test file");
    }

    fn make_picture(pic_type: PictureType) -> Picture {
        let mut cursor = Cursor::new(tiny_png_bytes());
        let mut picture = Picture::from_reader(&mut cursor).expect("failed to parse sample image");
        picture.set_pic_type(pic_type);
        picture
    }

    fn metadata_fixture_path(file_name: &str) -> PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

        let fixture_root = manifest_dir
            .ancestors()
            .map(|ancestor| ancestor.join("tests/fixtures/metadata_preservation"))
            .find(|candidate| candidate.is_dir())
            .unwrap_or_else(|| {
                panic!(
                    "failed to locate metadata fixtures from manifest dir {}",
                    manifest_dir.display()
                )
            });

        fixture_root.join(file_name)
    }

    fn copy_metadata_fixture(file_name: &str) -> (PathBuf, PathBuf) {
        let dir = make_temp_dir("metadata-preservation-fixture");
        let source_path = metadata_fixture_path(file_name);
        let target_path = dir.join(file_name);
        fs::copy(&source_path, &target_path).expect("failed to copy metadata fixture");
        (dir, target_path)
    }

    fn metadata_title_field(value: &str) -> Vec<MetadataEditorField> {
        vec![MetadataEditorField {
            id: "common:title".to_string(),
            field_name: "Title".to_string(),
            value: value.to_string(),
            common: true,
        }]
    }

    #[test]
    fn test_build_replaygain_scan_targets_from_request_uses_supplied_album_references() {
        let dir = make_temp_dir("replaygain-request-references");
        let album_dir = dir.join("album");
        fs::create_dir_all(&album_dir).expect("failed to create album dir");
        let selected_path = album_dir.join("selected.mp3");
        let peer_path = album_dir.join("peer.mp3");
        write_file(&selected_path, b"selected");
        write_file(&peer_path, b"peer");

        let request_targets = vec![crate::protocol::ReplayGainScanTarget {
            path: selected_path.clone(),
            album: "Album".to_string(),
            album_artist: "Artist".to_string(),
        }];
        let request_album_references = vec![crate::protocol::ReplayGainAlbumReference {
            album: "Album".to_string(),
            album_artist: "Artist".to_string(),
            paths: vec![selected_path.clone(), peer_path.clone()],
        }];

        let (targets, references) = MetadataManager::build_replaygain_scan_targets_from_request(
            request_targets,
            request_album_references,
        )
        .expect("request payload should build scan targets");

        assert_eq!(targets.len(), 1);
        let target_key = targets[0].album_key.clone();
        let resolved_refs = references
            .get(&target_key)
            .expect("album references should include selected target album");
        assert_eq!(resolved_refs.len(), 2);
        assert!(resolved_refs.contains(&selected_path));
        assert!(resolved_refs.contains(&peer_path));

        fs::remove_dir_all(&dir).expect("failed to remove temp dir");
    }

    #[test]
    fn test_build_replaygain_scan_targets_from_request_falls_back_to_selected_paths() {
        let dir = make_temp_dir("replaygain-request-fallback");
        let album_dir = dir.join("album");
        fs::create_dir_all(&album_dir).expect("failed to create album dir");
        let selected_path = album_dir.join("selected.mp3");
        write_file(&selected_path, b"selected");

        let request_targets = vec![crate::protocol::ReplayGainScanTarget {
            path: selected_path.clone(),
            album: "Album".to_string(),
            album_artist: "Artist".to_string(),
        }];

        let (targets, references) = MetadataManager::build_replaygain_scan_targets_from_request(
            request_targets,
            Vec::new(),
        )
        .expect("request payload should build scan targets");

        assert_eq!(targets.len(), 1);
        let target_key = targets[0].album_key.clone();
        let resolved_refs = references
            .get(&target_key)
            .expect("fallback references should include selected target album");
        assert_eq!(resolved_refs, &vec![selected_path.clone()]);

        fs::remove_dir_all(&dir).expect("failed to remove temp dir");
    }

    fn staged_picture_overwrite() -> Vec<(u8, Picture)> {
        vec![(3, make_picture(PictureType::CoverFront))]
    }

    fn read_track_title(path: &Path) -> Option<String> {
        let tagged_file = read_from_path(path).ok()?;
        tagged_file
            .primary_tag()?
            .title()
            .map(|value| value.to_string())
    }

    fn seed_id3v2_user_text(path: &Path, key: &str, value: &str) {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open id3v2 fixture");

        match extension.as_str() {
            "mp3" => {
                let mut mpeg_file =
                    MpegFile::read_from(&mut file, ParseOptions::new()).expect("read mp3 failed");
                let mut id3v2 = mpeg_file
                    .id3v2_mut()
                    .map(std::mem::take)
                    .unwrap_or_default();
                let _ = id3v2.insert_user_text(key.to_string(), value.to_string());
                mpeg_file.set_id3v2(id3v2);
                file.rewind().expect("rewind mp3 failed");
                mpeg_file
                    .save_to(&mut file, WriteOptions::default())
                    .expect("write mp3 failed");
            }
            "aac" => {
                let mut aac_file =
                    AacFile::read_from(&mut file, ParseOptions::new()).expect("read aac failed");
                let mut id3v2 = aac_file.id3v2_mut().map(std::mem::take).unwrap_or_default();
                let _ = id3v2.insert_user_text(key.to_string(), value.to_string());
                aac_file.set_id3v2(id3v2);
                file.rewind().expect("rewind aac failed");
                aac_file
                    .save_to(&mut file, WriteOptions::default())
                    .expect("write aac failed");
            }
            "wav" => {
                let mut wav_file =
                    WavFile::read_from(&mut file, ParseOptions::new()).expect("read wav failed");
                let mut id3v2 = wav_file.id3v2_mut().map(std::mem::take).unwrap_or_default();
                let _ = id3v2.insert_user_text(key.to_string(), value.to_string());
                wav_file.set_id3v2(id3v2);
                file.rewind().expect("rewind wav failed");
                wav_file
                    .save_to(&mut file, WriteOptions::default())
                    .expect("write wav failed");
            }
            other => panic!("unsupported id3v2 fixture extension: {other}"),
        }
    }

    fn seed_id3v2_user_text_fields(path: &Path, fields: &[(&str, &str)]) {
        for (key, value) in fields {
            seed_id3v2_user_text(path, key, value);
        }
    }

    fn read_id3v2_user_text(path: &Path, key: &str) -> Option<String> {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        match extension.as_str() {
            "mp3" => {
                let mpeg_file = MpegFile::read_from(&mut file, ParseOptions::new()).ok()?;
                mpeg_file.id3v2()?.get_user_text(key).map(str::to_string)
            }
            "aac" => {
                let aac_file = AacFile::read_from(&mut file, ParseOptions::new()).ok()?;
                aac_file.id3v2()?.get_user_text(key).map(str::to_string)
            }
            "wav" => {
                let wav_file = WavFile::read_from(&mut file, ParseOptions::new()).ok()?;
                wav_file.id3v2()?.get_user_text(key).map(str::to_string)
            }
            _ => None,
        }
    }

    fn seed_mpeg_ape_text_fields(path: &Path, fields: &[(&str, &str)]) {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open mpeg ape fixture");
        let mut mpeg_file =
            MpegFile::read_from(&mut file, ParseOptions::new()).expect("read mpeg failed");
        let mut ape = mpeg_file.ape_mut().map(std::mem::take).unwrap_or_default();
        for (key, value) in fields {
            ape.insert(
                ApeItem::new((*key).to_string(), ItemValue::Text((*value).to_string()))
                    .expect("invalid ape item"),
            );
        }
        mpeg_file.set_ape(ape);
        file.rewind().expect("rewind mpeg ape failed");
        mpeg_file
            .save_to(&mut file, WriteOptions::default())
            .expect("write mpeg ape failed");
    }

    fn read_mpeg_ape_text(path: &Path, key: &str) -> Option<String> {
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        let mpeg_file = MpegFile::read_from(&mut file, ParseOptions::new()).ok()?;
        let ape = mpeg_file.ape()?;
        let item = ape.get(key)?;
        match item.value() {
            ItemValue::Text(value) => Some(value.clone()),
            _ => None,
        }
    }

    fn seed_mpeg_id3v1_only(path: &Path) {
        let _ = TagType::Id3v2.remove_from_path(path);
        let _ = TagType::Ape.remove_from_path(path);

        let mut tag = Id3v1Tag::new();
        tag.set_title("Legacy ID3v1 Title".to_string());
        tag.set_artist("Legacy ID3v1 Artist".to_string());
        tag.set_album("Legacy ID3v1 Album".to_string());
        tag.set_comment("Legacy ID3v1 Comment".to_string());
        tag.save_to_path(path, WriteOptions::default())
            .expect("write id3v1 failed");
    }

    fn read_mpeg_id3v1_snapshot(path: &Path) -> Option<(String, String, String, String)> {
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        let mpeg_file = MpegFile::read_from(&mut file, ParseOptions::new()).ok()?;
        let tag = mpeg_file.id3v1()?;
        Some((
            tag.title()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tag.artist()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tag.album()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tag.comment()
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ))
    }

    fn seed_wavpack_ape_text_fields(path: &Path, fields: &[(&str, &str)]) {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open wavpack ape fixture");
        let mut wavpack_file =
            WavPackFile::read_from(&mut file, ParseOptions::new()).expect("read wavpack failed");
        let mut ape = wavpack_file
            .ape_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        for (key, value) in fields {
            ape.insert(
                ApeItem::new((*key).to_string(), ItemValue::Text((*value).to_string()))
                    .expect("invalid wavpack ape item"),
            );
        }
        wavpack_file.set_ape(ape);
        file.rewind().expect("rewind wavpack ape failed");
        wavpack_file
            .save_to(&mut file, WriteOptions::default())
            .expect("write wavpack ape failed");
    }

    fn read_wavpack_ape_text(path: &Path, key: &str) -> Option<String> {
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        let wavpack_file = WavPackFile::read_from(&mut file, ParseOptions::new()).ok()?;
        let ape = wavpack_file.ape()?;
        let item = ape.get(key)?;
        match item.value() {
            ItemValue::Text(value) => Some(value.clone()),
            _ => None,
        }
    }

    fn seed_wavpack_id3v1_only(path: &Path) {
        let _ = TagType::Ape.remove_from_path(path);
        let _ = TagType::Id3v2.remove_from_path(path);

        let mut tag = Id3v1Tag::new();
        tag.set_title("Legacy WavPack ID3v1 Title".to_string());
        tag.set_artist("Legacy WavPack ID3v1 Artist".to_string());
        tag.set_album("Legacy WavPack ID3v1 Album".to_string());
        tag.set_comment("Legacy WavPack ID3v1 Comment".to_string());
        tag.save_to_path(path, WriteOptions::default())
            .expect("write wavpack id3v1 failed");
    }

    fn read_wavpack_id3v1_snapshot(path: &Path) -> Option<(String, String, String, String)> {
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        let wavpack_file = WavPackFile::read_from(&mut file, ParseOptions::new()).ok()?;
        let tag = wavpack_file.id3v1()?;
        Some((
            tag.title()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tag.artist()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tag.album()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            tag.comment()
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ))
    }

    fn seed_vorbis_comment(path: &Path, key: &str, value: &str) {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open vorbis fixture");
        match extension.as_str() {
            "flac" => {
                let mut flac_file =
                    FlacFile::read_from(&mut file, ParseOptions::new()).expect("read flac failed");
                let mut vorbis = flac_file
                    .vorbis_comments_mut()
                    .map(std::mem::take)
                    .unwrap_or_default();
                vorbis.push(key.to_string(), value.to_string());
                flac_file.set_vorbis_comments(vorbis);
                file.rewind().expect("rewind flac failed");
                flac_file
                    .save_to(&mut file, WriteOptions::default())
                    .expect("write flac failed");
            }
            "ogg" => {
                let mut vorbis_file = VorbisFile::read_from(&mut file, ParseOptions::new())
                    .expect("read ogg vorbis failed");
                vorbis_file
                    .vorbis_comments_mut()
                    .push(key.to_string(), value.to_string());
                file.rewind().expect("rewind ogg failed");
                vorbis_file
                    .save_to(&mut file, WriteOptions::default())
                    .expect("write ogg vorbis failed");
            }
            "opus" => {
                let mut opus_file =
                    OpusFile::read_from(&mut file, ParseOptions::new()).expect("read opus failed");
                opus_file
                    .vorbis_comments_mut()
                    .push(key.to_string(), value.to_string());
                file.rewind().expect("rewind opus failed");
                opus_file
                    .save_to(&mut file, WriteOptions::default())
                    .expect("write opus failed");
            }
            other => panic!("unsupported vorbis fixture extension: {other}"),
        }
    }

    fn seed_vorbis_comment_fields(path: &Path, fields: &[(&str, &str)]) {
        for (key, value) in fields {
            seed_vorbis_comment(path, key, value);
        }
    }

    fn read_vorbis_comment(path: &Path, key: &str) -> Option<String> {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        match extension.as_str() {
            "flac" => {
                let flac_file = FlacFile::read_from(&mut file, ParseOptions::new()).ok()?;
                flac_file
                    .vorbis_comments()
                    .and_then(|tag| tag.get(key))
                    .map(str::to_string)
            }
            "ogg" => {
                let vorbis_file = VorbisFile::read_from(&mut file, ParseOptions::new()).ok()?;
                vorbis_file.vorbis_comments().get(key).map(str::to_string)
            }
            "opus" => {
                let opus_file = OpusFile::read_from(&mut file, ParseOptions::new()).ok()?;
                opus_file.vorbis_comments().get(key).map(str::to_string)
            }
            _ => None,
        }
    }

    fn mp4_sentinel_ident(name: &'static str) -> AtomIdent<'static> {
        AtomIdent::Freeform {
            mean: Cow::Borrowed("com.roqtune"),
            name: Cow::Borrowed(name),
        }
    }

    fn seed_mp4_freeform(path: &Path, name: &'static str, value: &str) {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("failed to open mp4 fixture");
        let mut mp4_file =
            Mp4File::read_from(&mut file, ParseOptions::new()).expect("read mp4 failed");
        let mut ilst = mp4_file.ilst_mut().map(std::mem::take).unwrap_or_default();
        ilst.insert(Atom::new(
            mp4_sentinel_ident(name),
            AtomData::UTF8(value.to_string()),
        ));
        mp4_file.set_ilst(ilst);
        file.rewind().expect("rewind mp4 failed");
        mp4_file
            .save_to(&mut file, WriteOptions::default())
            .expect("write mp4 failed");
    }

    fn seed_mp4_freeform_fields(path: &Path, fields: &[(&'static str, &str)]) {
        for (name, value) in fields {
            seed_mp4_freeform(path, name, value);
        }
    }

    fn read_mp4_freeform(path: &Path, name: &'static str) -> Option<String> {
        let mut file = OpenOptions::new().read(true).open(path).ok()?;
        let mp4_file = Mp4File::read_from(&mut file, ParseOptions::new()).ok()?;
        let ilst = mp4_file.ilst()?;
        let ident = mp4_sentinel_ident(name);
        let atom = ilst.get(&ident)?;
        for data in atom.data() {
            match data {
                AtomData::UTF8(value) | AtomData::UTF16(value) => {
                    return Some(value.clone());
                }
                _ => {}
            }
        }
        None
    }

    #[test]
    fn test_external_cover_image_paths_returns_sorted_supported_matches() {
        let dir = make_temp_dir("external-covers");
        let track_path = dir.join("track.flac");
        write_file(&track_path, b"audio");

        let expected = vec![
            dir.join("cover.jpg"),
            dir.join("FRONT.PNG"),
            dir.join("folder.webp"),
            dir.join("album.JPEG"),
            dir.join("art.png"),
        ];
        for path in &expected {
            write_file(path, tiny_png_bytes());
        }

        write_file(&dir.join("cover.gif"), tiny_png_bytes());
        write_file(&dir.join("not-a-cover.jpg"), tiny_png_bytes());
        write_file(&dir.join("front.txt"), b"nope");

        let mut expected_sorted = expected.clone();
        expected_sorted.sort();

        let actual = MetadataManager::external_cover_image_paths(&track_path);
        assert_eq!(actual, expected_sorted);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_embedded_image_slots_includes_common_slots_when_empty() {
        let dir = make_temp_dir("slots-empty");
        let track_path = dir.join("track.mp3");
        write_file(&track_path, b"audio");

        let (embedded_count, slots) =
            MetadataManager::collect_embedded_image_slots(&track_path, None, &[], None);
        assert_eq!(embedded_count, 0);
        assert_eq!(slots.len(), super::COMMON_IMAGE_SLOT_SPECS.len());

        for ((expected_code, expected_label), slot) in
            super::COMMON_IMAGE_SLOT_SPECS.iter().zip(slots.iter())
        {
            assert_eq!(slot.picture_type_code, *expected_code);
            assert_eq!(slot.label, *expected_label);
            assert!(slot.common);
            assert!(slot.image_path.is_none());
            assert_eq!(slot.details, "No embedded image");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_embedded_image_slots_appends_uncommon_types() {
        let dir = make_temp_dir("slots-extras");
        let track_path = dir.join("track.mp3");
        write_file(&track_path, b"audio");

        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(make_picture(PictureType::CoverFront));
        tag.push_picture(make_picture(PictureType::Conductor));
        tag.push_picture(make_picture(PictureType::ScreenCapture));
        let tags = vec![tag];

        let (embedded_count, slots) =
            MetadataManager::collect_embedded_image_slots(&track_path, None, &tags, None);
        assert_eq!(embedded_count, 3);

        let common_len = super::COMMON_IMAGE_SLOT_SPECS.len();
        assert_eq!(slots.len(), common_len + 2);

        let common_codes: Vec<u8> = slots
            .iter()
            .take(common_len)
            .map(|slot| slot.picture_type_code)
            .collect();
        let expected_common_codes: Vec<u8> = super::COMMON_IMAGE_SLOT_SPECS
            .iter()
            .map(|(picture_type_code, _)| *picture_type_code)
            .collect();
        assert_eq!(common_codes, expected_common_codes);

        let extra_codes: Vec<u8> = slots
            .iter()
            .skip(common_len)
            .map(|slot| slot.picture_type_code)
            .collect();
        assert_eq!(extra_codes, vec![9, 16]);
        assert!(slots.iter().skip(common_len).all(|slot| !slot.common));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_embedded_image_slots_uses_secondary_tags_when_primary_has_no_pictures() {
        let dir = make_temp_dir("slots-secondary");
        let track_path = dir.join("track.mp3");
        write_file(&track_path, b"audio");

        let primary = Tag::new(TagType::Ape);
        let mut secondary = Tag::new(TagType::Id3v2);
        secondary.push_picture(make_picture(PictureType::CoverFront));
        let tags = vec![primary, secondary];

        let (embedded_count, slots) =
            MetadataManager::collect_embedded_image_slots(&track_path, Some(&tags[0]), &tags, None);
        assert_eq!(embedded_count, 1);
        assert!(slots
            .iter()
            .any(|slot| slot.picture_type_code == 3 && slot.details != "No embedded image"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prepare_image_overwrites_deduplicates_and_sorts_by_type() {
        let dir = make_temp_dir("overwrite-map");
        let front_a = dir.join("front-a.png");
        let front_b = dir.join("front-b.png");
        let back = dir.join("back.png");

        let mut red = image::RgbaImage::new(1, 1);
        red.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        red.save(&front_a).expect("failed to write front-a image");

        let mut green = image::RgbaImage::new(1, 1);
        green.put_pixel(0, 0, image::Rgba([0, 255, 0, 255]));
        green.save(&front_b).expect("failed to write front-b image");

        let mut blue = image::RgbaImage::new(1, 1);
        blue.put_pixel(0, 0, image::Rgba([0, 0, 255, 255]));
        blue.save(&back).expect("failed to write back image");

        let front_b_bytes = fs::read(&front_b).expect("failed to read front-b image");
        let back_bytes = fs::read(&back).expect("failed to read back image");

        let overwrites = vec![
            PropertiesImageOverwrite {
                picture_type_code: 4,
                source_path: back.clone(),
            },
            PropertiesImageOverwrite {
                picture_type_code: 3,
                source_path: front_a.clone(),
            },
            PropertiesImageOverwrite {
                picture_type_code: 3,
                source_path: front_b.clone(),
            },
        ];

        let prepared =
            MetadataManager::prepare_image_overwrites(&overwrites).expect("prepare failed");
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].0, 3);
        assert_eq!(prepared[1].0, 4);
        assert_eq!(prepared[0].1.pic_type().as_u8(), 3);
        assert_eq!(prepared[1].1.pic_type().as_u8(), 4);
        assert_eq!(prepared[0].1.data(), front_b_bytes.as_slice());
        assert_eq!(prepared[1].1.data(), back_bytes.as_slice());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prepare_image_deletes_deduplicates_and_sorts() {
        let deletes = vec![
            PropertiesImageDelete {
                picture_type_code: 8,
            },
            PropertiesImageDelete {
                picture_type_code: 3,
            },
            PropertiesImageDelete {
                picture_type_code: 8,
            },
            PropertiesImageDelete {
                picture_type_code: 4,
            },
        ];
        assert_eq!(
            MetadataManager::prepare_image_deletes(&deletes),
            vec![3, 4, 8]
        );
    }

    #[test]
    fn test_collect_embedded_image_slots_uses_fallback_cover_when_no_tags_have_pictures() {
        let dir = make_temp_dir("slots-fallback-cover");
        let track_path = dir.join("track.mp3");
        write_file(&track_path, b"audio");

        let (embedded_count, slots) = MetadataManager::collect_embedded_image_slots(
            &track_path,
            None,
            &[],
            Some(tiny_png_bytes()),
        );
        assert_eq!(embedded_count, 1);
        assert!(slots
            .iter()
            .any(|slot| slot.picture_type_code == 0 && slot.details != "No embedded image"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_editable_field_values_includes_replaygain_keys() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.insert_text(ItemKey::TrackTitle, "ReplayGain Fixture".to_string());
        tag.insert_text(ItemKey::ReplayGainTrackGain, "-9.20 dB".to_string());
        tag.insert_text(ItemKey::ReplayGainTrackPeak, "0.987654".to_string());
        tag.insert_text(ItemKey::ReplayGainAlbumGain, "-8.10 dB".to_string());
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, "0.998877".to_string());
        assert_eq!(
            MetadataManager::collect_editable_field_values(&tag)
                .get("key:REPLAYGAIN_TRACK_GAIN")
                .map(String::as_str),
            Some("-9.20 dB")
        );
        assert_eq!(
            MetadataManager::collect_editable_field_values(&tag)
                .get("key:REPLAYGAIN_ALBUM_GAIN")
                .map(String::as_str),
            Some("-8.10 dB")
        );
    }

    #[test]
    fn test_lossless_id3v2_save_preserves_user_text_for_supported_formats() {
        const ID3_SENTINEL_KEY: &str = "ROQTUNE_LOSSLESS_SENTINEL";
        const ID3_SENTINEL_VALUE: &str = "keep-id3";

        for fixture_name in ["base.mp3", "base.aac", "base.wav"] {
            let (dir, fixture_path) = copy_metadata_fixture(fixture_name);
            seed_id3v2_user_text(&fixture_path, ID3_SENTINEL_KEY, ID3_SENTINEL_VALUE);

            let prepared_image_overwrites = staged_picture_overwrite();
            let metadata_fields = metadata_title_field("Lossless ID3v2 Title");
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &metadata_fields,
                &prepared_image_overwrites,
                &[],
            )
            .expect("lossless id3v2 save failed");
            assert!(handled);

            assert_eq!(
                read_id3v2_user_text(&fixture_path, ID3_SENTINEL_KEY).as_deref(),
                Some(ID3_SENTINEL_VALUE)
            );
            assert_eq!(
                read_track_title(&fixture_path).as_deref(),
                Some("Lossless ID3v2 Title")
            );

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_lossless_vorbis_save_preserves_unmapped_comment_for_supported_formats() {
        const VORBIS_SENTINEL_KEY: &str = "ROQTUNE_LOSSLESS_SENTINEL";
        const VORBIS_SENTINEL_VALUE: &str = "keep-vorbis";

        for fixture_name in ["base.flac", "base.ogg", "base.opus"] {
            let (dir, fixture_path) = copy_metadata_fixture(fixture_name);
            seed_vorbis_comment(&fixture_path, VORBIS_SENTINEL_KEY, VORBIS_SENTINEL_VALUE);

            let prepared_image_overwrites = staged_picture_overwrite();
            let metadata_fields = metadata_title_field("Lossless Vorbis Title");
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &metadata_fields,
                &prepared_image_overwrites,
                &[],
            )
            .expect("lossless vorbis save failed");
            assert!(handled);

            assert_eq!(
                read_vorbis_comment(&fixture_path, VORBIS_SENTINEL_KEY).as_deref(),
                Some(VORBIS_SENTINEL_VALUE)
            );
            assert_eq!(
                read_track_title(&fixture_path).as_deref(),
                Some("Lossless Vorbis Title")
            );

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_lossless_mp4_save_preserves_freeform_atom_for_supported_formats() {
        const MP4_SENTINEL_NAME: &str = "LOSSLESS_SENTINEL";
        const MP4_SENTINEL_VALUE: &str = "keep-mp4";

        for fixture_name in ["base.m4a", "base.mp4"] {
            let (dir, fixture_path) = copy_metadata_fixture(fixture_name);
            seed_mp4_freeform(&fixture_path, MP4_SENTINEL_NAME, MP4_SENTINEL_VALUE);

            let prepared_image_overwrites = staged_picture_overwrite();
            let metadata_fields = metadata_title_field("Lossless MP4 Title");
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &metadata_fields,
                &prepared_image_overwrites,
                &[],
            )
            .expect("lossless mp4 save failed");
            assert!(handled);

            assert_eq!(
                read_mp4_freeform(&fixture_path, MP4_SENTINEL_NAME).as_deref(),
                Some(MP4_SENTINEL_VALUE)
            );
            assert_eq!(
                read_track_title(&fixture_path).as_deref(),
                Some("Lossless MP4 Title")
            );

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_lossless_artwork_edit_preserves_dense_id3v2_user_text_metadata() {
        const DENSE_ID3_FIELDS: &[(&str, &str)] = &[
            ("REPLAYGAIN_TRACK_GAIN", "-9.20 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
            ("REPLAYGAIN_ALBUM_GAIN", "-8.10 dB"),
            ("REPLAYGAIN_ALBUM_PEAK", "0.998877"),
            ("REPLAYGAIN_REFERENCE_LOUDNESS", "89.0 dB"),
            ("ROQTUNE_CUSTOM_FIELD_ALPHA", "alpha"),
            ("ROQTUNE_CUSTOM_FIELD_BETA", "beta=1"),
            ("roqtune_custom_field_lower", "lower"),
        ];

        for fixture_name in ["base.mp3", "base.aac", "base.wav"] {
            let (dir, fixture_path) = copy_metadata_fixture(fixture_name);
            seed_id3v2_user_text_fields(&fixture_path, DENSE_ID3_FIELDS);

            let prepared_image_overwrites = staged_picture_overwrite();
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &[],
                &prepared_image_overwrites,
                &[],
            )
            .expect("lossless id3v2 image-only save failed");
            assert!(handled);

            for (key, expected_value) in DENSE_ID3_FIELDS {
                assert_eq!(
                    read_id3v2_user_text(&fixture_path, key).as_deref(),
                    Some(*expected_value),
                    "fixture={fixture_name} key={key}"
                );
            }

            let metadata_fields = metadata_title_field("Dense ID3v2 Title");
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &metadata_fields,
                &[],
                &[],
            )
            .expect("lossless id3v2 metadata save failed");
            assert!(handled);
            assert_eq!(
                read_track_title(&fixture_path).as_deref(),
                Some("Dense ID3v2 Title")
            );
            for (key, expected_value) in DENSE_ID3_FIELDS {
                assert_eq!(
                    read_id3v2_user_text(&fixture_path, key).as_deref(),
                    Some(*expected_value),
                    "fixture={fixture_name} key={key}"
                );
            }

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_lossless_artwork_edit_preserves_dense_vorbis_comment_metadata() {
        const DENSE_VORBIS_FIELDS: &[(&str, &str)] = &[
            ("REPLAYGAIN_TRACK_GAIN", "-9.20 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
            ("REPLAYGAIN_ALBUM_GAIN", "-8.10 dB"),
            ("REPLAYGAIN_ALBUM_PEAK", "0.998877"),
            ("REPLAYGAIN_REFERENCE_LOUDNESS", "89.0 dB"),
            ("R128_TRACK_GAIN", "-289"),
            ("ROQTUNE_CUSTOM_FIELD_ALPHA", "alpha"),
            ("ROQTUNE_CUSTOM_FIELD_BETA", "beta=1"),
            ("roqtune_custom_field_lower", "lower"),
        ];

        for fixture_name in ["base.flac", "base.ogg", "base.opus"] {
            let (dir, fixture_path) = copy_metadata_fixture(fixture_name);
            seed_vorbis_comment_fields(&fixture_path, DENSE_VORBIS_FIELDS);

            let prepared_image_overwrites = staged_picture_overwrite();
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &[],
                &prepared_image_overwrites,
                &[],
            )
            .expect("lossless vorbis image-only save failed");
            assert!(handled);

            for (key, expected_value) in DENSE_VORBIS_FIELDS {
                assert_eq!(
                    read_vorbis_comment(&fixture_path, key).as_deref(),
                    Some(*expected_value),
                    "fixture={fixture_name} key={key}"
                );
            }

            let metadata_fields = metadata_title_field("Dense Vorbis Title");
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &metadata_fields,
                &[],
                &[],
            )
            .expect("lossless vorbis metadata save failed");
            assert!(handled);
            assert_eq!(
                read_track_title(&fixture_path).as_deref(),
                Some("Dense Vorbis Title")
            );
            for (key, expected_value) in DENSE_VORBIS_FIELDS {
                assert_eq!(
                    read_vorbis_comment(&fixture_path, key).as_deref(),
                    Some(*expected_value),
                    "fixture={fixture_name} key={key}"
                );
            }

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_lossless_artwork_edit_preserves_dense_mp4_freeform_metadata() {
        const DENSE_MP4_FIELDS: &[(&str, &str)] = &[
            ("replaygain_track_gain", "-9.20 dB"),
            ("replaygain_track_peak", "0.987654"),
            ("replaygain_album_gain", "-8.10 dB"),
            ("replaygain_album_peak", "0.998877"),
            ("replaygain_reference_loudness", "89.0 dB"),
            ("roqtune_custom_field_alpha", "alpha"),
            ("roqtune_custom_field_beta", "beta=1"),
            ("roqtune_custom_field_lower", "lower"),
        ];

        for fixture_name in ["base.m4a", "base.mp4"] {
            let (dir, fixture_path) = copy_metadata_fixture(fixture_name);
            seed_mp4_freeform_fields(&fixture_path, DENSE_MP4_FIELDS);

            let prepared_image_overwrites = staged_picture_overwrite();
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &[],
                &prepared_image_overwrites,
                &[],
            )
            .expect("lossless mp4 image-only save failed");
            assert!(handled);

            for (name, expected_value) in DENSE_MP4_FIELDS {
                assert_eq!(
                    read_mp4_freeform(&fixture_path, name).as_deref(),
                    Some(*expected_value),
                    "fixture={fixture_name} name={name}"
                );
            }

            let metadata_fields = metadata_title_field("Dense MP4 Title");
            let handled = MetadataManager::save_track_properties_lossless(
                &fixture_path,
                &metadata_fields,
                &[],
                &[],
            )
            .expect("lossless mp4 metadata save failed");
            assert!(handled);
            assert_eq!(
                read_track_title(&fixture_path).as_deref(),
                Some("Dense MP4 Title")
            );
            for (name, expected_value) in DENSE_MP4_FIELDS {
                assert_eq!(
                    read_mp4_freeform(&fixture_path, name).as_deref(),
                    Some(*expected_value),
                    "fixture={fixture_name} name={name}"
                );
            }

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_lossless_mpeg_edit_preserves_dense_ape_metadata() {
        const DENSE_APE_FIELDS: &[(&str, &str)] = &[
            ("REPLAYGAIN_TRACK_GAIN", "-9.20 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
            ("REPLAYGAIN_ALBUM_GAIN", "-8.10 dB"),
            ("REPLAYGAIN_ALBUM_PEAK", "0.998877"),
            ("REPLAYGAIN_REFERENCE_LOUDNESS", "89.0 dB"),
            ("ROQTUNE_CUSTOM_FIELD_ALPHA", "alpha"),
            ("ROQTUNE_CUSTOM_FIELD_BETA", "beta=1"),
            ("roqtune_custom_field_lower", "lower"),
        ];

        let (dir, fixture_path) = copy_metadata_fixture("base.mp3");
        seed_mpeg_ape_text_fields(&fixture_path, DENSE_APE_FIELDS);

        let prepared_image_overwrites = staged_picture_overwrite();
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &[],
            &prepared_image_overwrites,
            &[],
        )
        .expect("lossless mpeg image-only save failed");
        assert!(handled);

        for (key, expected_value) in DENSE_APE_FIELDS {
            assert_eq!(
                read_mpeg_ape_text(&fixture_path, key).as_deref(),
                Some(*expected_value),
                "key={key}"
            );
        }

        let metadata_fields = metadata_title_field("Dense MPEG Legacy Title");
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &metadata_fields,
            &[],
            &[],
        )
        .expect("lossless mpeg metadata save failed");
        assert!(handled);
        assert_eq!(
            read_track_title(&fixture_path).as_deref(),
            Some("Dense MPEG Legacy Title")
        );
        for (key, expected_value) in DENSE_APE_FIELDS {
            assert_eq!(
                read_mpeg_ape_text(&fixture_path, key).as_deref(),
                Some(*expected_value),
                "key={key}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lossless_mpeg_edit_preserves_id3v1_only_metadata() {
        let (dir, fixture_path) = copy_metadata_fixture("base.mp3");
        seed_mpeg_id3v1_only(&fixture_path);
        let before = read_mpeg_id3v1_snapshot(&fixture_path).expect("missing id3v1 before save");

        let prepared_image_overwrites = staged_picture_overwrite();
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &[],
            &prepared_image_overwrites,
            &[],
        )
        .expect("lossless mpeg image-only save failed");
        assert!(handled);
        let after_image =
            read_mpeg_id3v1_snapshot(&fixture_path).expect("missing id3v1 after image save");
        assert_eq!(after_image, before);

        let metadata_fields = metadata_title_field("ID3v2 New Title");
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &metadata_fields,
            &[],
            &[],
        )
        .expect("lossless mpeg metadata save failed");
        assert!(handled);
        let after_metadata =
            read_mpeg_id3v1_snapshot(&fixture_path).expect("missing id3v1 after metadata save");
        assert_eq!(after_metadata, before);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lossless_wavpack_edit_preserves_dense_ape_metadata() {
        const DENSE_APE_FIELDS: &[(&str, &str)] = &[
            ("REPLAYGAIN_TRACK_GAIN", "-9.20 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
            ("REPLAYGAIN_ALBUM_GAIN", "-8.10 dB"),
            ("REPLAYGAIN_ALBUM_PEAK", "0.998877"),
            ("REPLAYGAIN_REFERENCE_LOUDNESS", "89.0 dB"),
            ("ROQTUNE_CUSTOM_FIELD_ALPHA", "alpha"),
            ("ROQTUNE_CUSTOM_FIELD_BETA", "beta=1"),
            ("roqtune_custom_field_lower", "lower"),
        ];

        let (dir, fixture_path) = copy_metadata_fixture("base.wv");
        seed_wavpack_ape_text_fields(&fixture_path, DENSE_APE_FIELDS);

        let prepared_image_overwrites = staged_picture_overwrite();
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &[],
            &prepared_image_overwrites,
            &[],
        )
        .expect("lossless wavpack image-only save failed");
        assert!(handled);

        for (key, expected_value) in DENSE_APE_FIELDS {
            assert_eq!(
                read_wavpack_ape_text(&fixture_path, key).as_deref(),
                Some(*expected_value),
                "key={key}"
            );
        }

        let metadata_fields = metadata_title_field("Dense WavPack Legacy Title");
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &metadata_fields,
            &[],
            &[],
        )
        .expect("lossless wavpack metadata save failed");
        assert!(handled);
        assert_eq!(
            read_track_title(&fixture_path).as_deref(),
            Some("Dense WavPack Legacy Title")
        );
        for (key, expected_value) in DENSE_APE_FIELDS {
            assert_eq!(
                read_wavpack_ape_text(&fixture_path, key).as_deref(),
                Some(*expected_value),
                "key={key}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lossless_wavpack_edit_preserves_id3v1_only_metadata() {
        let (dir, fixture_path) = copy_metadata_fixture("base.wv");
        seed_wavpack_id3v1_only(&fixture_path);
        let before = read_wavpack_id3v1_snapshot(&fixture_path).expect("missing id3v1 before save");

        let prepared_image_overwrites = staged_picture_overwrite();
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &[],
            &prepared_image_overwrites,
            &[],
        )
        .expect("lossless wavpack image-only save failed");
        assert!(handled);
        let after_image =
            read_wavpack_id3v1_snapshot(&fixture_path).expect("missing id3v1 after image save");
        assert_eq!(after_image, before);

        let metadata_fields = metadata_title_field("WavPack New Title");
        let handled = MetadataManager::save_track_properties_lossless(
            &fixture_path,
            &metadata_fields,
            &[],
            &[],
        )
        .expect("lossless wavpack metadata save failed");
        assert!(handled);
        let after_metadata =
            read_wavpack_id3v1_snapshot(&fixture_path).expect("missing id3v1 after metadata save");
        assert_eq!(after_metadata, before);

        let _ = fs::remove_dir_all(&dir);
    }
}
