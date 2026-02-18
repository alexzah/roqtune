//! Metadata read/write runtime component.
//!
//! This manager serves track Properties payloads and persists edited metadata
//! values back to audio files, then synchronizes library index rows when present.

use std::collections::HashSet;
use std::path::Path;

use log::{debug, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::Accessor;
use lofty::read_from_path;
use lofty::tag::{ItemKey, Tag};

use crate::db_manager::DbManager;
use crate::protocol::{Message, MetadataEditorField, MetadataMessage, TrackMetadataSummary};

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

    fn read_properties_payload(path: &Path) -> Result<(String, Vec<MetadataEditorField>), String> {
        let tagged_file =
            read_from_path(path).map_err(|error| format!("Failed to read tags: {error}"))?;
        let source_tag = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag());

        let title = Self::get_common_value(source_tag, "common:title");
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
                value: Self::get_common_value(source_tag, id),
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

        Ok((display_name, fields))
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

    fn save_track_properties(
        &self,
        path: &Path,
        fields: &[MetadataEditorField],
    ) -> Result<(TrackMetadataSummary, Option<String>), String> {
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
            let value = fields
                .iter()
                .find(|field| field.id == field_id)
                .map(|field| field.value.as_str())
                .unwrap_or("");
            Self::apply_common_field(tag, field_id, value);
        }

        for field in fields {
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
                        Ok((display_name, fields)) => {
                            let _ = self.bus_producer.send(Message::Metadata(
                                MetadataMessage::TrackPropertiesLoaded {
                                    request_id,
                                    path,
                                    display_name,
                                    fields,
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
                    fields,
                })) => {
                    debug!(
                        "MetadataManager: saving properties request_id={} path={}",
                        request_id,
                        path.display()
                    );
                    match self.save_track_properties(&path, &fields) {
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
