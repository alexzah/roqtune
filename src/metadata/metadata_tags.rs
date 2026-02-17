//! Shared tag/cover-art readers backed by `lofty`.

use std::path::Path;

use lofty::file::TaggedFileExt;
use lofty::prelude::Accessor;
use lofty::read_from_path;
use lofty::tag::{ItemKey, Tag};

/// Normalized common metadata values extracted from file tags.
#[derive(Debug, Clone, Default)]
pub struct CommonTrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub date: String,
    pub year: String,
    pub genre: String,
    pub track_number: String,
}

fn first_non_empty_value<F>(primary_tag: Option<&Tag>, tags: &[Tag], mut extractor: F) -> String
where
    F: FnMut(&Tag) -> Option<String>,
{
    if let Some(tag) = primary_tag {
        if let Some(value) = extractor(tag) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    for tag in tags {
        if let Some(value) = extractor(tag) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    String::new()
}

fn derive_year_from_date(date: &str) -> String {
    let year: String = date.chars().take(4).collect();
    if year.chars().count() == 4 {
        year
    } else {
        String::new()
    }
}

/// Reads normalized common metadata values from a media file.
pub fn read_common_track_metadata(path: &Path) -> Option<CommonTrackMetadata> {
    let tagged_file = read_from_path(path).ok()?;
    let primary_tag = tagged_file.primary_tag();
    let tags = tagged_file.tags();

    let title = first_non_empty_value(primary_tag, tags, |tag| {
        tag.title().map(|value| value.into_owned())
    });
    let artist = first_non_empty_value(primary_tag, tags, |tag| {
        tag.artist().map(|value| value.into_owned())
    });
    let album = first_non_empty_value(primary_tag, tags, |tag| {
        tag.album().map(|value| value.into_owned())
    });
    let album_artist = first_non_empty_value(primary_tag, tags, |tag| {
        tag.get_string(ItemKey::AlbumArtist)
            .or_else(|| tag.get_string(ItemKey::TrackArtist))
            .map(str::to_string)
    });
    let date = first_non_empty_value(primary_tag, tags, |tag| {
        tag.get_string(ItemKey::RecordingDate)
            .or_else(|| tag.get_string(ItemKey::OriginalReleaseDate))
            .or_else(|| tag.get_string(ItemKey::Year))
            .map(str::to_string)
    });
    let year = {
        let direct_year = first_non_empty_value(primary_tag, tags, |tag| {
            tag.get_string(ItemKey::Year).map(str::to_string)
        });
        if direct_year.is_empty() {
            derive_year_from_date(&date)
        } else {
            direct_year
        }
    };
    let genre = first_non_empty_value(primary_tag, tags, |tag| {
        tag.genre().map(|value| value.into_owned())
    });
    let track_number = first_non_empty_value(primary_tag, tags, |tag| {
        tag.get_string(ItemKey::TrackNumber)
            .map(str::to_string)
            .or_else(|| tag.track().map(|value| value.to_string()))
    });

    Some(CommonTrackMetadata {
        title,
        artist,
        album,
        album_artist,
        date,
        year,
        genre,
        track_number,
    })
}

/// Reads embedded cover-art bytes from a media file, if present.
pub fn read_embedded_cover_art(path: &Path) -> Option<Vec<u8>> {
    let tagged_file = read_from_path(path).ok()?;
    let primary_tag = tagged_file.primary_tag();
    let tags = tagged_file.tags();

    if let Some(tag) = primary_tag {
        if let Some(picture) = tag.pictures().first() {
            return Some(picture.data().to_vec());
        }
    }

    for tag in tags {
        if let Some(picture) = tag.pictures().first() {
            return Some(picture.data().to_vec());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::derive_year_from_date;

    #[test]
    fn test_derive_year_from_date_with_full_value() {
        assert_eq!(derive_year_from_date("1998-10-31"), "1998");
    }

    #[test]
    fn test_derive_year_from_date_with_short_value() {
        assert_eq!(derive_year_from_date("99"), "");
    }
}
