//! Shared tag/cover-art readers backed by `lofty`.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use lofty::config::{ParseOptions, ParsingMode};
use lofty::file::TaggedFile;
use lofty::file::TaggedFileExt;
use lofty::prelude::Accessor;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};
use log::{debug, warn};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{
    MetadataOptions, MetadataRevision, StandardTagKey, StandardVisualKey, Value as SymphoniaValue,
};
use symphonia::core::probe::Hint;

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
    let mut consecutive_digits = String::with_capacity(4);
    for ch in date.chars() {
        if ch.is_ascii_digit() {
            consecutive_digits.push(ch);
            if consecutive_digits.len() == 4 {
                return consecutive_digits;
            }
        } else {
            consecutive_digits.clear();
        }
    }
    String::new()
}

fn metadata_parse_options(
    read_cover_art: bool,
    parsing_mode: ParsingMode,
    max_junk_bytes: usize,
) -> ParseOptions {
    ParseOptions::new()
        .read_properties(false)
        .read_cover_art(read_cover_art)
        .parsing_mode(parsing_mode)
        .max_junk_bytes(max_junk_bytes)
}

fn read_tagged_file_for_metadata(path: &Path, read_cover_art: bool) -> Option<TaggedFile> {
    let primary_options = metadata_parse_options(read_cover_art, ParsingMode::BestAttempt, 1024);
    let relaxed_options = metadata_parse_options(read_cover_art, ParsingMode::Relaxed, 64 * 1024);

    match Probe::open(path) {
        Ok(probe) => match probe.options(primary_options).read() {
            Ok(tagged_file) => return Some(tagged_file),
            Err(primary_error) => {
                debug!(
                    "Metadata read primary parse failed for {}: {}",
                    path.display(),
                    primary_error
                );
            }
        },
        Err(open_error) => {
            debug!(
                "Metadata read could not open {} with extension-based probe: {}",
                path.display(),
                open_error
            );
        }
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            debug!(
                "Metadata read failed for {} while preparing relaxed/content-based fallback: {}",
                path.display(),
                error
            );
            return None;
        }
    };

    let guessed_probe = match Probe::new(BufReader::new(file))
        .options(relaxed_options)
        .guess_file_type()
    {
        Ok(probe) => probe,
        Err(error) => {
            debug!(
                "Metadata read failed for {} while guessing file type from content: {}",
                path.display(),
                error
            );
            return None;
        }
    };

    match guessed_probe.read() {
        Ok(tagged_file) => {
            debug!(
                "Metadata read recovered via relaxed/content-based parsing for {}",
                path.display()
            );
            Some(tagged_file)
        }
        Err(error) => {
            debug!(
                "Metadata read failed for {} after relaxed/content-based fallback: {}",
                path.display(),
                error
            );
            None
        }
    }
}

fn read_common_track_metadata_with_lofty(path: &Path) -> Option<CommonTrackMetadata> {
    let tagged_file = read_tagged_file_for_metadata(path, false)?;
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
            .or_else(|| tag.get_string(ItemKey::ReleaseDate))
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

fn read_embedded_cover_art_with_lofty(path: &Path) -> Option<Vec<u8>> {
    let tagged_file = read_tagged_file_for_metadata(path, true)?;
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

fn open_symphonia_probe(path: &Path) -> Option<symphonia::core::probe::ProbeResult> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }

    symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()
}

fn set_if_empty(target: &mut String, value: &str) -> bool {
    let trimmed = value.trim();
    if target.is_empty() && !trimmed.is_empty() {
        *target = trimmed.to_string();
        true
    } else {
        false
    }
}

fn symphonia_value_to_string(value: &SymphoniaValue) -> String {
    value.to_string().trim().to_string()
}

fn apply_symphonia_tag(
    metadata: &mut CommonTrackMetadata,
    tag: &symphonia::core::meta::Tag,
) -> bool {
    let value = symphonia_value_to_string(&tag.value);
    if value.is_empty() {
        return false;
    }

    let mut updated = false;
    match tag.std_key {
        Some(StandardTagKey::TrackTitle) => updated |= set_if_empty(&mut metadata.title, &value),
        Some(StandardTagKey::Artist) => updated |= set_if_empty(&mut metadata.artist, &value),
        Some(StandardTagKey::Album) => updated |= set_if_empty(&mut metadata.album, &value),
        Some(StandardTagKey::AlbumArtist) => {
            updated |= set_if_empty(&mut metadata.album_artist, &value)
        }
        Some(StandardTagKey::Date)
        | Some(StandardTagKey::ReleaseDate)
        | Some(StandardTagKey::OriginalDate)
        | Some(StandardTagKey::TaggingDate) => updated |= set_if_empty(&mut metadata.date, &value),
        Some(StandardTagKey::Genre) => updated |= set_if_empty(&mut metadata.genre, &value),
        Some(StandardTagKey::TrackNumber) | Some(StandardTagKey::Part) => {
            updated |= set_if_empty(&mut metadata.track_number, &value)
        }
        _ => {}
    }

    if updated {
        return true;
    }

    match tag.key.trim().to_ascii_uppercase().as_str() {
        "TIT2" | "TITLE" => set_if_empty(&mut metadata.title, &value),
        "TPE1" | "ARTIST" => set_if_empty(&mut metadata.artist, &value),
        "TALB" | "ALBUM" => set_if_empty(&mut metadata.album, &value),
        "TPE2" | "ALBUMARTIST" | "ALBUM_ARTIST" | "ALBUM ARTIST" | "ALBUMARTISTS"
        | "ALBUM_ARTISTS" => set_if_empty(&mut metadata.album_artist, &value),
        "TDRC" | "TDRL" | "TDOR" | "DATE" | "RELEASEDATE" | "ORIGINALDATE" => {
            set_if_empty(&mut metadata.date, &value)
        }
        "TYER" | "YEAR" => set_if_empty(&mut metadata.year, &value),
        "TCON" | "GENRE" => set_if_empty(&mut metadata.genre, &value),
        "TRCK" | "TRACK" | "TRACKNUMBER" => set_if_empty(&mut metadata.track_number, &value),
        _ => false,
    }
}

fn apply_symphonia_revision(
    metadata: &mut CommonTrackMetadata,
    revision: &MetadataRevision,
) -> bool {
    let mut updated = false;
    for tag in revision.tags() {
        updated |= apply_symphonia_tag(metadata, tag);
    }
    updated
}

fn has_any_common_metadata(metadata: &CommonTrackMetadata) -> bool {
    !metadata.title.is_empty()
        || !metadata.artist.is_empty()
        || !metadata.album.is_empty()
        || !metadata.album_artist.is_empty()
        || !metadata.date.is_empty()
        || !metadata.year.is_empty()
        || !metadata.genre.is_empty()
        || !metadata.track_number.is_empty()
}

fn read_common_track_metadata_with_symphonia(path: &Path) -> Option<CommonTrackMetadata> {
    let mut probed = open_symphonia_probe(path)?;
    let mut metadata = CommonTrackMetadata::default();

    if let Some(probe_meta) = probed.metadata.get() {
        if let Some(revision) = probe_meta.current() {
            let _ = apply_symphonia_revision(&mut metadata, revision);
        }
    }

    while !probed.format.metadata().is_latest() {
        let _ = probed.format.metadata().pop();
    }
    if let Some(revision) = probed.format.metadata().current() {
        let _ = apply_symphonia_revision(&mut metadata, revision);
    }

    if metadata.year.is_empty() {
        metadata.year = derive_year_from_date(&metadata.date);
    }

    if has_any_common_metadata(&metadata) {
        Some(metadata)
    } else {
        None
    }
}

fn first_visual_data(revision: &MetadataRevision) -> Option<Vec<u8>> {
    revision
        .visuals()
        .iter()
        .find(|visual| {
            matches!(visual.usage, Some(StandardVisualKey::FrontCover)) && !visual.data.is_empty()
        })
        .or_else(|| {
            revision
                .visuals()
                .iter()
                .find(|visual| !visual.data.is_empty())
        })
        .map(|visual| visual.data.to_vec())
}

fn read_embedded_cover_art_with_symphonia(path: &Path) -> Option<Vec<u8>> {
    let mut probed = open_symphonia_probe(path)?;

    if let Some(probe_meta) = probed.metadata.get() {
        if let Some(revision) = probe_meta.current() {
            if let Some(cover_data) = first_visual_data(revision) {
                return Some(cover_data);
            }
        }
    }

    while !probed.format.metadata().is_latest() {
        let _ = probed.format.metadata().pop();
    }
    if let Some(revision) = probed.format.metadata().current() {
        return first_visual_data(revision);
    }

    None
}

/// Reads normalized common metadata values from a media file.
pub fn read_common_track_metadata(path: &Path) -> Option<CommonTrackMetadata> {
    if let Some(lofty_metadata) = read_common_track_metadata_with_lofty(path) {
        return Some(lofty_metadata);
    }

    let symphonia_metadata = read_common_track_metadata_with_symphonia(path);
    if symphonia_metadata.is_some() {
        debug!(
            "Metadata read recovered via symphonia fallback for {}",
            path.display()
        );
    } else {
        warn!(
            "Metadata read failed for {} in both lofty and symphonia paths",
            path.display()
        );
    }
    symphonia_metadata
}

/// Reads embedded cover-art bytes from a media file, if present.
pub fn read_embedded_cover_art(path: &Path) -> Option<Vec<u8>> {
    if let Some(lofty_cover) = read_embedded_cover_art_with_lofty(path) {
        return Some(lofty_cover);
    }

    let symphonia_cover = read_embedded_cover_art_with_symphonia(path);
    if symphonia_cover.is_some() {
        debug!(
            "Embedded cover-art read recovered via symphonia fallback for {}",
            path.display()
        );
    }
    symphonia_cover
}

#[cfg(test)]
mod tests {
    use super::derive_year_from_date;
    use super::read_common_track_metadata;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_mp3_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be valid")
            .as_nanos();
        std::env::temp_dir().join(format!("roqtune_{name}_{nonce}.mp3"))
    }

    fn write_mp3_with_large_junk_gap(path: &PathBuf, junk_bytes: usize) {
        let mut bytes = Vec::new();
        // ID3v2.3 header with payload size 0x23 (35 bytes)
        bytes.extend_from_slice(&[0x49, 0x44, 0x33, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x23]);
        // TALB frame content (UTF-16LE "aaaaaaaaaaa")
        bytes.extend_from_slice(&[
            0x54, 0x41, 0x4C, 0x42, 0x00, 0x00, 0x00, 0x19, 0x00, 0x00, 0x01, 0xFF, 0xFE, 0x61,
            0x00, 0x61, 0x00, 0x61, 0x00, 0x61, 0x00, 0x61, 0x00, 0x61, 0x00, 0x61, 0x00, 0x61,
            0x00, 0x61, 0x00, 0x61, 0x00, 0x61, 0x00,
        ]);
        bytes.extend(std::iter::repeat_n(0x20, junk_bytes));
        // Start of an MPEG frame (minimal bytes, enough for tag reader context)
        bytes.extend_from_slice(&[
            0xFF, 0xFB, 0x50, 0xC4, 0x00, 0x03, 0xC0, 0x00, 0x01, 0xA4, 0x00, 0x00, 0x00, 0x20,
            0x00, 0x00, 0x34, 0x80, 0x00, 0x00, 0x04,
        ]);

        fs::write(path, bytes).expect("should write mp3 fixture");
    }

    #[test]
    fn test_derive_year_from_date_with_full_value() {
        assert_eq!(derive_year_from_date("1998-10-31"), "1998");
    }

    #[test]
    fn test_derive_year_from_date_with_short_value() {
        assert_eq!(derive_year_from_date("99"), "");
    }

    #[test]
    fn test_derive_year_from_date_with_non_leading_year() {
        assert_eq!(derive_year_from_date("released 2003-04-01"), "2003");
    }

    #[test]
    fn test_read_common_track_metadata_with_large_junk_gap() {
        let path = unique_temp_mp3_path("large_junk_gap");
        write_mp3_with_large_junk_gap(&path, 4_096);

        let parsed = read_common_track_metadata(path.as_path())
            .expect("metadata should be readable with relaxed fallback parsing");
        assert!(
            !parsed.album.is_empty(),
            "album from TALB frame should be parsed"
        );

        fs::remove_file(path).expect("fixture should be removable");
    }
}
