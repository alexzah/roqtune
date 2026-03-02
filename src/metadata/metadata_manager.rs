//! Metadata read/write runtime component.
//!
//! This manager serves track Properties payloads and persists edited metadata
//! values back to audio files, then synchronizes library index rows when present.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use log::{debug, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{Picture, PictureInformation, PictureType};
use lofty::prelude::Accessor;
use lofty::read_from_path;
use lofty::tag::{ItemKey, Tag};

use crate::db_manager::DbManager;
use crate::image_pipeline::{self, ManagedImageKind};
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

    fn common_item_key(field_id: &str, tag: &Tag) -> Option<ItemKey> {
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
                if ItemKey::Bpm.map_key(tag.tag_type()).is_some() {
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

    fn save_track_properties(
        &self,
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
        let mut tagged_file =
            read_from_path(path).map_err(|error| format!("Failed to read tags: {error}"))?;
        let tag_type = tagged_file.primary_tag_type();
        if tagged_file.tag(tag_type).is_none() {
            tagged_file.insert_tag(Tag::new(tag_type));
        }

        let tag = tagged_file
            .tag_mut(tag_type)
            .ok_or_else(|| format!("No writable tag available for {:?}", tag_type))?;

        let common_keys = Self::common_item_keys(tag);

        for (field_id, _) in COMMON_FIELD_SPECS {
            let value = metadata_fields
                .iter()
                .find(|field| field.id == field_id)
                .map(|field| field.value.as_str())
                .unwrap_or("");
            Self::apply_common_field(tag, field_id, value);
        }

        for field in metadata_fields {
            if field.common || !field.id.starts_with("key:") {
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

        for picture_type_code in prepared_image_deletes {
            let picture_type = PictureType::from_u8(picture_type_code);
            tag.remove_picture_type(picture_type);
        }

        for (picture_type_code, picture) in prepared_image_overwrites {
            let picture_type = PictureType::from_u8(picture_type_code);
            tag.remove_picture_type(picture_type);
            tag.push_picture(picture);
        }

        tag.remove_empty();
        tagged_file
            .save_to_path(path, WriteOptions::default())
            .map_err(|error| format!("Failed to write tags: {error}"))?;

        let refreshed =
            read_from_path(path).map_err(|error| format!("Failed to refresh tags: {error}"))?;
        let refreshed_tag = refreshed.primary_tag().or_else(|| refreshed.first_tag());
        let summary = Self::build_summary(path, refreshed_tag);

        let db_sync_warning = match self
            .db_manager
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
    use crate::protocol::{PropertiesImageDelete, PropertiesImageOverwrite};
    use lofty::picture::{Picture, PictureType};
    use lofty::tag::{Tag, TagType};
    use std::fs;
    use std::io::Cursor;
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
}
