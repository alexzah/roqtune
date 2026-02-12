//! UI adapter and presentation-state manager.
//!
//! This component bridges event-bus messages to Slint model updates, performs
//! metadata/cover-art lookup, and owns playlist table presentation behavior.

use std::hash::{Hash, Hasher};
use std::io::Read;
use std::time::{Duration, Instant};
use std::{
    cell::RefCell,
    collections::hash_map::DefaultHasher,
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, Receiver as StdReceiver, Sender as StdSender},
    thread,
};

use governor::state::NotKeyed;
use log::{debug, info, warn};
use slint::{Image, Model, ModelRc, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{
    config::{self, PlaylistColumnConfig},
    metadata_tags, protocol, AppWindow, LibraryRowData,
    MetadataEditorField as UiMetadataEditorField, TrackRowData,
};
use governor::{Quota, RateLimiter};

/// Shared UI models that are created in `main` and attached to the Slint window.
pub struct UiState {
    /// Track table model consumed directly by Slint.
    pub track_model: Rc<VecModel<TrackRowData>>,
}

/// Consumes bus messages and applies corresponding UI state updates.
pub struct UiManager {
    ui: slint::Weak<AppWindow>,
    bus_receiver: Receiver<protocol::Message>,
    bus_sender: Sender<protocol::Message>,
    library_scan_progress_rx: StdReceiver<protocol::LibraryMessage>,
    cover_art_lookup_tx: StdSender<CoverArtLookupRequest>,
    metadata_lookup_tx: StdSender<MetadataLookupRequest>,
    last_cover_art_lookup_path: Option<PathBuf>,
    active_playlist_id: String,
    playlist_ids: Vec<String>,
    playlist_names: Vec<String>,
    track_ids: Vec<String>,
    track_paths: Vec<PathBuf>,
    track_cover_art_paths: Vec<Option<PathBuf>>,
    track_cover_art_missing_tracks: HashSet<PathBuf>,
    track_metadata: Vec<TrackMetadata>,
    view_indices: Vec<usize>,
    selected_indices: Vec<usize>,
    selection_anchor_track_id: Option<String>,
    copied_track_paths: Vec<PathBuf>,
    active_playing_index: Option<usize>,
    drag_indices: Vec<usize>,
    is_dragging: bool,
    pressed_index: Option<usize>,
    progress_rl:
        RateLimiter<NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>,
    // Cached progress values to avoid unnecessary UI updates
    last_elapsed_ms: u64,
    last_total_ms: u64,
    current_playing_track_path: Option<PathBuf>,
    current_playing_track_metadata: Option<protocol::DetailedMetadata>,
    current_technical_metadata: Option<protocol::TechnicalMetadata>,
    current_output_path_info: Option<protocol::OutputPathInfo>,
    playlist_columns: Vec<PlaylistColumnConfig>,
    playlist_column_content_targets_px: Vec<u32>,
    playlist_column_target_widths_px: HashMap<String, u32>,
    playlist_column_widths_px: Vec<u32>,
    playlist_column_width_overrides_px: HashMap<String, u32>,
    playlist_columns_available_width_px: u32,
    playlist_columns_content_width_px: u32,
    playlist_row_height_px: u32,
    album_art_column_min_width_px: u32,
    album_art_column_max_width_px: u32,
    filter_sort_column_key: Option<String>,
    filter_sort_direction: Option<PlaylistSortDirection>,
    filter_search_query: String,
    filter_search_visible: bool,
    playback_active: bool,
    processed_message_count: u64,
    lagged_message_count: u64,
    last_message_at: Instant,
    last_progress_at: Option<Instant>,
    last_health_log_at: Instant,
    collection_mode: i32,
    library_view_stack: Vec<LibraryViewState>,
    library_entries: Vec<LibraryEntry>,
    library_view_indices: Vec<usize>,
    library_selected_indices: Vec<usize>,
    library_selection_anchor: Option<usize>,
    library_cover_art_paths: HashMap<PathBuf, Option<PathBuf>>,
    folder_cover_art_paths: HashMap<PathBuf, Option<PathBuf>>,
    library_enrichment:
        HashMap<protocol::LibraryEnrichmentEntity, protocol::LibraryEnrichmentPayload>,
    library_enrichment_pending: HashSet<protocol::LibraryEnrichmentEntity>,
    library_enrichment_last_request_at: HashMap<protocol::LibraryEnrichmentEntity, Instant>,
    library_enrichment_retry_counts: HashMap<protocol::LibraryEnrichmentEntity, u32>,
    library_enrichment_retry_not_before: HashMap<protocol::LibraryEnrichmentEntity, Instant>,
    library_last_detail_enrichment_entity: Option<protocol::LibraryEnrichmentEntity>,
    library_online_metadata_enabled: bool,
    library_online_metadata_prompt_pending: bool,
    library_artist_prefetch_first_row: usize,
    library_artist_prefetch_row_count: usize,
    library_scroll_positions: HashMap<LibraryScrollViewKey, usize>,
    pending_library_scroll_restore_row: Option<usize>,
    library_scroll_restore_token: i32,
    library_artist_row_indices: HashMap<String, Vec<usize>>,
    library_last_prefetch_entities: Vec<protocol::LibraryEnrichmentEntity>,
    library_last_background_entities: Vec<protocol::LibraryEnrichmentEntity>,
    library_search_query: String,
    library_page_request_id: u64,
    library_page_view: Option<protocol::LibraryViewQuery>,
    library_page_next_offset: usize,
    library_page_total: usize,
    library_page_entries: Vec<protocol::LibraryEntryPayload>,
    library_page_query: String,
    library_search_visible: bool,
    library_scan_in_progress: bool,
    library_status_text: String,
    library_add_to_playlist_checked: Vec<bool>,
    library_add_to_dialog_visible: bool,
    library_toast_generation: u64,
    pending_paste_feedback: bool,
    copied_library_selections: Vec<protocol::LibrarySelectionSpec>,
    properties_request_nonce: u64,
    properties_pending_request_id: Option<u64>,
    properties_pending_request_kind: Option<PropertiesRequestKind>,
    properties_target_path: Option<PathBuf>,
    properties_target_title: String,
    properties_original_fields: Vec<protocol::MetadataEditorField>,
    properties_fields: Vec<protocol::MetadataEditorField>,
    properties_dialog_visible: bool,
    properties_busy: bool,
    properties_error_text: String,
}

/// Normalized track metadata snapshot used for row rendering and side panel display.
#[derive(Clone)]
struct TrackMetadata {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    date: String,
    year: String,
    genre: String,
    track_number: String,
}

/// Width policy used by adaptive playlist column sizing.
#[derive(Clone, Copy, Debug)]
struct ColumnWidthProfile {
    min_px: u32,
    preferred_px: u32,
    max_px: u32,
}

/// Cover-art lookup request payload used by the internal worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverArtLookupRequest {
    track_path: Option<PathBuf>,
}

/// Deferred metadata-lookup request payload used by the internal worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataLookupRequest {
    track_id: String,
    track_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaylistSortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PropertiesRequestKind {
    Load,
    Save,
}

#[derive(Clone, Debug)]
enum LibraryViewState {
    SongsRoot,
    ArtistsRoot,
    AlbumsRoot,
    GenresRoot,
    DecadesRoot,
    GlobalSearch,
    ArtistDetail { artist: String },
    AlbumDetail { album: String, album_artist: String },
    GenreDetail { genre: String },
    DecadeDetail { decade: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LibraryScrollViewKey {
    SongsRoot,
    ArtistsRoot,
    AlbumsRoot,
    GenresRoot,
    DecadesRoot,
    GlobalSearch,
}

#[derive(Clone, Debug)]
enum LibraryEntry {
    Song(protocol::LibrarySong),
    Artist(protocol::LibraryArtist),
    Album(protocol::LibraryAlbum),
    Genre(protocol::LibraryGenre),
    Decade(protocol::LibraryDecade),
}

#[derive(Clone, Debug)]
struct LibraryRowPresentation {
    leading: String,
    primary: String,
    secondary: String,
    item_kind: i32,
    cover_art_path: Option<PathBuf>,
    is_playing: bool,
    selected: bool,
}

const PLAYLIST_COLUMN_KIND_TEXT: i32 = 0;
const PLAYLIST_COLUMN_KIND_ALBUM_ART: i32 = 1;
const BASE_ROW_HEIGHT_PX: u32 = 30;
const ALBUM_ART_ROW_PADDING_PX: u32 = 8;
const COLLECTION_MODE_PLAYLIST: i32 = 0;
const COLLECTION_MODE_LIBRARY: i32 = 1;
const LIBRARY_ITEM_KIND_SONG: i32 = 0;
const LIBRARY_ITEM_KIND_ARTIST: i32 = 1;
const LIBRARY_ITEM_KIND_ALBUM: i32 = 2;
const LIBRARY_ITEM_KIND_GENRE: i32 = 3;
const LIBRARY_ITEM_KIND_DECADE: i32 = 4;
const DETAIL_ENRICHMENT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const PREFETCH_RETRY_MAX_ATTEMPTS: u32 = 4;
const PREFETCH_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const LIBRARY_PREFETCH_FALLBACK_VISIBLE_ROWS: usize = 24;
const LIBRARY_PREFETCH_TOP_OVERSCAN_ROWS: usize = 2;
const LIBRARY_PREFETCH_BOTTOM_OVERSCAN_ROWS: usize = 10;
const LIBRARY_BACKGROUND_WARM_QUEUE_SIZE: usize = 6;
const COVER_ART_IMAGE_CACHE_MAX_BYTES: u64 = 128 * 1024 * 1024;
const COVER_ART_IMAGE_CACHE_MAX_ENTRIES: usize = 4096;
const COVER_ART_FAILED_PATHS_MAX_ENTRIES: usize = 4096;
const LIBRARY_PAGE_FETCH_LIMIT: usize = 512;

#[derive(Default)]
struct CoverArtImageCache {
    entries: HashMap<PathBuf, (Image, u64)>,
    lru: VecDeque<PathBuf>,
    total_bytes: u64,
}

impl CoverArtImageCache {
    fn touch_lru(&mut self, path: &PathBuf) {
        if let Some(position) = self.lru.iter().position(|candidate| candidate == path) {
            let _ = self.lru.remove(position);
        }
        self.lru.push_back(path.clone());
    }

    fn get(&mut self, path: &PathBuf) -> Option<Image> {
        let cached = self.entries.get(path).map(|(image, _)| image.clone())?;
        self.touch_lru(path);
        Some(cached)
    }

    fn insert(&mut self, path: PathBuf, image: Image, approx_bytes: u64) {
        if let Some((_, existing_bytes)) = self.entries.remove(&path) {
            self.total_bytes = self.total_bytes.saturating_sub(existing_bytes);
        }
        self.touch_lru(&path);
        self.total_bytes = self.total_bytes.saturating_add(approx_bytes);
        self.entries.insert(path, (image, approx_bytes));

        while self.total_bytes > COVER_ART_IMAGE_CACHE_MAX_BYTES
            || self.entries.len() > COVER_ART_IMAGE_CACHE_MAX_ENTRIES
        {
            let Some(evicted_path) = self.lru.pop_front() else {
                break;
            };
            if let Some((_, bytes)) = self.entries.remove(&evicted_path) {
                self.total_bytes = self.total_bytes.saturating_sub(bytes);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
        self.total_bytes = 0;
    }
}

#[derive(Default)]
struct FailedCoverPathCache {
    paths: HashSet<PathBuf>,
    order: VecDeque<PathBuf>,
}

impl FailedCoverPathCache {
    fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    fn insert(&mut self, path: PathBuf) {
        if self.paths.insert(path.clone()) {
            self.order.push_back(path);
            while self.paths.len() > COVER_ART_FAILED_PATHS_MAX_ENTRIES {
                let Some(evicted) = self.order.pop_front() else {
                    break;
                };
                self.paths.remove(&evicted);
            }
        }
    }

    fn clear(&mut self) {
        self.paths.clear();
        self.order.clear();
    }
}

thread_local! {
    static TRACK_ROW_COVER_ART_IMAGE_CACHE: RefCell<CoverArtImageCache> =
        RefCell::new(CoverArtImageCache::default());
    static TRACK_ROW_COVER_ART_FAILED_PATHS: RefCell<FailedCoverPathCache> =
        RefCell::new(FailedCoverPathCache::default());
}

fn fit_column_widths_to_available_space(
    widths: &mut [u32],
    min_widths: &[u32],
    _max_widths: &[u32],
    available_width_px: u32,
) {
    let mut used_width: u32 = widths.iter().copied().sum();
    if used_width > available_width_px {
        let mut deficit = used_width - available_width_px;
        while deficit > 0 {
            let adjustable_indices: Vec<usize> = widths
                .iter()
                .enumerate()
                .filter_map(|(index, width)| (*width > min_widths[index]).then_some(index))
                .collect();
            if adjustable_indices.is_empty() {
                break;
            }
            let step = (deficit / adjustable_indices.len() as u32).max(1);
            let mut reduced_this_round = 0u32;
            for index in adjustable_indices {
                let shrink_capacity = widths[index] - min_widths[index];
                if shrink_capacity == 0 {
                    continue;
                }
                let shrink_by = shrink_capacity.min(step).min(deficit);
                widths[index] -= shrink_by;
                deficit -= shrink_by;
                reduced_this_round += shrink_by;
                if deficit == 0 {
                    break;
                }
            }
            if reduced_this_round == 0 {
                break;
            }
        }
    }
    used_width = widths.iter().copied().sum();
    debug!(
        "Playlist column sizing fitted: available={} used={} columns={} (shrink-only)",
        available_width_px,
        used_width,
        widths.len()
    );
}

impl UiManager {
    fn roqtune_cache_root() -> Option<PathBuf> {
        dirs::cache_dir().map(|path| path.join("roqtune"))
    }

    fn covers_cache_dir() -> Option<PathBuf> {
        Self::roqtune_cache_root().map(|path| path.join("covers"))
    }

    fn enrichment_images_cache_dir() -> Option<PathBuf> {
        Self::roqtune_cache_root().map(|path| path.join("library_enrichment").join("images"))
    }

    fn is_managed_cached_image_path(path: &Path) -> bool {
        let in_cover_cache = Self::covers_cache_dir()
            .map(|cache_dir| path.starts_with(cache_dir))
            .unwrap_or(false);
        let in_enrichment_cache = Self::enrichment_images_cache_dir()
            .map(|cache_dir| path.starts_with(cache_dir))
            .unwrap_or(false);
        in_cover_cache || in_enrichment_cache
    }

    fn delete_cached_image_file_if_managed(path: &Path) {
        if !Self::is_managed_cached_image_path(path) {
            return;
        }
        if !path.exists() {
            return;
        }
        if let Err(error) = std::fs::remove_file(path) {
            debug!(
                "Failed removing invalid managed cache image {}: {}",
                path.display(),
                error
            );
        }
    }

    fn failed_cover_path(path: &Path) -> bool {
        TRACK_ROW_COVER_ART_FAILED_PATHS.with(|failed| failed.borrow().contains(path))
    }

    fn find_external_cover_art(track_path: &Path) -> Option<PathBuf> {
        let parent = track_path.parent()?;
        let names = ["cover", "front", "folder", "album", "art"];
        let extensions = ["jpg", "jpeg", "png", "webp"];

        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut found_files = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let file_stem_lower = file_stem.to_lowercase();
                if !names.iter().any(|&name| file_stem_lower == name) {
                    continue;
                }
                let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                    continue;
                };
                if extensions
                    .iter()
                    .any(|candidate| ext.eq_ignore_ascii_case(candidate))
                {
                    found_files.push(path);
                }
            }
            found_files.sort();
            if let Some(found) = found_files.into_iter().next() {
                return Some(found);
            }
        }

        None
    }

    fn embedded_art_cache_stem(track_path: &Path) -> Option<String> {
        let mut hasher = DefaultHasher::new();
        track_path.hash(&mut hasher);
        let hash = hasher.finish();
        Some(format!("{hash:x}"))
    }

    fn detect_image_extension(bytes: &[u8]) -> Option<&'static str> {
        if bytes.len() >= 8
            && bytes[0] == 0x89
            && bytes[1] == b'P'
            && bytes[2] == b'N'
            && bytes[3] == b'G'
            && bytes[4] == 0x0D
            && bytes[5] == 0x0A
            && bytes[6] == 0x1A
            && bytes[7] == 0x0A
        {
            return Some("png");
        }
        if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
            return Some("jpg");
        }
        if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
            return Some("webp");
        }
        if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
            return Some("gif");
        }
        if bytes.len() >= 2 && bytes[0] == b'B' && bytes[1] == b'M' {
            return Some("bmp");
        }
        None
    }

    fn is_valid_image_file(path: &Path) -> bool {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return false,
        };
        let mut header = [0u8; 16];
        let read = match file.read(&mut header) {
            Ok(read) => read,
            Err(_) => return false,
        };
        if read == 0 {
            return false;
        }
        Self::detect_image_extension(&header[..read]).is_some()
    }

    fn embedded_art_cache_candidates(track_path: &Path) -> Option<Vec<PathBuf>> {
        let cache_dir = Self::covers_cache_dir()?;
        let stem = Self::embedded_art_cache_stem(track_path)?;
        Some(
            ["png", "jpg", "jpeg", "webp", "gif", "bmp"]
                .iter()
                .map(|ext| cache_dir.join(format!("{stem}.{ext}")))
                .collect(),
        )
    }

    fn embedded_art_cache_path_if_present(track_path: &Path) -> Option<PathBuf> {
        let candidates = Self::embedded_art_cache_candidates(track_path)?;
        for cache_path in candidates {
            if Self::failed_cover_path(&cache_path) {
                continue;
            }
            if !cache_path.exists() {
                continue;
            }
            if !Self::is_valid_image_file(&cache_path) {
                Self::delete_cached_image_file_if_managed(&cache_path);
                continue;
            }
            return Some(cache_path);
        }
        None
    }

    fn find_external_cover_art_cached(&mut self, track_path: &Path) -> Option<PathBuf> {
        let parent = track_path.parent()?.to_path_buf();
        if let Some(cached) = self.folder_cover_art_paths.get(&parent) {
            return cached.clone();
        }

        let resolved = Self::find_external_cover_art(track_path);
        self.folder_cover_art_paths.insert(parent, resolved.clone());
        resolved
    }

    fn library_track_number_leading(track_number: &str) -> String {
        let trimmed = track_number.trim();
        if trimmed.is_empty() {
            return String::new();
        }

        let first_component = trimmed.split('/').next().map(str::trim).unwrap_or(trimmed);
        if first_component.is_empty() {
            return String::new();
        }

        if let Ok(parsed) = first_component.parse::<u32>() {
            return format!("{parsed:02}");
        }

        first_component.to_string()
    }

    fn parse_library_year(raw_year: &str) -> Option<i32> {
        let mut digits = String::with_capacity(4);
        for ch in raw_year.trim().chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                if digits.len() == 4 {
                    return digits.parse::<i32>().ok();
                }
            } else {
                digits.clear();
            }
        }
        None
    }

    fn build_artist_detail_entries(
        albums: Vec<protocol::LibraryAlbum>,
        songs: Vec<protocol::LibrarySong>,
    ) -> Vec<LibraryEntry> {
        let mut songs_by_album: HashMap<(String, String), Vec<protocol::LibrarySong>> =
            HashMap::new();
        let mut album_year_by_key: HashMap<(String, String), i32> = HashMap::new();

        for song in songs {
            let key = (song.album.clone(), song.album_artist.clone());
            if let Some(year) = Self::parse_library_year(&song.year) {
                album_year_by_key
                    .entry(key.clone())
                    .and_modify(|current_year| *current_year = (*current_year).max(year))
                    .or_insert(year);
            }
            songs_by_album.entry(key).or_default().push(song);
        }

        let mut ordered_albums: Vec<((String, String), protocol::LibraryAlbum, Option<i32>)> =
            Vec::new();
        let mut seen_album_keys: HashSet<(String, String)> = HashSet::new();

        for album in albums {
            let key = (album.album.clone(), album.album_artist.clone());
            if !seen_album_keys.insert(key.clone()) {
                continue;
            }
            let year = album_year_by_key.get(&key).copied();
            ordered_albums.push((key, album, year));
        }

        for (key, album_songs) in &songs_by_album {
            if seen_album_keys.contains(key) {
                continue;
            }
            let synthetic_album = protocol::LibraryAlbum {
                album: key.0.clone(),
                album_artist: key.1.clone(),
                song_count: album_songs.len() as u32,
                representative_track_path: album_songs.first().map(|song| song.path.clone()),
            };
            let year = album_year_by_key.get(key).copied();
            ordered_albums.push((key.clone(), synthetic_album, year));
        }

        ordered_albums.sort_by(
            |(left_key, left_album, left_year), (right_key, right_album, right_year)| {
                right_year
                    .cmp(left_year)
                    .then_with(|| {
                        left_album
                            .album
                            .to_ascii_lowercase()
                            .cmp(&right_album.album.to_ascii_lowercase())
                    })
                    .then_with(|| {
                        left_album
                            .album_artist
                            .to_ascii_lowercase()
                            .cmp(&right_album.album_artist.to_ascii_lowercase())
                    })
                    .then_with(|| left_key.cmp(right_key))
            },
        );

        let mut entries = Vec::new();
        for (key, album, _) in ordered_albums {
            entries.push(LibraryEntry::Album(album));
            if let Some(album_songs) = songs_by_album.remove(&key) {
                entries.extend(album_songs.into_iter().map(LibraryEntry::Song));
            }
        }

        if !songs_by_album.is_empty() {
            let mut remaining_songs: Vec<protocol::LibrarySong> =
                songs_by_album.into_values().flatten().collect();
            remaining_songs.sort_by(|left, right| {
                left.album
                    .to_ascii_lowercase()
                    .cmp(&right.album.to_ascii_lowercase())
                    .then_with(|| {
                        left.album_artist
                            .to_ascii_lowercase()
                            .cmp(&right.album_artist.to_ascii_lowercase())
                    })
                    .then_with(|| left.path.cmp(&right.path))
            });
            entries.extend(remaining_songs.into_iter().map(LibraryEntry::Song));
        }

        entries
    }

    fn build_global_search_entries(
        songs: Vec<protocol::LibrarySong>,
        artists: Vec<protocol::LibraryArtist>,
        albums: Vec<protocol::LibraryAlbum>,
    ) -> Vec<LibraryEntry> {
        let mut entries = Vec::with_capacity(songs.len() + artists.len() + albums.len());
        entries.extend(songs.into_iter().map(LibraryEntry::Song));
        entries.extend(artists.into_iter().map(LibraryEntry::Artist));
        entries.extend(albums.into_iter().map(LibraryEntry::Album));
        entries.sort_by(|left, right| {
            let left_key = match left {
                LibraryEntry::Song(song) => song.title.to_ascii_lowercase(),
                LibraryEntry::Artist(artist) => artist.artist.to_ascii_lowercase(),
                LibraryEntry::Album(album) => album.album.to_ascii_lowercase(),
                LibraryEntry::Genre(genre) => genre.genre.to_ascii_lowercase(),
                LibraryEntry::Decade(decade) => decade.decade.to_ascii_lowercase(),
            };
            let right_key = match right {
                LibraryEntry::Song(song) => song.title.to_ascii_lowercase(),
                LibraryEntry::Artist(artist) => artist.artist.to_ascii_lowercase(),
                LibraryEntry::Album(album) => album.album.to_ascii_lowercase(),
                LibraryEntry::Genre(genre) => genre.genre.to_ascii_lowercase(),
                LibraryEntry::Decade(decade) => decade.decade.to_ascii_lowercase(),
            };
            let left_kind_rank = match left {
                LibraryEntry::Artist(_) => 0,
                LibraryEntry::Album(_) => 1,
                LibraryEntry::Song(_) => 2,
                LibraryEntry::Genre(_) => 3,
                LibraryEntry::Decade(_) => 4,
            };
            let right_kind_rank = match right {
                LibraryEntry::Artist(_) => 0,
                LibraryEntry::Album(_) => 1,
                LibraryEntry::Song(_) => 2,
                LibraryEntry::Genre(_) => 3,
                LibraryEntry::Decade(_) => 4,
            };
            left_key
                .cmp(&right_key)
                .then_with(|| left_kind_rank.cmp(&right_kind_rank))
        });
        entries
    }

    fn coalesce_cover_art_requests(
        mut latest: CoverArtLookupRequest,
        request_rx: &StdReceiver<CoverArtLookupRequest>,
    ) -> CoverArtLookupRequest {
        while let Ok(next) = request_rx.try_recv() {
            latest = next;
        }
        latest
    }

    fn drain_metadata_lookup_requests(
        first: MetadataLookupRequest,
        request_rx: &StdReceiver<MetadataLookupRequest>,
    ) -> Vec<MetadataLookupRequest> {
        let mut pending = vec![first];
        while let Ok(next) = request_rx.try_recv() {
            pending.push(next);
            if pending.len() >= 256 {
                break;
            }
        }
        pending
    }

    /// Creates a UI manager and starts an internal cover-art lookup worker thread.
    pub fn new(
        ui: slint::Weak<AppWindow>,
        bus_receiver: Receiver<protocol::Message>,
        bus_sender: Sender<protocol::Message>,
        initial_online_metadata_enabled: bool,
        initial_online_metadata_prompt_pending: bool,
        library_scan_progress_rx: StdReceiver<protocol::LibraryMessage>,
    ) -> Self {
        let (cover_art_lookup_tx, cover_art_lookup_rx) = mpsc::channel::<CoverArtLookupRequest>();
        let cover_art_bus_sender = bus_sender.clone();
        thread::spawn(move || {
            while let Ok(request) = cover_art_lookup_rx.recv() {
                let latest_request =
                    UiManager::coalesce_cover_art_requests(request, &cover_art_lookup_rx);
                let cover_art_path = latest_request
                    .track_path
                    .as_ref()
                    .and_then(|path| UiManager::find_cover_art(path.as_path()));
                let _ = cover_art_bus_sender.send(protocol::Message::Playback(
                    protocol::PlaybackMessage::CoverArtChanged(cover_art_path),
                ));
            }
        });
        let (metadata_lookup_tx, metadata_lookup_rx) = mpsc::channel::<MetadataLookupRequest>();
        let metadata_bus_sender = bus_sender.clone();
        thread::spawn(move || {
            while let Ok(request) = metadata_lookup_rx.recv() {
                let pending =
                    UiManager::drain_metadata_lookup_requests(request, &metadata_lookup_rx);
                let mut latest_by_track_id: HashMap<String, PathBuf> = HashMap::new();
                for item in pending {
                    latest_by_track_id.insert(item.track_id, item.track_path);
                }
                let mut updates = Vec::with_capacity(latest_by_track_id.len());
                for (track_id, track_path) in latest_by_track_id {
                    updates.push(protocol::TrackMetadataPatch {
                        track_id,
                        summary: UiManager::read_track_metadata_summary(track_path.as_path()),
                    });
                }
                if updates.is_empty() {
                    continue;
                }
                let _ = metadata_bus_sender.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::TrackMetadataBatchUpdated { updates },
                ));
            }
        });

        let enrichment_prefetch_tick_sender = bus_sender.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(240));
            if enrichment_prefetch_tick_sender
                .send(protocol::Message::Library(
                    protocol::LibraryMessage::EnrichmentPrefetchTick,
                ))
                .is_err()
            {
                break;
            }
        });

        Self {
            ui: ui.clone(),
            bus_receiver,
            bus_sender,
            library_scan_progress_rx,
            cover_art_lookup_tx,
            metadata_lookup_tx,
            last_cover_art_lookup_path: None,
            active_playlist_id: String::new(),
            playlist_ids: Vec::new(),
            playlist_names: Vec::new(),
            track_ids: Vec::new(),
            track_paths: Vec::new(),
            track_cover_art_paths: Vec::new(),
            track_cover_art_missing_tracks: HashSet::new(),
            track_metadata: Vec::new(),
            view_indices: Vec::new(),
            selected_indices: Vec::new(),
            selection_anchor_track_id: None,
            copied_track_paths: Vec::new(),
            active_playing_index: None,
            drag_indices: Vec::new(),
            is_dragging: false,
            pressed_index: None,
            progress_rl: RateLimiter::direct(
                Quota::with_period(Duration::from_millis(30)).unwrap(),
            ),
            last_elapsed_ms: 0,
            last_total_ms: 0,
            current_playing_track_path: None,
            current_playing_track_metadata: None,
            current_technical_metadata: None,
            current_output_path_info: None,
            playlist_columns: config::default_playlist_columns(),
            playlist_column_content_targets_px: Vec::new(),
            playlist_column_target_widths_px: HashMap::new(),
            playlist_column_widths_px: Vec::new(),
            playlist_column_width_overrides_px: HashMap::new(),
            playlist_columns_available_width_px: 0,
            playlist_columns_content_width_px: 0,
            playlist_row_height_px: BASE_ROW_HEIGHT_PX,
            album_art_column_min_width_px: config::default_playlist_album_art_column_min_width_px(),
            album_art_column_max_width_px: config::default_playlist_album_art_column_max_width_px(),
            filter_sort_column_key: None,
            filter_sort_direction: None,
            filter_search_query: String::new(),
            filter_search_visible: false,
            playback_active: false,
            processed_message_count: 0,
            lagged_message_count: 0,
            last_message_at: Instant::now(),
            last_progress_at: None,
            last_health_log_at: Instant::now(),
            collection_mode: COLLECTION_MODE_PLAYLIST,
            library_view_stack: vec![LibraryViewState::SongsRoot],
            library_entries: Vec::new(),
            library_view_indices: Vec::new(),
            library_selected_indices: Vec::new(),
            library_selection_anchor: None,
            library_cover_art_paths: HashMap::new(),
            folder_cover_art_paths: HashMap::new(),
            library_enrichment: HashMap::new(),
            library_enrichment_pending: HashSet::new(),
            library_enrichment_last_request_at: HashMap::new(),
            library_enrichment_retry_counts: HashMap::new(),
            library_enrichment_retry_not_before: HashMap::new(),
            library_last_detail_enrichment_entity: None,
            library_online_metadata_enabled: initial_online_metadata_enabled,
            library_online_metadata_prompt_pending: initial_online_metadata_prompt_pending,
            library_artist_prefetch_first_row: 0,
            library_artist_prefetch_row_count: 0,
            library_scroll_positions: HashMap::new(),
            pending_library_scroll_restore_row: None,
            library_scroll_restore_token: 0,
            library_artist_row_indices: HashMap::new(),
            library_last_prefetch_entities: Vec::new(),
            library_last_background_entities: Vec::new(),
            library_search_query: String::new(),
            library_page_request_id: 0,
            library_page_view: None,
            library_page_next_offset: 0,
            library_page_total: 0,
            library_page_entries: Vec::new(),
            library_page_query: String::new(),
            library_search_visible: false,
            library_scan_in_progress: false,
            library_status_text: String::new(),
            library_add_to_playlist_checked: Vec::new(),
            library_add_to_dialog_visible: false,
            library_toast_generation: 0,
            pending_paste_feedback: false,
            copied_library_selections: Vec::new(),
            properties_request_nonce: 0,
            properties_pending_request_id: None,
            properties_pending_request_kind: None,
            properties_target_path: None,
            properties_target_title: String::new(),
            properties_original_fields: Vec::new(),
            properties_fields: Vec::new(),
            properties_dialog_visible: false,
            properties_busy: false,
            properties_error_text: String::new(),
        }
    }

    fn on_message_received(&mut self) {
        let now = Instant::now();
        self.processed_message_count = self.processed_message_count.saturating_add(1);
        self.log_health_if_due(now);
        self.last_message_at = now;
    }

    fn on_message_lagged(&mut self, skipped: u64) {
        self.lagged_message_count = self.lagged_message_count.saturating_add(skipped.max(1));
        let now = Instant::now();
        if now.duration_since(self.last_health_log_at) >= Duration::from_secs(5) {
            warn!(
                "UiManager lagged on control bus, skipped={} total_lagged={} processed={}",
                skipped, self.lagged_message_count, self.processed_message_count
            );
            self.last_health_log_at = now;
        }
    }

    fn log_health_if_due(&mut self, now: Instant) {
        if now.duration_since(self.last_health_log_at) < Duration::from_secs(5) {
            return;
        }

        let since_last_message_ms = now.duration_since(self.last_message_at).as_millis();
        let since_last_progress_ms = self
            .last_progress_at
            .map(|last| now.duration_since(last).as_millis() as u64);
        info!(
            "UiManager health: processed={}, lagged={}, playback_active={}, since_last_message_ms={}, since_last_progress_ms={:?}",
            self.processed_message_count,
            self.lagged_message_count,
            self.playback_active,
            since_last_message_ms,
            since_last_progress_ms
        );

        if self.playback_active {
            if let Some(last_progress_at) = self.last_progress_at {
                if now.duration_since(last_progress_at) > Duration::from_secs(2) {
                    warn!(
                        "UiManager: playback is active but no progress message for {}ms",
                        now.duration_since(last_progress_at).as_millis()
                    );
                }
            } else {
                warn!("UiManager: playback is active but no progress message received yet");
            }
        }

        self.last_health_log_at = now;
    }

    fn render_technical_info_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(meta) = self.current_technical_metadata.as_ref() {
            parts.push(format!(
                "{} | {} kbps | {} Hz | {} ch",
                meta.format, meta.bitrate_kbps, meta.sample_rate_hz, meta.channel_count
            ));
        }
        if let Some(path_info) = self.current_output_path_info.as_ref() {
            let output_format = match path_info.output_stream.sample_format {
                protocol::OutputSampleFormat::F32 => "f32",
                protocol::OutputSampleFormat::I16 => "i16",
                protocol::OutputSampleFormat::U16 => "u16",
                protocol::OutputSampleFormat::Unknown => "unknown",
            };
            let mut transforms = Vec::new();
            if path_info.resampled {
                transforms.push("resample");
            }
            if path_info.remixed_channels {
                transforms.push("channel-map");
            }
            if path_info.dithered {
                transforms.push("dither");
            }
            let transform_text = if transforms.is_empty() {
                "direct".to_string()
            } else {
                transforms.join("+")
            };
            parts.push(format!(
                "Path: {} Hz/{} ch -> {} Hz/{} ch | {} bit {} ({})",
                path_info.source_sample_rate_hz,
                path_info.source_channel_count,
                path_info.output_stream.sample_rate_hz,
                path_info.output_stream.channel_count,
                path_info.output_stream.bits_per_sample,
                output_format,
                transform_text
            ));
        }
        parts.join(" | ")
    }

    fn refresh_technical_info_ui(&self) {
        let info_text = self.render_technical_info_text();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_technical_info(info_text.into());
        });
    }

    fn find_cover_art(track_path: &Path) -> Option<PathBuf> {
        Self::find_external_cover_art(track_path).or_else(|| Self::extract_embedded_art(track_path))
    }

    fn extract_embedded_art(track_path: &Path) -> Option<PathBuf> {
        let cache_dir = Self::covers_cache_dir()?;
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).ok()?;
        }

        if let Some(existing_path) = Self::embedded_art_cache_path_if_present(track_path) {
            return Some(existing_path);
        }

        let cover_bytes = metadata_tags::read_embedded_cover_art(track_path)?;
        let extension = Self::detect_image_extension(&cover_bytes)?;
        let stem = Self::embedded_art_cache_stem(track_path)?;
        let cache_path = cache_dir.join(format!("{stem}.{extension}"));
        let temp_path = cache_dir.join(format!("{stem}.{extension}.tmp"));

        if Self::failed_cover_path(&cache_path) {
            return None;
        }
        if std::fs::write(&temp_path, &cover_bytes).is_err() {
            return None;
        }
        if !Self::is_valid_image_file(&temp_path) {
            let _ = std::fs::remove_file(&temp_path);
            return None;
        }
        if std::fs::rename(&temp_path, &cache_path).is_err() {
            let _ = std::fs::remove_file(&temp_path);
            return None;
        }

        Some(cache_path)
    }

    fn update_cover_art(&mut self, track_path: Option<&PathBuf>) {
        let requested_track_path = track_path.cloned();
        if self.last_cover_art_lookup_path == requested_track_path {
            return;
        }
        self.last_cover_art_lookup_path = requested_track_path.clone();
        let _ = self.cover_art_lookup_tx.send(CoverArtLookupRequest {
            track_path: requested_track_path,
        });
    }

    fn is_album_art_column_visible(&self) -> bool {
        self.playlist_columns
            .iter()
            .any(|column| column.enabled && Self::is_album_art_builtin_column(column))
    }

    fn ensure_track_cover_art_slots(&mut self) {
        if self.track_cover_art_paths.len() < self.track_paths.len() {
            self.track_cover_art_paths
                .resize(self.track_paths.len(), None);
        } else if self.track_cover_art_paths.len() > self.track_paths.len() {
            self.track_cover_art_paths.truncate(self.track_paths.len());
        }
    }

    fn resolve_track_cover_art_path(&mut self, source_index: usize) -> Option<PathBuf> {
        let track_path = self.track_paths.get(source_index).cloned()?;
        if let Some(Some(existing_path)) = self.track_cover_art_paths.get(source_index) {
            if Self::failed_cover_path(existing_path) {
                if let Some(cache_slot) = self.track_cover_art_paths.get_mut(source_index) {
                    *cache_slot = None;
                }
                self.track_cover_art_missing_tracks
                    .insert(track_path.clone());
                return None;
            }
            return Some(existing_path.clone());
        }
        if let Some(cached_embedded) = Self::embedded_art_cache_path_if_present(&track_path) {
            if let Some(cache_slot) = self.track_cover_art_paths.get_mut(source_index) {
                *cache_slot = Some(cached_embedded.clone());
            }
            self.track_cover_art_missing_tracks.remove(&track_path);
            return Some(cached_embedded);
        }
        if self.track_cover_art_missing_tracks.contains(&track_path) {
            return None;
        }

        let resolved_path = self
            .find_external_cover_art_cached(&track_path)
            .or_else(|| Self::embedded_art_cache_path_if_present(&track_path));
        if let Some(cache_slot) = self.track_cover_art_paths.get_mut(source_index) {
            *cache_slot = resolved_path.clone();
        }
        if resolved_path.is_none() {
            self.track_cover_art_missing_tracks.insert(track_path);
        }
        resolved_path
    }

    fn row_cover_art_path(&mut self, source_index: usize) -> Option<PathBuf> {
        self.resolve_track_cover_art_path(source_index)
    }

    fn approximate_cover_art_size_bytes(path: &Path) -> u64 {
        std::fs::metadata(path)
            .map(|meta| meta.len().max(1))
            .unwrap_or(256 * 1024)
    }

    fn try_load_cover_art_image(path: &Path) -> Option<Image> {
        if Self::failed_cover_path(path) {
            return None;
        }

        let image_path = path.to_path_buf();
        if let Some(cached) =
            TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| cache.borrow_mut().get(&image_path))
        {
            return Some(cached);
        }

        match Image::load_from_path(path) {
            Ok(image) => {
                let approx_bytes = Self::approximate_cover_art_size_bytes(path);
                TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| {
                    cache
                        .borrow_mut()
                        .insert(image_path.clone(), image.clone(), approx_bytes);
                });
                Some(image)
            }
            Err(error) => {
                debug!(
                    "Failed loading cover art image {}: {}",
                    path.display(),
                    error
                );
                TRACK_ROW_COVER_ART_FAILED_PATHS.with(|failed| {
                    failed.borrow_mut().insert(image_path);
                });
                Self::delete_cached_image_file_if_managed(path);
                None
            }
        }
    }

    fn load_track_row_cover_art_image(path: Option<&PathBuf>) -> Image {
        path.and_then(|path| Self::try_load_cover_art_image(path))
            .unwrap_or_default()
    }

    fn to_detailed_metadata(track_metadata: &TrackMetadata) -> protocol::DetailedMetadata {
        protocol::DetailedMetadata {
            title: track_metadata.title.clone(),
            artist: track_metadata.artist.clone(),
            album: track_metadata.album.clone(),
            date: track_metadata.date.clone(),
            genre: track_metadata.genre.clone(),
        }
    }

    fn normalize_metadata_key(key: &str) -> String {
        let mut normalized = String::new();
        for ch in key.chars() {
            if ch.is_ascii_alphanumeric() {
                normalized.push(ch.to_ascii_lowercase());
            } else if ch == '_' || ch == '-' || ch.is_whitespace() {
                normalized.push('_');
            }
        }
        normalized
    }

    fn metadata_value(track_metadata: &TrackMetadata, key: &str) -> String {
        let normalized = Self::normalize_metadata_key(key);
        match normalized.as_str() {
            "title" => track_metadata.title.clone(),
            "artist" => track_metadata.artist.clone(),
            "album" => track_metadata.album.clone(),
            "album_artist" | "albumartist" => track_metadata.album_artist.clone(),
            "date" => track_metadata.date.clone(),
            "year" => {
                if !track_metadata.year.is_empty() {
                    track_metadata.year.clone()
                } else if track_metadata.date.len() >= 4 {
                    track_metadata.date[0..4].to_string()
                } else {
                    String::new()
                }
            }
            "genre" => track_metadata.genre.clone(),
            "track" | "track_number" | "tracknumber" => track_metadata.track_number.clone(),
            _ => String::new(),
        }
    }

    fn render_column_value(track_metadata: &TrackMetadata, format_string: &str) -> String {
        let mut rendered = String::new();
        let mut chars = format_string.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '{' {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    rendered.push('{');
                    continue;
                }

                let mut token = String::new();
                let mut found_closing = false;
                for token_ch in chars.by_ref() {
                    if token_ch == '}' {
                        found_closing = true;
                        break;
                    }
                    token.push(token_ch);
                }

                if found_closing {
                    let value = Self::metadata_value(track_metadata, token.trim());
                    rendered.push_str(&value);
                } else {
                    rendered.push('{');
                    rendered.push_str(&token);
                }
            } else if ch == '}' {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                rendered.push('}');
            } else {
                rendered.push(ch);
            }
        }

        rendered
    }

    fn build_playlist_row_values(
        track_metadata: &TrackMetadata,
        playlist_columns: &[PlaylistColumnConfig],
    ) -> Vec<String> {
        playlist_columns
            .iter()
            .filter(|column| column.enabled)
            .map(|column| {
                if Self::is_album_art_builtin_column(column) {
                    String::new()
                } else {
                    Self::render_column_value(track_metadata, &column.format)
                }
            })
            .collect()
    }

    fn visible_playlist_columns(&self) -> Vec<&PlaylistColumnConfig> {
        self.playlist_columns
            .iter()
            .filter(|column| column.enabled)
            .collect()
    }

    fn normalize_column_format(format: &str) -> String {
        format.trim().to_ascii_lowercase()
    }

    fn is_album_art_builtin_column(column: &PlaylistColumnConfig) -> bool {
        !column.custom && Self::normalize_column_format(&column.format) == "{album_art}"
    }

    fn playlist_column_kind(column: &PlaylistColumnConfig) -> i32 {
        if Self::is_album_art_builtin_column(column) {
            PLAYLIST_COLUMN_KIND_ALBUM_ART
        } else {
            PLAYLIST_COLUMN_KIND_TEXT
        }
    }

    fn visible_playlist_column_kinds(columns: &[PlaylistColumnConfig]) -> Vec<i32> {
        columns
            .iter()
            .filter(|column| column.enabled)
            .map(Self::playlist_column_kind)
            .collect()
    }

    fn playlist_column_key(column: &PlaylistColumnConfig) -> String {
        if column.custom {
            format!("custom:{}|{}", column.name.trim(), column.format.trim())
        } else {
            Self::normalize_column_format(&column.format)
        }
    }

    fn clamp_column_override_width_px(&self, column_key: &str, width_px: u32) -> u32 {
        if let Some(column) = self
            .playlist_columns
            .iter()
            .find(|column| Self::playlist_column_key(column) == column_key)
        {
            let profile = self.column_width_profile_for_column(column);
            return width_px.clamp(profile.min_px, profile.max_px);
        }
        width_px.max(1)
    }

    fn is_sortable_playlist_column(column: &PlaylistColumnConfig) -> bool {
        !Self::is_album_art_builtin_column(column)
    }

    fn album_art_column_width_profile(&self) -> ColumnWidthProfile {
        let min_px = self.album_art_column_min_width_px.clamp(12, 512);
        let max_px = self
            .album_art_column_max_width_px
            .clamp(min_px.max(24), 1024);

        ColumnWidthProfile {
            min_px,
            preferred_px: 72u32.clamp(min_px, max_px),
            max_px,
        }
    }

    fn column_width_profile_for_column(&self, column: &PlaylistColumnConfig) -> ColumnWidthProfile {
        if Self::is_album_art_builtin_column(column) {
            return self.album_art_column_width_profile();
        }

        let normalized_format = column.format.trim().to_ascii_lowercase();
        let normalized_name = column.name.trim().to_ascii_lowercase();

        if normalized_format == "{track}"
            || normalized_format == "{track_number}"
            || normalized_format == "{tracknumber}"
            || normalized_name == "track #"
            || normalized_name == "track"
        {
            return ColumnWidthProfile {
                min_px: 52,
                preferred_px: 68,
                max_px: 90,
            };
        }

        if normalized_format == "{disc}" || normalized_format == "{disc_number}" {
            return ColumnWidthProfile {
                min_px: 50,
                preferred_px: 64,
                max_px: 84,
            };
        }

        if normalized_format == "{year}"
            || normalized_format == "{date}"
            || normalized_name == "year"
        {
            return ColumnWidthProfile {
                min_px: 64,
                preferred_px: 78,
                max_px: 104,
            };
        }

        if normalized_format == "{title}" || normalized_name == "title" {
            return ColumnWidthProfile {
                min_px: 140,
                preferred_px: 230,
                max_px: 440,
            };
        }

        if normalized_format == "{artist}"
            || normalized_format == "{album_artist}"
            || normalized_format == "{albumartist}"
            || normalized_name == "artist"
            || normalized_name == "album artist"
        {
            return ColumnWidthProfile {
                min_px: 120,
                preferred_px: 190,
                max_px: 320,
            };
        }

        if normalized_format == "{album}" || normalized_name == "album" {
            return ColumnWidthProfile {
                min_px: 140,
                preferred_px: 220,
                max_px: 360,
            };
        }

        if normalized_format == "{genre}" || normalized_name == "genre" {
            return ColumnWidthProfile {
                min_px: 100,
                preferred_px: 140,
                max_px: 210,
            };
        }

        if normalized_name == "duration"
            || normalized_name == "time"
            || normalized_format == "{duration}"
        {
            return ColumnWidthProfile {
                min_px: 78,
                preferred_px: 92,
                max_px: 120,
            };
        }

        if column.custom {
            return ColumnWidthProfile {
                min_px: 110,
                preferred_px: 180,
                max_px: 340,
            };
        }

        ColumnWidthProfile {
            min_px: 100,
            preferred_px: 170,
            max_px: 300,
        }
    }

    fn refresh_playlist_column_content_targets(&mut self) {
        const SAMPLE_LIMIT: usize = 256;
        const MAX_MEASURED_CHARS: usize = 80;
        const CHAR_WIDTH_PX: u32 = 7;
        const CONTENT_PADDING_PX: u32 = 24;

        let visible_columns = self.visible_playlist_columns();
        let total_rows = self.track_metadata.len();
        let stride = if total_rows > SAMPLE_LIMIT {
            ((total_rows as f32) / (SAMPLE_LIMIT as f32)).ceil() as usize
        } else {
            1
        };

        let mut targets = Vec::with_capacity(visible_columns.len());
        for column in visible_columns {
            let profile = self.column_width_profile_for_column(column);
            if Self::is_album_art_builtin_column(column) {
                targets.push(profile.preferred_px.clamp(profile.min_px, profile.max_px));
                continue;
            }
            let mut char_width_samples = Vec::new();

            let header_chars = column.name.chars().take(MAX_MEASURED_CHARS).count() as u32;
            char_width_samples.push(header_chars);

            if total_rows > 0 {
                for metadata in self
                    .track_metadata
                    .iter()
                    .step_by(stride)
                    .take(SAMPLE_LIMIT)
                {
                    let rendered_value = Self::render_column_value(metadata, &column.format);
                    let measured_chars =
                        rendered_value.chars().take(MAX_MEASURED_CHARS).count() as u32;
                    char_width_samples.push(measured_chars);
                }
            }

            char_width_samples.sort_unstable();
            let percentile_index = ((char_width_samples.len() - 1) * 9) / 10;
            let percentile_chars = char_width_samples
                .get(percentile_index)
                .copied()
                .unwrap_or(0);
            let measured_width_px = percentile_chars
                .saturating_mul(CHAR_WIDTH_PX)
                .saturating_add(CONTENT_PADDING_PX);
            let preferred = if total_rows == 0 {
                profile.preferred_px
            } else {
                measured_width_px.max(profile.min_px)
            };
            targets.push(preferred.clamp(profile.min_px, profile.max_px));
        }

        self.playlist_column_content_targets_px = targets;
    }

    fn resolve_playlist_column_target_width_px(
        override_width_px: Option<u32>,
        current_width_px: Option<u32>,
        stored_target_width_px: Option<u32>,
        content_target_width_px: u32,
        profile: ColumnWidthProfile,
        preserve_current_widths: bool,
    ) -> u32 {
        let base_width_px = if let Some(override_width_px) = override_width_px {
            override_width_px
        } else if preserve_current_widths {
            stored_target_width_px
                .or(current_width_px)
                .unwrap_or(content_target_width_px)
        } else {
            content_target_width_px
        };
        base_width_px.clamp(profile.min_px, profile.max_px)
    }

    fn layout_min_width_px_for_column(
        column: &PlaylistColumnConfig,
        profile: ColumnWidthProfile,
        target_width_px: u32,
    ) -> u32 {
        if Self::is_album_art_builtin_column(column) {
            return target_width_px;
        }
        profile.min_px
    }

    fn compute_playlist_column_widths(
        &self,
        available_width_px: u32,
        preserve_current_widths: bool,
    ) -> (Vec<u32>, HashMap<String, u32>) {
        const COLUMN_SPACING_PX: u32 = 10;
        let visible_columns = self.visible_playlist_columns();
        if visible_columns.is_empty() {
            return (Vec::new(), HashMap::new());
        }

        let mut min_widths = Vec::with_capacity(visible_columns.len());
        let mut max_widths = Vec::with_capacity(visible_columns.len());
        let mut widths = Vec::with_capacity(visible_columns.len());
        let mut target_widths_by_key = HashMap::with_capacity(visible_columns.len());

        for (index, column) in visible_columns.iter().enumerate() {
            let profile = self.column_width_profile_for_column(column);
            let column_key = Self::playlist_column_key(column);
            let override_width = self
                .playlist_column_width_overrides_px
                .get(&column_key)
                .copied();
            let content_target = self
                .playlist_column_content_targets_px
                .get(index)
                .copied()
                .unwrap_or(profile.preferred_px);
            let stored_target = self
                .playlist_column_target_widths_px
                .get(&column_key)
                .copied();
            let current_width = self.playlist_column_widths_px.get(index).copied();
            let target = Self::resolve_playlist_column_target_width_px(
                override_width,
                current_width,
                stored_target,
                content_target,
                profile,
                preserve_current_widths,
            );
            target_widths_by_key.insert(column_key, target);
            min_widths.push(Self::layout_min_width_px_for_column(
                column, profile, target,
            ));
            max_widths.push(profile.max_px);
            widths.push(target);
        }

        let spacing_total =
            COLUMN_SPACING_PX.saturating_mul((widths.len().saturating_sub(1)) as u32);
        let preferred_total: u32 = widths.iter().copied().sum();
        let available_for_columns = if available_width_px == 0 {
            preferred_total
        } else {
            available_width_px.saturating_sub(spacing_total)
        };

        fit_column_widths_to_available_space(
            &mut widths,
            &min_widths,
            &max_widths,
            available_for_columns,
        );

        (widths, target_widths_by_key)
    }

    fn compute_playlist_row_height_px_for_visible_columns(
        visible_columns: &[&PlaylistColumnConfig],
        column_widths_px: &[u32],
        album_art_profile: ColumnWidthProfile,
    ) -> u32 {
        let Some((column_index, _column)) = visible_columns
            .iter()
            .enumerate()
            .find(|(_, column)| Self::is_album_art_builtin_column(column))
        else {
            return BASE_ROW_HEIGHT_PX;
        };

        let album_art_width_px = column_widths_px
            .get(column_index)
            .copied()
            .unwrap_or(album_art_profile.preferred_px);

        album_art_width_px
            .saturating_add(ALBUM_ART_ROW_PADDING_PX)
            .clamp(
                BASE_ROW_HEIGHT_PX,
                album_art_profile
                    .max_px
                    .saturating_add(ALBUM_ART_ROW_PADDING_PX),
            )
    }

    fn compute_playlist_row_height_px(&self, column_widths_px: &[u32]) -> u32 {
        let visible_columns = self.visible_playlist_columns();
        Self::compute_playlist_row_height_px_for_visible_columns(
            &visible_columns,
            column_widths_px,
            self.album_art_column_width_profile(),
        )
    }

    fn apply_playlist_column_layout_internal(&mut self, preserve_current_widths: bool) {
        if self.playlist_column_content_targets_px.len() != self.visible_playlist_columns().len() {
            self.refresh_playlist_column_content_targets();
        }

        let (widths, target_widths_by_key) = self.compute_playlist_column_widths(
            self.playlist_columns_available_width_px,
            preserve_current_widths,
        );
        let row_height_px = self.compute_playlist_row_height_px(&widths);
        let content_width = widths
            .iter()
            .copied()
            .sum::<u32>()
            .saturating_add(10u32.saturating_mul((widths.len().saturating_sub(1)) as u32));
        self.playlist_column_target_widths_px = target_widths_by_key;

        if widths == self.playlist_column_widths_px
            && content_width == self.playlist_columns_content_width_px
            && row_height_px == self.playlist_row_height_px
        {
            return;
        }

        self.playlist_column_widths_px = widths.clone();
        self.playlist_columns_content_width_px = content_width;
        self.playlist_row_height_px = row_height_px;

        let widths_i32: Vec<i32> = widths
            .into_iter()
            .map(|width| width.min(i32::MAX as u32) as i32)
            .collect();
        let mut gap_positions: Vec<i32> = Vec::with_capacity(widths_i32.len() + 1);
        let mut cursor_px: i32 = 0;
        gap_positions.push(0);
        for (index, width_px) in widths_i32.iter().enumerate() {
            cursor_px = cursor_px.saturating_add((*width_px).max(0));
            if index + 1 < widths_i32.len() {
                cursor_px = cursor_px.saturating_add(10);
            }
            gap_positions.push(cursor_px);
        }
        let content_width_i32 = content_width.min(i32::MAX as u32) as i32;
        let row_height_i32 = row_height_px.min(i32::MAX as u32) as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_column_widths_px(ModelRc::from(Rc::new(VecModel::from(widths_i32))));
            ui.set_playlist_column_gap_positions_px(ModelRc::from(Rc::new(VecModel::from(
                gap_positions,
            ))));
            ui.set_playlist_columns_content_width_px(content_width_i32);
            ui.set_playlist_row_height_px(row_height_i32);
        });
    }

    fn apply_playlist_column_layout(&mut self) {
        self.apply_playlist_column_layout_internal(false);
    }

    fn apply_playlist_column_layout_preserving_current_widths(&mut self) {
        self.apply_playlist_column_layout_internal(true);
    }

    fn status_text_from_track_metadata(track_metadata: &TrackMetadata) -> slint::SharedString {
        let title = if track_metadata.title.is_empty() {
            "Unknown".to_string()
        } else {
            track_metadata.title.clone()
        };
        if !track_metadata.artist.is_empty() {
            format!("{} - {}", track_metadata.artist, title).into()
        } else {
            title.into()
        }
    }

    fn resolve_display_target(
        selected_indices: &[usize],
        track_paths: &[PathBuf],
        track_metadata: &[TrackMetadata],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        if let Some(&selected_index) = selected_indices.first() {
            let selected_path = track_paths.get(selected_index).cloned();
            let selected_metadata = track_metadata
                .get(selected_index)
                .map(Self::to_detailed_metadata);
            return (selected_path, selected_metadata);
        }

        let playing_path = playing_track_path.cloned();
        if let Some(path) = playing_track_path {
            if let Some(index) = track_paths.iter().position(|candidate| candidate == path) {
                if let Some(metadata) = track_metadata.get(index) {
                    return (playing_path, Some(Self::to_detailed_metadata(metadata)));
                }
            }
        }

        (playing_path, playing_track_metadata.cloned())
    }

    fn update_display_for_selection(
        &mut self,
        selected_indices: &[usize],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) {
        let (display_path, display_metadata) = Self::resolve_display_target(
            selected_indices,
            &self.track_paths,
            &self.track_metadata,
            playing_track_path,
            playing_track_metadata,
        );
        self.update_cover_art(display_path.as_ref());
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::MetadataDisplayChanged(display_metadata),
        ));
    }

    fn to_detailed_metadata_from_library_song(
        library_song: &protocol::LibrarySong,
    ) -> protocol::DetailedMetadata {
        protocol::DetailedMetadata {
            title: library_song.title.clone(),
            artist: library_song.artist.clone(),
            album: library_song.album.clone(),
            date: library_song.year.clone(),
            genre: library_song.genre.clone(),
        }
    }

    fn resolve_library_display_target(
        selected_indices: &[usize],
        library_entries: &[LibraryEntry],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        let selected_song = selected_indices
            .iter()
            .filter_map(|index| library_entries.get(*index))
            .find_map(|entry| match entry {
                LibraryEntry::Song(song) => Some(song),
                _ => None,
            });
        if let Some(song) = selected_song {
            return (
                Some(song.path.clone()),
                Some(Self::to_detailed_metadata_from_library_song(song)),
            );
        }

        let playing_path = playing_track_path.cloned();
        if let Some(path) = playing_track_path {
            let playing_song = library_entries.iter().find_map(|entry| match entry {
                LibraryEntry::Song(song) if &song.path == path => Some(song),
                _ => None,
            });
            if let Some(song) = playing_song {
                return (
                    playing_path,
                    Some(Self::to_detailed_metadata_from_library_song(song)),
                );
            }
        }

        (playing_path, playing_track_metadata.cloned())
    }

    fn update_display_for_library_selection(
        &mut self,
        selected_indices: &[usize],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) {
        let (display_path, display_metadata) = Self::resolve_library_display_target(
            selected_indices,
            &self.library_entries,
            playing_track_path,
            playing_track_metadata,
        );
        self.update_cover_art(display_path.as_ref());
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::MetadataDisplayChanged(display_metadata),
        ));
    }

    fn update_display_for_active_collection(
        &mut self,
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            let selected_indices = self.library_selected_indices.clone();
            self.update_display_for_library_selection(
                &selected_indices,
                playing_track_path,
                playing_track_metadata,
            );
        } else {
            let selected_indices = self.selected_indices.clone();
            self.update_display_for_selection(
                &selected_indices,
                playing_track_path,
                playing_track_metadata,
            );
        }
    }

    fn next_properties_request_id(&mut self) -> u64 {
        self.properties_request_nonce = self.properties_request_nonce.saturating_add(1);
        if self.properties_request_nonce == 0 {
            self.properties_request_nonce = 1;
        }
        self.properties_request_nonce
    }

    fn playlist_properties_target(&self) -> Option<(PathBuf, String)> {
        if self.selected_indices.len() != 1 {
            return None;
        }
        let index = *self.selected_indices.first()?;
        let path = self.track_paths.get(index)?.clone();
        let title = self
            .track_metadata
            .get(index)
            .map(|meta| meta.title.trim().to_string())
            .filter(|title| !title.is_empty())
            .or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        Some((path, title))
    }

    fn library_properties_target(&self) -> Option<(PathBuf, String)> {
        if self.library_selected_indices.len() != 1 {
            return None;
        }
        let source_index = *self.library_selected_indices.first()?;
        let song = match self.library_entries.get(source_index)? {
            LibraryEntry::Song(song) => song,
            _ => return None,
        };
        let title = song.title.trim().to_string();
        let display_title = if title.is_empty() {
            song.path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_default()
        } else {
            title
        };
        Some((song.path.clone(), display_title))
    }

    fn active_properties_target(&self) -> Option<(PathBuf, String)> {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.library_properties_target()
        } else {
            self.playlist_properties_target()
        }
    }

    fn sync_properties_action_state(&self) {
        let playlist_enabled = self.collection_mode == COLLECTION_MODE_PLAYLIST
            && self.playlist_properties_target().is_some();
        let library_enabled = self.collection_mode == COLLECTION_MODE_LIBRARY
            && self.library_properties_target().is_some();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_properties_enabled(playlist_enabled);
            ui.set_library_properties_enabled(library_enabled);
        });
    }

    fn to_ui_metadata_fields(
        fields: &[protocol::MetadataEditorField],
    ) -> Vec<UiMetadataEditorField> {
        fields
            .iter()
            .map(|field| UiMetadataEditorField {
                id: field.id.as_str().into(),
                field_name: field.field_name.as_str().into(),
                value: field.value.as_str().into(),
                common: field.common,
            })
            .collect()
    }

    fn properties_has_changes(&self) -> bool {
        if self.properties_fields.len() != self.properties_original_fields.len() {
            return true;
        }

        self.properties_fields
            .iter()
            .zip(self.properties_original_fields.iter())
            .any(|(left, right)| left.id != right.id || left.value != right.value)
    }

    fn properties_save_enabled(&self) -> bool {
        self.properties_dialog_visible
            && !self.properties_busy
            && self.properties_target_path.is_some()
            && !self.properties_fields.is_empty()
            && self.properties_has_changes()
    }

    fn sync_properties_dialog_ui(&self) {
        let visible = self.properties_dialog_visible;
        let busy = self.properties_busy;
        let error_text = self.properties_error_text.clone();
        let target_title = self.properties_target_title.clone();
        let fields = Self::to_ui_metadata_fields(&self.properties_fields);
        let save_enabled = self.properties_save_enabled();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_show_properties_dialog(visible);
            ui.set_properties_busy(busy);
            ui.set_properties_error_text(error_text.into());
            ui.set_properties_target_title(target_title.into());
            ui.set_properties_fields(ModelRc::from(Rc::new(VecModel::from(fields))));
            ui.set_properties_save_enabled(save_enabled);
        });
    }

    fn sync_properties_edit_state_ui(&self) {
        let error_text = self.properties_error_text.clone();
        let save_enabled = self.properties_save_enabled();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_properties_error_text(error_text.into());
            ui.set_properties_save_enabled(save_enabled);
        });
    }

    fn reset_properties_dialog_state(&mut self) {
        self.properties_pending_request_id = None;
        self.properties_pending_request_kind = None;
        self.properties_target_path = None;
        self.properties_target_title.clear();
        self.properties_original_fields.clear();
        self.properties_fields.clear();
        self.properties_dialog_visible = false;
        self.properties_busy = false;
        self.properties_error_text.clear();
    }

    fn open_properties_for_current_selection(&mut self) {
        let Some((path, target_title)) = self.active_properties_target() else {
            return;
        };

        self.properties_target_path = Some(path.clone());
        self.properties_target_title = target_title;
        self.properties_original_fields.clear();
        self.properties_fields.clear();
        self.properties_error_text.clear();
        self.properties_dialog_visible = true;
        self.properties_busy = true;

        let request_id = self.next_properties_request_id();
        self.properties_pending_request_id = Some(request_id);
        self.properties_pending_request_kind = Some(PropertiesRequestKind::Load);

        let _ = self.bus_sender.send(protocol::Message::Metadata(
            protocol::MetadataMessage::RequestTrackProperties { request_id, path },
        ));
        self.sync_properties_dialog_ui();
    }

    fn edit_properties_field(&mut self, index: usize, value: String) {
        if self.properties_busy || !self.properties_dialog_visible {
            return;
        }
        let Some(field) = self.properties_fields.get_mut(index) else {
            return;
        };
        if field.value == value {
            return;
        }
        field.value = value;
        self.properties_error_text.clear();
        self.sync_properties_edit_state_ui();
    }

    fn save_properties(&mut self) {
        if !self.properties_save_enabled() {
            return;
        }
        let Some(path) = self.properties_target_path.clone() else {
            return;
        };

        let request_id = self.next_properties_request_id();
        self.properties_pending_request_id = Some(request_id);
        self.properties_pending_request_kind = Some(PropertiesRequestKind::Save);
        self.properties_busy = true;
        self.properties_error_text.clear();
        let fields = self.properties_fields.clone();
        let _ = self.bus_sender.send(protocol::Message::Metadata(
            protocol::MetadataMessage::SaveTrackProperties {
                request_id,
                path,
                fields,
            },
        ));
        self.sync_properties_dialog_ui();
    }

    fn cancel_properties(&mut self) {
        self.reset_properties_dialog_state();
        self.sync_properties_dialog_ui();
    }

    fn expected_properties_response(
        &self,
        kind: PropertiesRequestKind,
        request_id: u64,
        path: &Path,
    ) -> bool {
        self.properties_dialog_visible
            && self.properties_pending_request_kind == Some(kind)
            && self.properties_pending_request_id == Some(request_id)
            && self.properties_target_path.as_deref() == Some(path)
    }

    fn handle_properties_loaded(
        &mut self,
        request_id: u64,
        path: PathBuf,
        display_name: String,
        fields: Vec<protocol::MetadataEditorField>,
    ) {
        if !self.expected_properties_response(PropertiesRequestKind::Load, request_id, &path) {
            return;
        }

        self.properties_pending_request_id = None;
        self.properties_pending_request_kind = None;
        self.properties_busy = false;
        self.properties_error_text.clear();
        self.properties_target_title = display_name;
        self.properties_original_fields = fields.clone();
        self.properties_fields = fields;
        self.sync_properties_dialog_ui();
    }

    fn handle_properties_load_failed(&mut self, request_id: u64, path: PathBuf, error: String) {
        if !self.expected_properties_response(PropertiesRequestKind::Load, request_id, &path) {
            return;
        }

        self.properties_pending_request_id = None;
        self.properties_pending_request_kind = None;
        self.properties_busy = false;
        self.properties_error_text = error;
        self.sync_properties_dialog_ui();
    }

    fn apply_summary_to_playlist_metadata(
        &mut self,
        path: &Path,
        summary: &protocol::TrackMetadataSummary,
    ) -> bool {
        let mut changed = false;
        let date = if summary.date.trim().is_empty() {
            summary.year.clone()
        } else {
            summary.date.clone()
        };

        for (index, track_path) in self.track_paths.iter().enumerate() {
            if track_path.as_path() != path {
                continue;
            }
            let Some(metadata) = self.track_metadata.get_mut(index) else {
                continue;
            };

            if metadata.title != summary.title {
                metadata.title = summary.title.clone();
                changed = true;
            }
            if metadata.artist != summary.artist {
                metadata.artist = summary.artist.clone();
                changed = true;
            }
            if metadata.album != summary.album {
                metadata.album = summary.album.clone();
                changed = true;
            }
            if metadata.album_artist != summary.album_artist {
                metadata.album_artist = summary.album_artist.clone();
                changed = true;
            }
            if metadata.date != date {
                metadata.date = date.clone();
                changed = true;
            }
            if metadata.year != summary.year {
                metadata.year = summary.year.clone();
                changed = true;
            }
            if metadata.genre != summary.genre {
                metadata.genre = summary.genre.clone();
                changed = true;
            }
            if metadata.track_number != summary.track_number {
                metadata.track_number = summary.track_number.clone();
                changed = true;
            }
        }

        if self.current_playing_track_path.as_deref() == Some(path) {
            let detailed = protocol::DetailedMetadata {
                title: summary.title.clone(),
                artist: summary.artist.clone(),
                album: summary.album.clone(),
                date,
                genre: summary.genre.clone(),
            };
            if self.current_playing_track_metadata.as_ref().map(|meta| {
                meta.title == detailed.title
                    && meta.artist == detailed.artist
                    && meta.album == detailed.album
                    && meta.date == detailed.date
                    && meta.genre == detailed.genre
            }) != Some(true)
            {
                self.current_playing_track_metadata = Some(detailed);
                changed = true;
            }
        }

        changed
    }

    fn apply_summary_to_library_entries(
        &mut self,
        path: &Path,
        summary: &protocol::TrackMetadataSummary,
    ) -> bool {
        let mut changed = false;
        for entry in &mut self.library_entries {
            let LibraryEntry::Song(song) = entry else {
                continue;
            };
            if song.path.as_path() != path {
                continue;
            }

            if song.title != summary.title {
                song.title = summary.title.clone();
                changed = true;
            }
            if song.artist != summary.artist {
                song.artist = summary.artist.clone();
                changed = true;
            }
            if song.album != summary.album {
                song.album = summary.album.clone();
                changed = true;
            }
            if song.album_artist != summary.album_artist {
                song.album_artist = summary.album_artist.clone();
                changed = true;
            }
            if song.genre != summary.genre {
                song.genre = summary.genre.clone();
                changed = true;
            }
            if song.year != summary.year {
                song.year = summary.year.clone();
                changed = true;
            }
            if song.track_number != summary.track_number {
                song.track_number = summary.track_number.clone();
                changed = true;
            }
        }
        changed
    }

    fn handle_properties_saved(
        &mut self,
        request_id: u64,
        path: PathBuf,
        summary: protocol::TrackMetadataSummary,
        db_sync_warning: Option<String>,
    ) {
        if !self.expected_properties_response(PropertiesRequestKind::Save, request_id, &path) {
            return;
        }

        self.properties_pending_request_id = None;
        self.properties_pending_request_kind = None;
        self.properties_busy = false;

        let playlist_changed = self.apply_summary_to_playlist_metadata(&path, &summary);
        let library_changed = self.apply_summary_to_library_entries(&path, &summary);

        if playlist_changed {
            self.refresh_playlist_column_content_targets();
            self.apply_playlist_column_layout();
            self.rebuild_track_model();
        }

        if self.collection_mode == COLLECTION_MODE_LIBRARY && library_changed {
            self.sync_library_ui();
        }

        if self.collection_mode == COLLECTION_MODE_LIBRARY && db_sync_warning.is_none() {
            self.request_library_view_data();
        }

        let playing_track_path = self.current_playing_track_path.clone();
        let playing_track_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );

        if let Some(warning) = db_sync_warning {
            self.library_status_text = warning.clone();
            self.show_library_toast(warning);
        }

        self.reset_properties_dialog_state();
        self.sync_properties_dialog_ui();
        self.sync_properties_action_state();
    }

    fn handle_properties_save_failed(&mut self, request_id: u64, path: PathBuf, error: String) {
        if !self.expected_properties_response(PropertiesRequestKind::Save, request_id, &path) {
            return;
        }

        self.properties_pending_request_id = None;
        self.properties_pending_request_kind = None;
        self.properties_busy = false;
        self.properties_error_text = error;
        self.sync_properties_dialog_ui();
    }

    fn normalized_search_query(query: &str) -> String {
        query.trim().to_ascii_lowercase()
    }

    fn reset_filter_state_fields(
        filter_sort_column_key: &mut Option<String>,
        filter_sort_direction: &mut Option<PlaylistSortDirection>,
        filter_search_query: &mut String,
        filter_search_visible: &mut bool,
    ) {
        *filter_sort_column_key = None;
        *filter_sort_direction = None;
        filter_search_query.clear();
        *filter_search_visible = false;
    }

    fn reset_filter_state(&mut self) {
        Self::reset_filter_state_fields(
            &mut self.filter_sort_column_key,
            &mut self.filter_sort_direction,
            &mut self.filter_search_query,
            &mut self.filter_search_visible,
        );
    }

    fn is_filter_applied(&self) -> bool {
        self.filter_sort_direction.is_some() || !self.filter_search_query.trim().is_empty()
    }

    fn is_filter_view_active(&self) -> bool {
        self.is_filter_applied()
    }

    fn map_view_to_source_index(&self, view_index: usize) -> Option<usize> {
        if self.view_indices.is_empty() {
            return (view_index < self.track_metadata.len()).then_some(view_index);
        }
        self.view_indices.get(view_index).copied()
    }

    fn map_source_to_view_index(&self, source_index: usize) -> Option<usize> {
        if self.view_indices.is_empty() {
            return (source_index < self.track_metadata.len()).then_some(source_index);
        }
        self.view_indices
            .iter()
            .position(|&candidate| candidate == source_index)
    }

    fn map_library_view_to_source_index(&self, view_index: usize) -> Option<usize> {
        if self.library_view_indices.is_empty() {
            if self.library_entries.is_empty() {
                return None;
            }
            if self.library_search_query.trim().is_empty() {
                return (view_index < self.library_entries.len()).then_some(view_index);
            }
            return None;
        }
        self.library_view_indices.get(view_index).copied()
    }

    fn library_text_matches_search(value: &str, normalized_query: &str) -> bool {
        value.to_ascii_lowercase().contains(normalized_query)
    }

    fn library_entry_matches_search(entry: &LibraryEntry, normalized_query: &str) -> bool {
        if normalized_query.is_empty() {
            return true;
        }
        match entry {
            LibraryEntry::Song(song) => {
                Self::library_text_matches_search(&song.title, normalized_query)
                    || Self::library_text_matches_search(&song.artist, normalized_query)
                    || Self::library_text_matches_search(&song.album, normalized_query)
                    || Self::library_text_matches_search(&song.album_artist, normalized_query)
                    || Self::library_text_matches_search(&song.genre, normalized_query)
                    || Self::library_text_matches_search(&song.year, normalized_query)
                    || Self::library_text_matches_search(&song.track_number, normalized_query)
                    || Self::library_text_matches_search(
                        &song.path.to_string_lossy(),
                        normalized_query,
                    )
            }
            LibraryEntry::Artist(artist) => {
                Self::library_text_matches_search(&artist.artist, normalized_query)
                    || artist.album_count.to_string().contains(normalized_query)
                    || artist.song_count.to_string().contains(normalized_query)
            }
            LibraryEntry::Album(album) => {
                Self::library_text_matches_search(&album.album, normalized_query)
                    || Self::library_text_matches_search(&album.album_artist, normalized_query)
                    || album.song_count.to_string().contains(normalized_query)
            }
            LibraryEntry::Genre(genre) => {
                Self::library_text_matches_search(&genre.genre, normalized_query)
                    || genre.song_count.to_string().contains(normalized_query)
            }
            LibraryEntry::Decade(decade) => {
                Self::library_text_matches_search(&decade.decade, normalized_query)
                    || decade.song_count.to_string().contains(normalized_query)
            }
        }
    }

    fn build_library_view_indices_for_query(
        entries: &[LibraryEntry],
        search_query: &str,
    ) -> Vec<usize> {
        let normalized_query = Self::normalized_search_query(search_query);
        entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                Self::library_entry_matches_search(entry, &normalized_query).then_some(index)
            })
            .collect()
    }

    fn selection_anchor_source_index(&self) -> Option<usize> {
        self.selection_anchor_track_id
            .as_ref()
            .and_then(|anchor_id| {
                self.track_ids
                    .iter()
                    .position(|track_id| track_id == anchor_id)
            })
    }

    fn set_selection_anchor_from_source_index(&mut self, source_index: usize) {
        self.selection_anchor_track_id = self.track_ids.get(source_index).cloned();
    }

    fn build_shift_selection_from_view_order(
        view_indices: &[usize],
        anchor_source_index: Option<usize>,
        clicked_source_index: usize,
    ) -> Vec<usize> {
        if view_indices.is_empty() {
            return vec![clicked_source_index];
        }
        let Some(clicked_view_index) = view_indices
            .iter()
            .position(|&source_index| source_index == clicked_source_index)
        else {
            return vec![clicked_source_index];
        };
        let anchor_source_index = anchor_source_index.unwrap_or(clicked_source_index);
        let Some(anchor_view_index) = view_indices
            .iter()
            .position(|&source_index| source_index == anchor_source_index)
        else {
            return vec![clicked_source_index];
        };
        let start = anchor_view_index.min(clicked_view_index);
        let end = anchor_view_index.max(clicked_view_index);
        view_indices[start..=end].to_vec()
    }

    fn active_sort_column_state(&self) -> Option<(usize, String)> {
        let sort_key = self.filter_sort_column_key.as_ref()?;
        self.visible_playlist_columns()
            .iter()
            .enumerate()
            .find_map(|(index, column)| {
                (Self::playlist_column_key(column) == *sort_key)
                    .then(|| (index, column.name.clone()))
            })
    }

    fn sort_state_model(&self) -> Vec<i32> {
        let active_key = self.filter_sort_column_key.as_ref();
        let active_state = self.filter_sort_direction;
        self.visible_playlist_columns()
            .iter()
            .map(|column| {
                let key = Self::playlist_column_key(column);
                if active_key == Some(&key) {
                    match active_state {
                        Some(PlaylistSortDirection::Ascending) => 1,
                        Some(PlaylistSortDirection::Descending) => 2,
                        None => 0,
                    }
                } else {
                    0
                }
            })
            .collect()
    }

    fn filter_summary_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some((_, column_name)) = self.active_sort_column_state() {
            let direction = match self.filter_sort_direction {
                Some(PlaylistSortDirection::Ascending) => "asc",
                Some(PlaylistSortDirection::Descending) => "desc",
                None => "",
            };
            if !direction.is_empty() {
                parts.push(format!("Sort: {} ({})", column_name, direction));
            }
        }

        let query = self.filter_search_query.trim();
        if !query.is_empty() {
            parts.push(format!("Search: \"{}\"", query));
        }

        if parts.is_empty() {
            if self.filter_search_visible {
                return "Filter view active".to_string();
            }
            return String::new();
        }

        format!(
            "Filter view ({}/{}): {}",
            self.view_indices.len(),
            self.track_metadata.len(),
            parts.join(" | ")
        )
    }

    fn search_result_text(&self) -> String {
        let total = self.track_metadata.len();
        let found = if self.is_filter_applied() || !self.view_indices.is_empty() {
            self.view_indices.len()
        } else {
            total
        };
        format!("{}/{}", found, total)
    }

    fn library_search_result_text(&self) -> String {
        let total = self.library_entries.len();
        let found = if matches!(self.current_library_view(), LibraryViewState::GlobalSearch)
            && self.library_search_query.trim().is_empty()
        {
            0
        } else if self.library_search_query.trim().is_empty() {
            total
        } else {
            self.library_view_indices.len()
        };
        format!("{}/{}", found, total)
    }

    fn sync_library_search_state_to_ui(&self) {
        let search_visible = self.library_search_visible;
        let search_result_text = self.library_search_result_text();

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_search_visible(search_visible);
            ui.set_library_search_result_text(search_result_text.into());
        });
    }

    fn sync_filter_state_to_ui(&self) {
        let sort_states = self.sort_state_model();
        let filter_active = self.is_filter_view_active();
        let search_visible = self.filter_search_visible;
        let search_query = self.filter_search_query.clone();
        let search_result_text = self.search_result_text();
        let summary = self.filter_summary_text();

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_filter_active(filter_active);
            ui.set_playlist_search_visible(search_visible);
            ui.set_playlist_search_query(search_query.into());
            ui.set_playlist_search_result_text(search_result_text.into());
            ui.set_playlist_filter_summary(summary.into());
            ui.set_playlist_column_sort_states(ModelRc::from(Rc::new(VecModel::from(sort_states))));
        });
    }

    fn rebuild_track_model(&mut self) {
        let normalized_query = Self::normalized_search_query(&self.filter_search_query);
        let mut active_sort = self.active_sort_column_state();

        if self.filter_sort_direction.is_some() && active_sort.is_none() {
            self.filter_sort_column_key = None;
            self.filter_sort_direction = None;
            active_sort = None;
        }

        let active_sort_index = active_sort.map(|(index, _)| index);
        let descending = self.filter_sort_direction == Some(PlaylistSortDirection::Descending);
        let album_art_column_visible = self.is_album_art_column_visible();
        if album_art_column_visible {
            self.ensure_track_cover_art_slots();
        }

        struct ViewRow {
            source_index: usize,
            values: Vec<String>,
            sort_key: String,
        }

        let mut rows: Vec<ViewRow> = Vec::with_capacity(self.track_metadata.len());
        for (source_index, metadata) in self.track_metadata.iter().enumerate() {
            let values = Self::build_playlist_row_values(metadata, &self.playlist_columns);
            if !normalized_query.is_empty()
                && !values
                    .iter()
                    .any(|value| value.to_ascii_lowercase().contains(&normalized_query))
            {
                continue;
            }

            let sort_key = active_sort_index
                .and_then(|index| values.get(index))
                .map(|value| value.to_ascii_lowercase())
                .unwrap_or_default();

            rows.push(ViewRow {
                source_index,
                values,
                sort_key,
            });
        }

        if active_sort_index.is_some() {
            rows.sort_by(|lhs, rhs| {
                let order = lhs
                    .sort_key
                    .cmp(&rhs.sort_key)
                    .then_with(|| lhs.source_index.cmp(&rhs.source_index));
                if descending {
                    order.reverse()
                } else {
                    order
                }
            });
        }

        self.view_indices = rows.iter().map(|row| row.source_index).collect();
        let selected_set: HashSet<usize> = self.selected_indices.iter().copied().collect();
        let active_playing_index = self.active_playing_index;
        let playback_active = self.playback_active;
        let selected_view_index = self
            .selected_indices
            .iter()
            .find_map(|&source_index| self.map_source_to_view_index(source_index))
            .map(|index| index as i32)
            .unwrap_or(-1);
        let playing_view_index = active_playing_index
            .and_then(|source_index| self.map_source_to_view_index(source_index))
            .map(|index| index as i32)
            .unwrap_or(-1);
        let row_data: Vec<(Vec<String>, Option<PathBuf>, bool, String)> = rows
            .into_iter()
            .map(|row| {
                let status = if Some(row.source_index) == active_playing_index {
                    if playback_active {
                        "▶️"
                    } else {
                        "⏸️"
                    }
                } else {
                    ""
                };
                let album_art_path = album_art_column_visible
                    .then(|| self.row_cover_art_path(row.source_index))
                    .flatten();
                (
                    row.values,
                    album_art_path,
                    selected_set.contains(&row.source_index),
                    status.to_string(),
                )
            })
            .collect();

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let mut rows = Vec::with_capacity(row_data.len());
            for (values, album_art_path, selected, status) in row_data {
                let values_shared: Vec<slint::SharedString> =
                    values.into_iter().map(Into::into).collect();
                rows.push(TrackRowData {
                    status: status.into(),
                    values: ModelRc::from(values_shared.as_slice()),
                    album_art: UiManager::load_track_row_cover_art_image(album_art_path.as_ref()),
                    selected,
                });
            }
            UiManager::update_or_replace_track_model(&ui, rows);

            ui.set_selected_track_index(selected_view_index);
            ui.set_playing_track_index(playing_view_index);
        });

        self.sync_filter_state_to_ui();
        self.sync_properties_action_state();
    }

    fn open_playlist_search(&mut self) {
        self.filter_search_visible = true;
        self.sync_filter_state_to_ui();
    }

    fn close_playlist_search(&mut self) {
        self.filter_search_visible = false;
        if !self.filter_search_query.is_empty() {
            self.filter_search_query.clear();
            self.rebuild_track_model();
        } else {
            self.sync_filter_state_to_ui();
        }
    }

    fn set_playlist_search_query(&mut self, query: String) {
        self.filter_search_visible = true;
        if self.filter_search_query == query {
            self.sync_filter_state_to_ui();
            return;
        }
        self.filter_search_query = query;
        self.rebuild_track_model();
    }

    fn open_library_search(&mut self) {
        self.library_search_visible = true;
        self.sync_library_search_state_to_ui();

        let ui_handle = self.ui.clone();
        let _ = self.ui.upgrade_in_event_loop(move |_| {
            let deferred_ui_handle = ui_handle.clone();
            slint::Timer::single_shot(Duration::from_millis(0), move || {
                if let Some(ui) = deferred_ui_handle.upgrade() {
                    if ui.get_collection_mode() == COLLECTION_MODE_LIBRARY
                        && ui.get_library_search_visible()
                    {
                        ui.invoke_focus_library_search_input();
                    }
                }
            });
        });
    }

    fn close_library_search(&mut self) {
        self.library_search_visible = false;
        if !self.library_search_query.is_empty() {
            self.library_search_query.clear();
            if matches!(self.current_library_view(), LibraryViewState::GlobalSearch) {
                self.request_library_view_data();
            } else {
                self.sync_library_ui();
            }
        } else {
            self.sync_library_search_state_to_ui();
        }
    }

    fn set_library_search_query(&mut self, query: String) {
        self.library_search_visible = true;
        if self.library_search_query == query {
            self.sync_library_search_state_to_ui();
            return;
        }
        self.library_search_query = query;
        if matches!(self.current_library_view(), LibraryViewState::GlobalSearch)
            && !self.library_search_query.trim().is_empty()
            && self.library_entries.is_empty()
        {
            self.request_library_view_data();
        } else {
            self.sync_library_ui();
        }
    }

    fn open_global_library_search(&mut self) {
        self.set_collection_mode(COLLECTION_MODE_LIBRARY);
        if !matches!(self.current_library_view(), LibraryViewState::GlobalSearch) {
            self.library_view_stack.push(LibraryViewState::GlobalSearch);
            self.reset_library_selection();
            self.library_add_to_dialog_visible = false;
            self.sync_library_add_to_playlist_ui();
        }
        self.request_library_view_data();
        self.open_library_search();
        self.sync_library_ui();
    }

    fn clear_playlist_filter_view(&mut self) {
        self.reset_filter_state();
        self.rebuild_track_model();
    }

    fn cycle_playlist_sort_by_column(&mut self, view_column_index: usize) {
        let sort_key = {
            let visible_columns = self.visible_playlist_columns();
            let Some(column) = visible_columns.get(view_column_index) else {
                return;
            };
            if !Self::is_sortable_playlist_column(column) {
                return;
            }
            Self::playlist_column_key(column)
        };

        if self.filter_sort_column_key.as_deref() != Some(sort_key.as_str()) {
            self.filter_sort_column_key = Some(sort_key);
            self.filter_sort_direction = Some(PlaylistSortDirection::Ascending);
        } else {
            match self.filter_sort_direction {
                Some(PlaylistSortDirection::Ascending) => {
                    self.filter_sort_direction = Some(PlaylistSortDirection::Descending);
                }
                Some(PlaylistSortDirection::Descending) => {
                    self.filter_sort_direction = None;
                    self.filter_sort_column_key = None;
                }
                None => {
                    self.filter_sort_direction = Some(PlaylistSortDirection::Ascending);
                }
            }
        }

        self.rebuild_track_model();
    }

    fn apply_filter_view_snapshot_locally(&mut self, source_indices: Vec<usize>) {
        let len = self.track_ids.len();
        let mut seen_indices = HashSet::new();
        let normalized: Vec<usize> = source_indices
            .into_iter()
            .filter(|&index| index < len)
            .filter(|index| seen_indices.insert(*index))
            .collect();

        let selected_ids: Vec<String> = self
            .selected_indices
            .iter()
            .filter_map(|&index| self.track_ids.get(index).cloned())
            .collect();
        let active_playing_id = self
            .active_playing_index
            .and_then(|index| self.track_ids.get(index).cloned());

        let mut new_track_ids = Vec::with_capacity(normalized.len());
        let mut new_track_paths = Vec::with_capacity(normalized.len());
        let mut new_track_cover_art_paths = Vec::with_capacity(normalized.len());
        let mut new_track_metadata = Vec::with_capacity(normalized.len());
        for &index in &normalized {
            if let (Some(id), Some(path), Some(metadata)) = (
                self.track_ids.get(index),
                self.track_paths.get(index),
                self.track_metadata.get(index),
            ) {
                new_track_ids.push(id.clone());
                new_track_paths.push(path.clone());
                new_track_cover_art_paths.push(
                    self.track_cover_art_paths
                        .get(index)
                        .cloned()
                        .unwrap_or(None),
                );
                new_track_metadata.push(metadata.clone());
            }
        }

        self.track_ids = new_track_ids;
        self.track_paths = new_track_paths;
        self.track_cover_art_paths = new_track_cover_art_paths;
        self.track_metadata = new_track_metadata;

        self.selected_indices = selected_ids
            .into_iter()
            .filter_map(|selected_id| self.track_ids.iter().position(|id| id == &selected_id))
            .collect();
        self.selected_indices.sort_unstable();
        self.selected_indices.dedup();

        self.active_playing_index = active_playing_id
            .as_ref()
            .and_then(|playing_id| self.track_ids.iter().position(|id| id == playing_id));
        if self.active_playing_index.is_none() {
            self.playback_active = false;
            self.current_playing_track_path = None;
            self.current_playing_track_metadata = None;
        }

        self.reset_filter_state();

        self.refresh_playlist_column_content_targets();
        self.apply_playlist_column_layout();
        let playing_path = self.current_playing_track_path.clone();
        let playing_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(playing_path.as_ref(), playing_metadata.as_ref());
        self.rebuild_track_model();
    }

    fn build_copied_track_paths(
        track_paths: &[PathBuf],
        selected_indices: &[usize],
        view_indices: &[usize],
    ) -> Vec<PathBuf> {
        let mut normalized = selected_indices.to_vec();
        normalized.sort_unstable();
        normalized.dedup();

        let ordered_indices = if view_indices.is_empty() {
            normalized
        } else {
            let mut selected_set: HashSet<usize> = normalized.iter().copied().collect();
            let mut ordered = Vec::with_capacity(normalized.len());

            for &source_index in view_indices {
                if selected_set.remove(&source_index) {
                    ordered.push(source_index);
                }
            }

            // Keep any selected-but-not-rendered rows in stable source order.
            for source_index in normalized {
                if selected_set.remove(&source_index) {
                    ordered.push(source_index);
                }
            }
            ordered
        };

        ordered_indices
            .into_iter()
            .filter_map(|index| track_paths.get(index).cloned())
            .collect()
    }

    fn copy_selected_tracks(&mut self) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.copy_selected_library_items();
            return;
        }
        if self.selected_indices.is_empty() {
            return;
        }
        self.copied_track_paths = Self::build_copied_track_paths(
            &self.track_paths,
            &self.selected_indices,
            &self.view_indices,
        );
        self.copied_library_selections.clear();
        let copied_count = self.copied_track_paths.len();
        self.library_status_text = format!("Copied {} tracks", copied_count);
        self.show_library_toast(self.library_status_text.clone());
        debug!(
            "UiManager: copied {} track(s) from selection",
            self.copied_track_paths.len()
        );
    }

    fn estimate_library_copied_track_count(
        entries: &[LibraryEntry],
        selected_indices: &[usize],
    ) -> usize {
        let mut seen = HashSet::new();
        let mut total = 0usize;

        for index in selected_indices {
            let Some(entry) = entries.get(*index) else {
                continue;
            };
            let (key, count) = match entry {
                LibraryEntry::Song(song) => {
                    (format!("song:{}", song.path.to_string_lossy()), 1usize)
                }
                LibraryEntry::Artist(artist) => (
                    format!("artist:{}", artist.artist),
                    artist.song_count as usize,
                ),
                LibraryEntry::Album(album) => (
                    format!("album:{}\u{001f}{}", album.album, album.album_artist),
                    album.song_count as usize,
                ),
                LibraryEntry::Genre(genre) => {
                    (format!("genre:{}", genre.genre), genre.song_count as usize)
                }
                LibraryEntry::Decade(decade) => (
                    format!("decade:{}", decade.decade),
                    decade.song_count as usize,
                ),
            };
            if seen.insert(key) {
                total = total.saturating_add(count);
            }
        }

        total
    }

    fn copy_selected_library_items(&mut self) {
        let selections = self.build_library_selection_specs();
        if selections.is_empty() {
            return;
        }
        self.copied_library_selections = selections;
        self.copied_track_paths.clear();
        let copied_count = Self::estimate_library_copied_track_count(
            &self.library_entries,
            &self.library_selected_indices,
        );
        self.library_status_text = format!("Copied {} tracks", copied_count);
        self.show_library_toast(self.library_status_text.clone());
        debug!(
            "UiManager: copied {} library selection item(s)",
            self.copied_library_selections.len()
        );
    }

    fn paste_copied_tracks(&mut self) {
        if !self.copied_track_paths.is_empty() {
            self.pending_paste_feedback = true;
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::PasteTracks(self.copied_track_paths.clone()),
            ));
            return;
        }

        if self.copied_library_selections.is_empty() || self.active_playlist_id.is_empty() {
            return;
        }

        self.pending_paste_feedback = true;
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::AddSelectionToPlaylists {
                selections: self.copied_library_selections.clone(),
                playlist_ids: vec![self.active_playlist_id.clone()],
            },
        ));
    }

    fn cut_selected_tracks(&mut self) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.copy_selected_library_items();
            return;
        }
        if self.is_filter_view_active() || self.selected_indices.is_empty() {
            return;
        }
        self.copy_selected_tracks();
        let _ = self.bus_sender.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::DeleteTracks(self.selected_indices.clone()),
        ));
    }

    fn current_library_view(&self) -> LibraryViewState {
        self.library_view_stack
            .last()
            .cloned()
            .unwrap_or(LibraryViewState::SongsRoot)
    }

    fn scroll_view_key_for_library_view(view: &LibraryViewState) -> Option<LibraryScrollViewKey> {
        match view {
            LibraryViewState::SongsRoot => Some(LibraryScrollViewKey::SongsRoot),
            LibraryViewState::ArtistsRoot => Some(LibraryScrollViewKey::ArtistsRoot),
            LibraryViewState::AlbumsRoot => Some(LibraryScrollViewKey::AlbumsRoot),
            LibraryViewState::GenresRoot => Some(LibraryScrollViewKey::GenresRoot),
            LibraryViewState::DecadesRoot => Some(LibraryScrollViewKey::DecadesRoot),
            LibraryViewState::GlobalSearch => Some(LibraryScrollViewKey::GlobalSearch),
            LibraryViewState::ArtistDetail { .. }
            | LibraryViewState::AlbumDetail { .. }
            | LibraryViewState::GenreDetail { .. }
            | LibraryViewState::DecadeDetail { .. } => None,
        }
    }

    fn remember_scroll_position_for_view(&mut self, view: &LibraryViewState, first_row: usize) {
        if let Some(key) = Self::scroll_view_key_for_library_view(view) {
            self.library_scroll_positions.insert(key, first_row);
        }
    }

    fn remember_current_library_scroll_position(&mut self) {
        let current_view = self.current_library_view();
        self.remember_scroll_position_for_view(
            &current_view,
            self.library_artist_prefetch_first_row,
        );
    }

    fn saved_scroll_position_for_view(&self, view: &LibraryViewState) -> Option<usize> {
        let key = Self::scroll_view_key_for_library_view(view)?;
        self.library_scroll_positions.get(&key).copied()
    }

    fn current_library_root_index(&self) -> i32 {
        match self.library_view_stack.first() {
            Some(LibraryViewState::SongsRoot) => 0,
            Some(LibraryViewState::ArtistsRoot) => 1,
            Some(LibraryViewState::AlbumsRoot) => 2,
            Some(LibraryViewState::GenresRoot) => 3,
            Some(LibraryViewState::DecadesRoot) => 4,
            Some(LibraryViewState::GlobalSearch) => match self.library_view_stack.first() {
                Some(LibraryViewState::SongsRoot) => 0,
                Some(LibraryViewState::ArtistsRoot) => 1,
                Some(LibraryViewState::AlbumsRoot) => 2,
                Some(LibraryViewState::GenresRoot) => 3,
                Some(LibraryViewState::DecadesRoot) => 4,
                _ => 0,
            },
            Some(LibraryViewState::ArtistDetail { .. }) => 1,
            Some(LibraryViewState::AlbumDetail { .. }) => 2,
            Some(LibraryViewState::GenreDetail { .. }) => 3,
            Some(LibraryViewState::DecadeDetail { .. }) => 4,
            None => 0,
        }
    }

    fn library_view_labels(view: &LibraryViewState) -> (String, String) {
        match view {
            LibraryViewState::SongsRoot => ("Songs".to_string(), "All songs".to_string()),
            LibraryViewState::ArtistsRoot => ("Artists".to_string(), "All artists".to_string()),
            LibraryViewState::AlbumsRoot => ("Albums".to_string(), "All albums".to_string()),
            LibraryViewState::GenresRoot => ("Genres".to_string(), "All genres".to_string()),
            LibraryViewState::DecadesRoot => ("Decades".to_string(), "All decades".to_string()),
            LibraryViewState::GlobalSearch => (
                "Global Search".to_string(),
                "Type to search songs, artists, and albums".to_string(),
            ),
            LibraryViewState::ArtistDetail { artist } => {
                (artist.clone(), "Songs grouped by album".to_string())
            }
            LibraryViewState::AlbumDetail {
                album,
                album_artist,
            } => (album.clone(), format!("by {}", album_artist)),
            LibraryViewState::GenreDetail { genre } => {
                (genre.clone(), "Songs in genre".to_string())
            }
            LibraryViewState::DecadeDetail { decade } => {
                (decade.clone(), "Songs in decade".to_string())
            }
        }
    }

    fn detail_enrichment_entity_for_view(
        view: &LibraryViewState,
    ) -> Option<protocol::LibraryEnrichmentEntity> {
        match view {
            LibraryViewState::ArtistDetail { artist } => {
                Some(protocol::LibraryEnrichmentEntity::Artist {
                    artist: artist.clone(),
                })
            }
            LibraryViewState::AlbumDetail {
                album,
                album_artist,
            } => Some(protocol::LibraryEnrichmentEntity::Album {
                album: album.clone(),
                album_artist: album_artist.clone(),
            }),
            _ => None,
        }
    }

    fn artist_enrichment_image_path(&self, artist: &str) -> Option<PathBuf> {
        let key = protocol::LibraryEnrichmentEntity::Artist {
            artist: artist.to_string(),
        };
        self.library_enrichment.get(&key).and_then(|payload| {
            (payload.status == protocol::LibraryEnrichmentStatus::Ready)
                .then(|| {
                    payload
                        .image_path
                        .as_ref()
                        .filter(|path| path.exists())
                        .cloned()
                })
                .flatten()
        })
    }

    fn retryable_enrichment_error_kind(
        payload: &protocol::LibraryEnrichmentPayload,
    ) -> Option<protocol::LibraryEnrichmentErrorKind> {
        if payload.status != protocol::LibraryEnrichmentStatus::Error {
            return None;
        }
        match payload.error_kind {
            Some(protocol::LibraryEnrichmentErrorKind::Timeout)
            | Some(protocol::LibraryEnrichmentErrorKind::RateLimited)
            | Some(protocol::LibraryEnrichmentErrorKind::BudgetExhausted) => payload.error_kind,
            _ => None,
        }
    }

    fn is_retryable_enrichment_result(payload: &protocol::LibraryEnrichmentPayload) -> bool {
        Self::retryable_enrichment_error_kind(payload).is_some()
    }

    fn clear_enrichment_retry_state(&mut self, entity: &protocol::LibraryEnrichmentEntity) {
        self.library_enrichment_retry_counts.remove(entity);
        self.library_enrichment_retry_not_before.remove(entity);
    }

    fn schedule_enrichment_retry_backoff(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
        payload: &protocol::LibraryEnrichmentPayload,
    ) {
        if !Self::is_retryable_enrichment_result(payload) {
            return;
        }

        let attempt = self
            .library_enrichment_retry_counts
            .entry(entity.clone())
            .and_modify(|value| *value = value.saturating_add(1))
            .or_insert(1);
        let exponent = (*attempt).saturating_sub(1).min(3);
        let delay = PREFETCH_RETRY_BASE_DELAY
            .checked_mul(1u32 << exponent)
            .unwrap_or(Duration::from_secs(4))
            .min(Duration::from_secs(4));
        self.library_enrichment_retry_not_before
            .insert(entity.clone(), Instant::now() + delay);
    }

    fn enrichment_retry_backoff_elapsed(&self, entity: &protocol::LibraryEnrichmentEntity) -> bool {
        self.library_enrichment_retry_not_before
            .get(entity)
            .is_none_or(|deadline| Instant::now() >= *deadline)
    }

    fn should_request_prefetch_for_entity(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
    ) -> bool {
        let Some(existing) = self.library_enrichment.get(entity).cloned() else {
            return true;
        };
        if existing.status == protocol::LibraryEnrichmentStatus::Ready {
            if matches!(entity, protocol::LibraryEnrichmentEntity::Artist { .. })
                && existing
                    .image_path
                    .as_ref()
                    .is_some_and(|path| !path.exists())
            {
                self.library_enrichment.remove(entity);
                return true;
            }
            return false;
        }
        if existing.status == protocol::LibraryEnrichmentStatus::Disabled
            || (existing.status == protocol::LibraryEnrichmentStatus::NotFound
                && existing.attempt_kind == protocol::LibraryEnrichmentAttemptKind::Detail)
        {
            return false;
        }

        if !Self::is_retryable_enrichment_result(&existing) {
            return false;
        }

        let attempts = self
            .library_enrichment_retry_counts
            .get(entity)
            .copied()
            .unwrap_or(0);
        if attempts >= PREFETCH_RETRY_MAX_ATTEMPTS {
            debug!(
                "Enrichment prefetch retry capped for {:?} after {} attempts",
                entity, attempts
            );
            return false;
        }
        if self.enrichment_retry_backoff_elapsed(entity) {
            self.library_enrichment.remove(entity);
            return true;
        }
        false
    }

    fn request_enrichment(
        &mut self,
        entity: protocol::LibraryEnrichmentEntity,
        priority: protocol::LibraryEnrichmentPriority,
    ) {
        if !self.library_online_metadata_enabled {
            return;
        }
        if self
            .library_enrichment
            .get(&entity)
            .is_some_and(|payload| payload.status == protocol::LibraryEnrichmentStatus::Disabled)
        {
            self.library_enrichment.remove(&entity);
        }
        if let Some(existing) = self.library_enrichment.get(&entity).cloned() {
            let should_clear_cached = (Self::is_retryable_enrichment_result(&existing)
                && self.enrichment_retry_backoff_elapsed(&entity))
                || (priority == protocol::LibraryEnrichmentPriority::Interactive
                    && existing.status == protocol::LibraryEnrichmentStatus::NotFound
                    && existing.attempt_kind != protocol::LibraryEnrichmentAttemptKind::Detail);
            if should_clear_cached {
                self.library_enrichment.remove(&entity);
            }
        }

        if self.library_enrichment_pending.contains(&entity) {
            if priority == protocol::LibraryEnrichmentPriority::Interactive {
                let should_retry = self
                    .library_enrichment_last_request_at
                    .get(&entity)
                    .is_none_or(|requested_at| {
                        requested_at.elapsed() >= DETAIL_ENRICHMENT_RETRY_INTERVAL
                    });
                if should_retry {
                    let _ = self.bus_sender.send(protocol::Message::Library(
                        protocol::LibraryMessage::RequestEnrichment {
                            entity: entity.clone(),
                            priority,
                        },
                    ));
                    self.library_enrichment_last_request_at
                        .insert(entity, Instant::now());
                }
            }
            return;
        }
        if self.library_enrichment.contains_key(&entity) {
            return;
        }

        if priority == protocol::LibraryEnrichmentPriority::Interactive {
            self.library_enrichment_pending.insert(entity.clone());
        }
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::RequestEnrichment {
                entity: entity.clone(),
                priority,
            },
        ));
        if priority == protocol::LibraryEnrichmentPriority::Interactive {
            self.library_enrichment_last_request_at
                .insert(entity, Instant::now());
        }
    }

    fn retain_pending_detail_entity_only(&mut self) {
        self.library_enrichment_pending
            .retain(|entity| self.library_last_detail_enrichment_entity.as_ref() == Some(entity));
        self.library_enrichment_last_request_at
            .retain(|entity, _| self.library_enrichment_pending.contains(entity));
    }

    fn replace_prefetch_queue_if_changed(
        &mut self,
        entities: Vec<protocol::LibraryEnrichmentEntity>,
    ) {
        if self.library_last_prefetch_entities == entities {
            return;
        }
        self.library_last_prefetch_entities = entities.clone();
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::ReplaceEnrichmentPrefetchQueue { entities },
        ));
    }

    fn replace_background_queue_if_changed(
        &mut self,
        entities: Vec<protocol::LibraryEnrichmentEntity>,
    ) {
        if self.library_last_background_entities == entities {
            return;
        }
        self.library_last_background_entities = entities.clone();
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::ReplaceEnrichmentBackgroundQueue { entities },
        ));
    }

    fn maybe_retry_stalled_detail_enrichment(&mut self) {
        let view = self.current_library_view();
        let Some(entity) = Self::detail_enrichment_entity_for_view(&view) else {
            return;
        };
        self.request_enrichment(entity, protocol::LibraryEnrichmentPriority::Interactive);
    }

    fn request_current_detail_enrichment_if_needed(&mut self, view: &LibraryViewState) {
        let Some(entity) = Self::detail_enrichment_entity_for_view(view) else {
            self.library_last_detail_enrichment_entity = None;
            return;
        };

        let is_new_detail_entity =
            self.library_last_detail_enrichment_entity.as_ref() != Some(&entity);
        if is_new_detail_entity {
            let should_refresh = self.library_enrichment.get(&entity).is_some_and(|payload| {
                payload.status == protocol::LibraryEnrichmentStatus::NotFound
                    || payload.status == protocol::LibraryEnrichmentStatus::Error
            });
            if should_refresh {
                self.library_enrichment.remove(&entity);
            }
        }
        self.library_last_detail_enrichment_entity = Some(entity.clone());
        self.request_enrichment(entity, protocol::LibraryEnrichmentPriority::Interactive);
    }

    fn should_request_background_warm_for_entity(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
    ) -> bool {
        let Some(existing) = self.library_enrichment.get(entity).cloned() else {
            return true;
        };
        if existing.status == protocol::LibraryEnrichmentStatus::Disabled {
            return false;
        }
        if existing.status == protocol::LibraryEnrichmentStatus::Ready
            && existing
                .image_path
                .as_ref()
                .is_some_and(|path| path.exists())
        {
            return false;
        }
        if existing.status == protocol::LibraryEnrichmentStatus::Ready
            && existing
                .image_path
                .as_ref()
                .is_some_and(|path| !path.exists())
        {
            self.library_enrichment.remove(entity);
            return true;
        }
        if existing.status == protocol::LibraryEnrichmentStatus::NotFound
            && existing.attempt_kind == protocol::LibraryEnrichmentAttemptKind::Detail
        {
            return false;
        }
        if existing.status == protocol::LibraryEnrichmentStatus::Error
            && existing.error_kind == Some(protocol::LibraryEnrichmentErrorKind::Hard)
        {
            return false;
        }
        if !Self::is_retryable_enrichment_result(&existing) {
            return false;
        }
        let attempts = self
            .library_enrichment_retry_counts
            .get(entity)
            .copied()
            .unwrap_or(0);
        if attempts >= PREFETCH_RETRY_MAX_ATTEMPTS {
            return false;
        }
        if self.enrichment_retry_backoff_elapsed(entity) {
            self.library_enrichment.remove(entity);
            return true;
        }
        false
    }

    fn prefetch_artist_enrichment_window(&mut self, first_row: usize, row_count: usize) {
        if !matches!(self.current_library_view(), LibraryViewState::ArtistsRoot) {
            self.retain_pending_detail_entity_only();
            self.replace_prefetch_queue_if_changed(Vec::new());
            self.replace_background_queue_if_changed(Vec::new());
            return;
        }
        if !self.library_online_metadata_enabled {
            self.retain_pending_detail_entity_only();
            self.replace_prefetch_queue_if_changed(Vec::new());
            self.replace_background_queue_if_changed(Vec::new());
            return;
        }
        if row_count == 0 {
            // Viewport row count can transiently report zero during list/model churn.
            // Keep the last prefetch queue instead of clearing it, so loading keeps progressing.
            return;
        }

        let start = first_row
            .saturating_sub(LIBRARY_PREFETCH_TOP_OVERSCAN_ROWS)
            .min(self.library_view_indices.len());
        let end = first_row
            .saturating_add(row_count)
            .saturating_add(LIBRARY_PREFETCH_BOTTOM_OVERSCAN_ROWS)
            .min(self.library_view_indices.len());
        let mut prefetch_entities = Vec::new();
        let mut seen_entities = HashSet::new();
        let mut visible_artist_entities = HashSet::new();
        let mut ordered_artist_entities = Vec::new();
        let mut seen_all_artists = HashSet::new();
        for source_index in self.library_view_indices.iter().copied() {
            let Some(LibraryEntry::Artist(artist)) = self.library_entries.get(source_index) else {
                continue;
            };
            let entity = protocol::LibraryEnrichmentEntity::Artist {
                artist: artist.artist.clone(),
            };
            if seen_all_artists.insert(entity.clone()) {
                ordered_artist_entities.push(entity);
            }
        }
        for row in start..end {
            let Some(source_index) = self.library_view_indices.get(row).copied() else {
                continue;
            };
            let Some(LibraryEntry::Artist(artist)) = self.library_entries.get(source_index) else {
                continue;
            };
            let entity = protocol::LibraryEnrichmentEntity::Artist {
                artist: artist.artist.clone(),
            };
            if !seen_entities.insert(entity.clone()) {
                continue;
            }
            visible_artist_entities.insert(entity.clone());
            if self.should_request_prefetch_for_entity(&entity) {
                prefetch_entities.push(entity);
            }
        }
        let mut background_entities = Vec::new();
        for entity in ordered_artist_entities {
            if background_entities.len() >= LIBRARY_BACKGROUND_WARM_QUEUE_SIZE {
                break;
            }
            if visible_artist_entities.contains(&entity) {
                continue;
            }
            if self.should_request_background_warm_for_entity(&entity) {
                background_entities.push(entity);
            }
        }

        self.library_enrichment_retry_counts
            .retain(|entity, _| match entity {
                protocol::LibraryEnrichmentEntity::Artist { .. } => {
                    visible_artist_entities.contains(entity)
                        || self.library_last_detail_enrichment_entity.as_ref() == Some(entity)
                }
                protocol::LibraryEnrichmentEntity::Album { .. } => true,
            });
        self.library_enrichment_retry_not_before
            .retain(|entity, _| match entity {
                protocol::LibraryEnrichmentEntity::Artist { .. } => {
                    visible_artist_entities.contains(entity)
                        || self.library_last_detail_enrichment_entity.as_ref() == Some(entity)
                }
                protocol::LibraryEnrichmentEntity::Album { .. } => true,
            });

        self.retain_pending_detail_entity_only();
        self.replace_prefetch_queue_if_changed(prefetch_entities);
        self.replace_background_queue_if_changed(background_entities);
    }

    fn on_enrichment_prefetch_tick(&mut self) {
        if !self.library_online_metadata_enabled || self.collection_mode != COLLECTION_MODE_LIBRARY
        {
            self.replace_prefetch_queue_if_changed(Vec::new());
            self.replace_background_queue_if_changed(Vec::new());
            return;
        }
        self.maybe_retry_stalled_detail_enrichment();

        if matches!(self.current_library_view(), LibraryViewState::ArtistsRoot) {
            let prefetch_row_count = if self.library_artist_prefetch_row_count == 0 {
                LIBRARY_PREFETCH_FALLBACK_VISIBLE_ROWS
            } else {
                self.library_artist_prefetch_row_count
            };
            self.prefetch_artist_enrichment_window(
                self.library_artist_prefetch_first_row,
                prefetch_row_count,
            );
        } else {
            self.prefetch_artist_enrichment_window(0, 0);
        }
    }

    fn refresh_visible_artist_rows(&mut self, artist_name: &str) -> bool {
        let view = self.current_library_view();
        if !matches!(
            view,
            LibraryViewState::ArtistsRoot | LibraryViewState::GlobalSearch
        ) {
            return false;
        }

        let Some(row_indices) = self.library_artist_row_indices.get(artist_name).cloned() else {
            return false;
        };
        let selected_set: HashSet<usize> = self.library_selected_indices.iter().copied().collect();
        let mut updates: Vec<(usize, LibraryRowPresentation)> = Vec::new();

        for row_index in row_indices {
            let Some(source_index) = self.library_view_indices.get(row_index).copied() else {
                continue;
            };
            let Some(LibraryEntry::Artist(artist)) = self.library_entries.get(source_index) else {
                continue;
            };
            let presentation = self.library_row_presentation_from_entry(
                &LibraryEntry::Artist(artist.clone()),
                &view,
                selected_set.contains(&source_index),
                true,
            );
            updates.push((row_index, presentation));
        }

        if updates.is_empty() {
            return false;
        }

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let current_model = ui.get_library_model();
            let Some(vec_model) = current_model
                .as_any()
                .downcast_ref::<VecModel<LibraryRowData>>()
            else {
                return;
            };

            for (row_index, presentation) in updates {
                if row_index < vec_model.row_count() {
                    let row_data = UiManager::library_row_data_from_presentation(presentation);
                    vec_model.set_row_data(row_index, row_data);
                }
            }
        });
        true
    }

    fn resolve_library_cover_art_path(&mut self, track_path: &PathBuf) -> Option<PathBuf> {
        if let Some(cached) = self.library_cover_art_paths.get(track_path) {
            if let Some(path) = cached {
                if Self::failed_cover_path(path) {
                    self.library_cover_art_paths
                        .insert(track_path.clone(), None);
                    return None;
                }
            } else if let Some(cached_embedded) =
                Self::embedded_art_cache_path_if_present(track_path)
            {
                self.library_cover_art_paths
                    .insert(track_path.clone(), Some(cached_embedded.clone()));
                return Some(cached_embedded);
            }
            return cached.clone();
        }
        let resolved = self
            .find_external_cover_art_cached(track_path)
            .or_else(|| Self::embedded_art_cache_path_if_present(track_path));
        self.library_cover_art_paths
            .insert(track_path.clone(), resolved.clone());
        resolved
    }

    fn update_or_replace_track_model(ui: &AppWindow, rows: Vec<TrackRowData>) {
        let current_model = ui.get_track_model();
        if let Some(vec_model) = current_model
            .as_any()
            .downcast_ref::<VecModel<TrackRowData>>()
        {
            vec_model.set_vec(rows);
            return;
        }
        ui.set_track_model(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    fn reset_library_selection(&mut self) {
        self.library_selected_indices.clear();
        self.library_selection_anchor = None;
    }

    fn update_or_replace_library_model(ui: &AppWindow, rows: Vec<LibraryRowData>) {
        let current_model = ui.get_library_model();
        if let Some(vec_model) = current_model
            .as_any()
            .downcast_ref::<VecModel<LibraryRowData>>()
        {
            if vec_model.row_count() == rows.len() {
                for (row_index, row_data) in rows.into_iter().enumerate() {
                    vec_model.set_row_data(row_index, row_data);
                }
            } else {
                vec_model.set_vec(rows);
            }
            return;
        }
        ui.set_library_model(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    fn library_row_data_from_presentation(entry: LibraryRowPresentation) -> LibraryRowData {
        let album_art = entry
            .cover_art_path
            .as_deref()
            .and_then(UiManager::try_load_cover_art_image);
        let has_album_art = album_art.is_some();
        LibraryRowData {
            leading: entry.leading.into(),
            primary: entry.primary.into(),
            secondary: entry.secondary.into(),
            item_kind: entry.item_kind,
            album_art: album_art.unwrap_or_default(),
            has_album_art,
            is_playing: entry.is_playing,
            selected: entry.selected,
        }
    }

    fn sync_library_selection_to_ui(&self) {
        let view_indices = self.library_view_indices.clone();
        let selected_set: HashSet<usize> = self.library_selected_indices.iter().copied().collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let current_model = ui.get_library_model();
            let Some(vec_model) = current_model
                .as_any()
                .downcast_ref::<VecModel<LibraryRowData>>()
            else {
                return;
            };
            if vec_model.row_count() != view_indices.len() {
                return;
            }

            for (row_index, source_index) in view_indices.iter().enumerate() {
                let Some(mut row_data) = vec_model.row_data(row_index) else {
                    continue;
                };
                let selected = selected_set.contains(source_index);
                if row_data.selected == selected {
                    continue;
                }
                row_data.selected = selected;
                vec_model.set_row_data(row_index, row_data);
            }
        });
    }

    fn sync_library_add_to_playlist_ui(&self) {
        let labels: Vec<slint::SharedString> = self
            .playlist_names
            .iter()
            .map(|name| name.as_str().into())
            .collect();
        let checked = if self.library_add_to_playlist_checked.len() == labels.len() {
            self.library_add_to_playlist_checked.clone()
        } else {
            vec![false; labels.len()]
        };
        let selected_count = self.library_selected_indices.len() as i32;
        let confirm_enabled = selected_count > 0 && checked.iter().any(|value| *value);
        let dialog_visible = self.library_add_to_dialog_visible;

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_add_to_playlist_labels(ModelRc::from(Rc::new(VecModel::from(labels))));
            ui.set_library_add_to_playlist_checked(ModelRc::from(Rc::new(VecModel::from(checked))));
            ui.set_library_add_to_dialog_visible(dialog_visible);
            ui.set_library_selected_count(selected_count);
            ui.set_library_add_to_confirm_enabled(confirm_enabled);
        });
    }

    fn select_library_list_item(
        &mut self,
        index: usize,
        ctrl: bool,
        shift: bool,
        context_click: bool,
    ) {
        let Some(source_index) = self.map_library_view_to_source_index(index) else {
            return;
        };

        if context_click {
            if !self.library_selected_indices.contains(&source_index) {
                self.library_selected_indices.clear();
                self.library_selected_indices.push(source_index);
            }
            self.library_selection_anchor = Some(source_index);
        } else if shift {
            let range = Self::build_shift_selection_from_view_order(
                &self.library_view_indices,
                self.library_selection_anchor,
                source_index,
            );
            if ctrl {
                for selected in range {
                    if !self.library_selected_indices.contains(&selected) {
                        self.library_selected_indices.push(selected);
                    }
                }
            } else {
                self.library_selected_indices = range;
            }
        } else if ctrl {
            if let Some(existing) = self
                .library_selected_indices
                .iter()
                .position(|selected| *selected == source_index)
            {
                self.library_selected_indices.remove(existing);
            } else {
                self.library_selected_indices.push(source_index);
            }
            self.library_selection_anchor = Some(source_index);
        } else {
            self.library_selected_indices.clear();
            self.library_selected_indices.push(source_index);
            self.library_selection_anchor = Some(source_index);
        }

        self.library_selected_indices.sort_unstable();
        self.library_selected_indices.dedup();
        if self.library_selected_indices.is_empty() {
            self.library_selection_anchor = None;
        }

        let playing_track_path = self.current_playing_track_path.clone();
        let playing_track_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );

        self.sync_library_selection_to_ui();
        self.sync_library_add_to_playlist_ui();
        self.sync_properties_action_state();
    }

    fn build_library_selection_specs(&self) -> Vec<protocol::LibrarySelectionSpec> {
        Self::build_library_selection_specs_for_entries(
            &self.library_entries,
            &self.library_selected_indices,
        )
    }

    fn build_library_selection_specs_for_entries(
        entries: &[LibraryEntry],
        selected_indices: &[usize],
    ) -> Vec<protocol::LibrarySelectionSpec> {
        let mut specs = Vec::new();
        let mut seen = HashSet::new();
        for index in selected_indices {
            let Some(entry) = entries.get(*index) else {
                continue;
            };
            let (key, spec) = match entry {
                LibraryEntry::Song(song) => (
                    format!("song:{}", song.path.to_string_lossy()),
                    protocol::LibrarySelectionSpec::Song {
                        path: song.path.clone(),
                    },
                ),
                LibraryEntry::Artist(artist) => (
                    format!("artist:{}", artist.artist),
                    protocol::LibrarySelectionSpec::Artist {
                        artist: artist.artist.clone(),
                    },
                ),
                LibraryEntry::Album(album) => (
                    format!("album:{}\u{001f}{}", album.album, album.album_artist),
                    protocol::LibrarySelectionSpec::Album {
                        album: album.album.clone(),
                        album_artist: album.album_artist.clone(),
                    },
                ),
                LibraryEntry::Genre(genre) => (
                    format!("genre:{}", genre.genre),
                    protocol::LibrarySelectionSpec::Genre {
                        genre: genre.genre.clone(),
                    },
                ),
                LibraryEntry::Decade(decade) => (
                    format!("decade:{}", decade.decade),
                    protocol::LibrarySelectionSpec::Decade {
                        decade: decade.decade.clone(),
                    },
                ),
            };
            if seen.insert(key) {
                specs.push(spec);
            }
        }
        specs
    }

    fn prepare_library_add_to_playlists(&mut self) {
        if self.library_selected_indices.is_empty() {
            self.library_status_text = "Select at least one library item.".to_string();
            self.show_library_toast("Select at least one library item.");
            self.sync_library_ui();
            return;
        }
        if self.playlist_ids.is_empty() {
            self.library_status_text = "No playlists available for Add To.".to_string();
            self.show_library_toast("No playlists available for Add To.");
            self.sync_library_ui();
            return;
        }
        self.library_add_to_playlist_checked = vec![false; self.playlist_ids.len()];
        self.library_add_to_dialog_visible = true;
        self.sync_library_add_to_playlist_ui();
    }

    fn toggle_library_add_to_playlist(&mut self, index: usize) {
        if index >= self.library_add_to_playlist_checked.len() {
            return;
        }
        self.library_add_to_playlist_checked[index] = !self.library_add_to_playlist_checked[index];
        self.sync_library_add_to_playlist_ui();
    }

    fn confirm_library_add_to_playlists(&mut self) {
        if self.library_selected_indices.is_empty() {
            self.library_add_to_dialog_visible = false;
            self.sync_library_add_to_playlist_ui();
            return;
        }

        let playlist_ids: Vec<String> = self
            .library_add_to_playlist_checked
            .iter()
            .enumerate()
            .filter_map(|(index, selected)| {
                if *selected {
                    self.playlist_ids.get(index).cloned()
                } else {
                    None
                }
            })
            .collect();
        if playlist_ids.is_empty() {
            self.library_status_text = "Select at least one target playlist.".to_string();
            self.show_library_toast("Select at least one target playlist.");
            self.sync_library_ui();
            return;
        }

        let selections = self.build_library_selection_specs();
        if selections.is_empty() {
            self.library_status_text = "No library items selected.".to_string();
            self.show_library_toast("No library items selected.");
            self.sync_library_ui();
            self.library_add_to_dialog_visible = false;
            self.sync_library_add_to_playlist_ui();
            return;
        }

        self.library_add_to_dialog_visible = false;
        self.sync_library_add_to_playlist_ui();
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::AddSelectionToPlaylists {
                selections,
                playlist_ids,
            },
        ));
    }

    fn cancel_library_add_to_playlists(&mut self) {
        self.library_add_to_dialog_visible = false;
        self.sync_library_add_to_playlist_ui();
    }

    fn show_library_toast(&mut self, message: impl Into<String>) {
        let message = message.into();
        if message.trim().is_empty() {
            return;
        }

        self.library_toast_generation = self.library_toast_generation.wrapping_add(1);
        let generation = self.library_toast_generation;
        let toast_message = message.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_toast_text(toast_message.into());
            ui.set_library_toast_visible(true);
        });

        let bus_sender = self.bus_sender.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(2200));
            let _ = bus_sender.send(protocol::Message::Library(
                protocol::LibraryMessage::ToastTimeout { generation },
            ));
        });
    }

    fn hide_library_toast(&self) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_toast_visible(false);
        });
    }

    fn library_row_presentation_from_entry(
        &mut self,
        entry: &LibraryEntry,
        view: &LibraryViewState,
        selected: bool,
        resolve_cover_art: bool,
    ) -> LibraryRowPresentation {
        let compact_song_row_view = matches!(
            view,
            LibraryViewState::AlbumDetail { .. } | LibraryViewState::ArtistDetail { .. }
        );
        let global_search_view = matches!(view, LibraryViewState::GlobalSearch);
        match entry {
            LibraryEntry::Song(song) => LibraryRowPresentation {
                leading: if compact_song_row_view {
                    Self::library_track_number_leading(&song.track_number)
                } else {
                    String::new()
                },
                primary: song.title.clone(),
                secondary: if compact_song_row_view {
                    song.artist.clone()
                } else if global_search_view {
                    format!("Song • {} • {}", song.artist, song.album)
                } else {
                    format!("{} • {}", song.artist, song.album)
                },
                item_kind: LIBRARY_ITEM_KIND_SONG,
                cover_art_path: if compact_song_row_view || !resolve_cover_art {
                    None
                } else {
                    self.resolve_library_cover_art_path(&song.path)
                },
                is_playing: self.current_playing_track_path.as_ref() == Some(&song.path),
                selected,
            },
            LibraryEntry::Artist(artist) => LibraryRowPresentation {
                leading: String::new(),
                primary: artist.artist.clone(),
                secondary: if global_search_view {
                    format!(
                        "Artist • {} albums • {} songs",
                        artist.album_count, artist.song_count
                    )
                } else {
                    format!(
                        "{} albums • {} songs",
                        artist.album_count, artist.song_count
                    )
                },
                item_kind: LIBRARY_ITEM_KIND_ARTIST,
                cover_art_path: if resolve_cover_art {
                    self.artist_enrichment_image_path(&artist.artist)
                } else {
                    None
                },
                is_playing: false,
                selected,
            },
            LibraryEntry::Album(album) => LibraryRowPresentation {
                leading: String::new(),
                primary: album.album.clone(),
                secondary: if global_search_view {
                    format!(
                        "Album • {} • {} songs",
                        album.album_artist, album.song_count
                    )
                } else {
                    format!("{} • {} songs", album.album_artist, album.song_count)
                },
                item_kind: LIBRARY_ITEM_KIND_ALBUM,
                cover_art_path: if resolve_cover_art {
                    album
                        .representative_track_path
                        .as_ref()
                        .and_then(|track_path| self.resolve_library_cover_art_path(track_path))
                } else {
                    None
                },
                is_playing: false,
                selected,
            },
            LibraryEntry::Genre(genre) => LibraryRowPresentation {
                leading: String::new(),
                primary: genre.genre.clone(),
                secondary: format!("{} songs", genre.song_count),
                item_kind: LIBRARY_ITEM_KIND_GENRE,
                cover_art_path: None,
                is_playing: false,
                selected,
            },
            LibraryEntry::Decade(decade) => LibraryRowPresentation {
                leading: String::new(),
                primary: decade.decade.clone(),
                secondary: format!("{} songs", decade.song_count),
                item_kind: LIBRARY_ITEM_KIND_DECADE,
                cover_art_path: None,
                is_playing: false,
                selected,
            },
        }
    }

    fn library_cover_decode_window(&self, total_rows: usize) -> (usize, usize) {
        if total_rows == 0 {
            return (0, 0);
        }
        let default_rows = 24usize;
        let lookahead_rows = 24usize;
        let row_count = if self.library_artist_prefetch_row_count == 0 {
            default_rows
        } else {
            self.library_artist_prefetch_row_count.min(128)
        };
        let start = self
            .library_artist_prefetch_first_row
            .saturating_sub(6)
            .min(total_rows);
        let end = start
            .saturating_add(row_count)
            .saturating_add(lookahead_rows)
            .min(total_rows);
        (start, end)
    }

    fn sync_library_ui(&mut self) {
        let view = self.current_library_view();
        let (title, subtitle) = Self::library_view_labels(&view);
        let detail_header_visible = matches!(
            view,
            LibraryViewState::AlbumDetail { .. } | LibraryViewState::ArtistDetail { .. }
        );
        self.request_current_detail_enrichment_if_needed(&view);
        let can_go_back = self.library_view_stack.len() > 1;
        let root_index = self.current_library_root_index();
        let scan_in_progress = self.library_scan_in_progress;
        let status_text = self.library_status_text.clone();
        let entries = self.library_entries.clone();
        self.library_view_indices = if matches!(view, LibraryViewState::GlobalSearch)
            && self.library_search_query.trim().is_empty()
        {
            Vec::new()
        } else {
            Self::build_library_view_indices_for_query(&entries, &self.library_search_query)
        };
        let library_view_indices = self.library_view_indices.clone();
        let (cover_decode_start, cover_decode_end) =
            self.library_cover_decode_window(library_view_indices.len());
        let allow_cover_decode_for_all = detail_header_visible;
        let selected_set: HashSet<usize> = self.library_selected_indices.iter().copied().collect();
        let detail_song_count = if detail_header_visible {
            entries
                .iter()
                .filter(|entry| matches!(entry, LibraryEntry::Song(_)))
                .count()
        } else {
            0
        };
        let album_year = if matches!(view, LibraryViewState::AlbumDetail { .. }) {
            entries.iter().find_map(|entry| {
                if let LibraryEntry::Song(song) = entry {
                    let year = song.year.trim();
                    if year.is_empty() {
                        None
                    } else {
                        Some(year.to_string())
                    }
                } else {
                    None
                }
            })
        } else {
            None
        };
        let detail_album_count = if matches!(view, LibraryViewState::ArtistDetail { .. }) {
            entries
                .iter()
                .filter(|entry| matches!(entry, LibraryEntry::Album(_)))
                .count()
        } else {
            0
        };
        let detail_header_meta = match &view {
            LibraryViewState::AlbumDetail { .. } => {
                let songs_label = if detail_song_count == 1 {
                    "song"
                } else {
                    "songs"
                };
                if let Some(year) = album_year {
                    format!("{} • {} {}", year, detail_song_count, songs_label)
                } else {
                    format!("{} {}", detail_song_count, songs_label)
                }
            }
            LibraryViewState::ArtistDetail { .. } => {
                let albums_label = if detail_album_count == 1 {
                    "album"
                } else {
                    "albums"
                };
                let songs_label = if detail_song_count == 1 {
                    "song"
                } else {
                    "songs"
                };
                format!(
                    "{} {} • {} {}",
                    detail_album_count, albums_label, detail_song_count, songs_label
                )
            }
            _ => String::new(),
        };
        let detail_entity = Self::detail_enrichment_entity_for_view(&view);
        let mut detail_header_blurb = String::new();
        let mut detail_header_source_name = String::new();
        let mut detail_header_source_url = String::new();
        let mut detail_header_source_visible = false;
        let mut detail_header_loading = false;
        let mut detail_header_art_path = if matches!(view, LibraryViewState::AlbumDetail { .. }) {
            entries.iter().find_map(|entry| {
                if let LibraryEntry::Song(song) = entry {
                    self.resolve_library_cover_art_path(&song.path)
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(entity) = detail_entity {
            if let Some(payload) = self.library_enrichment.get(&entity) {
                match payload.status {
                    protocol::LibraryEnrichmentStatus::Ready => {
                        detail_header_blurb = payload.blurb.clone();
                        detail_header_source_name = payload.source_name.clone();
                        detail_header_source_url = payload.source_url.clone();
                        detail_header_source_visible =
                            !payload.source_name.is_empty() && !payload.source_url.is_empty();
                        if matches!(entity, protocol::LibraryEnrichmentEntity::Artist { .. }) {
                            detail_header_art_path = payload.image_path.clone();
                        }
                    }
                    protocol::LibraryEnrichmentStatus::NotFound => {
                        detail_header_blurb =
                            "No matching internet metadata found for this item.".to_string();
                    }
                    protocol::LibraryEnrichmentStatus::Disabled => {
                        detail_header_blurb =
                            "Internet metadata is disabled in Settings.".to_string();
                    }
                    protocol::LibraryEnrichmentStatus::Error => {
                        if matches!(
                            payload.error_kind,
                            Some(protocol::LibraryEnrichmentErrorKind::Timeout)
                                | Some(protocol::LibraryEnrichmentErrorKind::RateLimited)
                                | Some(protocol::LibraryEnrichmentErrorKind::BudgetExhausted)
                        ) {
                            detail_header_loading = true;
                        } else {
                            detail_header_blurb = if payload.blurb.trim().is_empty() {
                                "Could not load internet metadata right now.".to_string()
                            } else {
                                payload.blurb.clone()
                            };
                        }
                    }
                }
            } else {
                detail_header_loading = self.library_online_metadata_enabled;
            }
        }
        let mut artist_row_indices: HashMap<String, Vec<usize>> = HashMap::new();
        let rows: Vec<LibraryRowPresentation> = library_view_indices
            .iter()
            .enumerate()
            .filter_map(|(row_index, source_index)| {
                entries.get(*source_index).map(|entry| {
                    if let LibraryEntry::Artist(artist) = entry {
                        artist_row_indices
                            .entry(artist.artist.clone())
                            .or_default()
                            .push(row_index);
                    }
                    let resolve_cover_art = matches!(entry, LibraryEntry::Artist(_))
                        || allow_cover_decode_for_all
                        || (row_index >= cover_decode_start && row_index < cover_decode_end);
                    self.library_row_presentation_from_entry(
                        entry,
                        &view,
                        selected_set.contains(source_index),
                        resolve_cover_art,
                    )
                })
            })
            .collect();
        let mut scroll_restore_row: Option<i32> = None;
        if let Some(saved_row) = self.pending_library_scroll_restore_row {
            if !rows.is_empty() {
                let clamped_row = saved_row.min(rows.len().saturating_sub(1));
                scroll_restore_row = Some(clamped_row as i32);
                self.pending_library_scroll_restore_row = None;
                self.library_scroll_restore_token =
                    self.library_scroll_restore_token.wrapping_add(1);
            }
        }
        let scroll_restore_token = self.library_scroll_restore_token;
        self.library_artist_row_indices = artist_row_indices;
        let collection_mode = self.collection_mode;
        let online_prompt_visible = self.collection_mode == COLLECTION_MODE_LIBRARY
            && self.library_online_metadata_prompt_pending;

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let rows: Vec<LibraryRowData> = rows
                .into_iter()
                .map(UiManager::library_row_data_from_presentation)
                .collect();
            ui.set_collection_mode(collection_mode);
            ui.set_library_root_index(root_index);
            ui.set_library_view_title(title.into());
            ui.set_library_view_subtitle(subtitle.into());
            ui.set_library_album_header_visible(detail_header_visible);
            ui.set_library_can_go_back(can_go_back);
            ui.set_library_scan_in_progress(scan_in_progress);
            ui.set_library_status_text(status_text.into());
            let album_header_art = detail_header_art_path
                .as_deref()
                .and_then(UiManager::try_load_cover_art_image);
            ui.set_library_album_header_has_art(album_header_art.is_some());
            ui.set_library_album_header_art(album_header_art.unwrap_or_default());
            ui.set_library_album_header_meta(detail_header_meta.into());
            ui.set_library_detail_header_blurb(detail_header_blurb.into());
            ui.set_library_detail_header_source_name(detail_header_source_name.into());
            ui.set_library_detail_header_source_url(detail_header_source_url.into());
            ui.set_library_detail_header_source_visible(detail_header_source_visible);
            ui.set_library_detail_header_loading(detail_header_loading);
            ui.set_library_online_prompt_visible(online_prompt_visible);
            Self::update_or_replace_library_model(&ui, rows);
        });
        if let Some(restore_row) = scroll_restore_row {
            let ui_handle = self.ui.clone();
            let _ = self.ui.upgrade_in_event_loop(move |_| {
                let deferred_ui_handle_late = ui_handle.clone();
                slint::Timer::single_shot(Duration::from_millis(16), move || {
                    if let Some(ui) = deferred_ui_handle_late.upgrade() {
                        ui.set_library_scroll_target_row(restore_row);
                        ui.set_library_scroll_restore_token(scroll_restore_token);
                    }
                });
            });
        }
        self.sync_library_search_state_to_ui();
        self.sync_properties_action_state();
    }

    fn set_collection_mode(&mut self, mode: i32) {
        let normalized_mode = if mode == COLLECTION_MODE_LIBRARY {
            COLLECTION_MODE_LIBRARY
        } else {
            COLLECTION_MODE_PLAYLIST
        };
        if self.collection_mode == normalized_mode {
            return;
        }
        self.collection_mode = normalized_mode;
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.request_library_view_data();
        } else {
            self.library_add_to_dialog_visible = false;
            self.sync_library_add_to_playlist_ui();
        }
        let playing_track_path = self.current_playing_track_path.clone();
        let playing_track_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );
        self.sync_library_ui();
    }

    fn prepare_library_view_transition(&mut self) {
        self.library_entries.clear();
        self.library_view_indices.clear();
        self.reset_library_page_state();
        self.library_artist_row_indices.clear();
        self.reset_library_selection();
        self.library_add_to_dialog_visible = false;
        self.replace_prefetch_queue_if_changed(Vec::new());
        self.replace_background_queue_if_changed(Vec::new());
        self.sync_library_add_to_playlist_ui();
    }

    fn set_library_root_section(&mut self, section: i32) {
        self.remember_current_library_scroll_position();
        let root = match section {
            1 => LibraryViewState::ArtistsRoot,
            2 => LibraryViewState::AlbumsRoot,
            3 => LibraryViewState::GenresRoot,
            4 => LibraryViewState::DecadesRoot,
            _ => LibraryViewState::SongsRoot,
        };
        self.library_view_stack.clear();
        self.library_view_stack.push(root);
        self.library_artist_prefetch_first_row = 0;
        self.library_artist_prefetch_row_count = 0;
        self.pending_library_scroll_restore_row = None;
        self.prepare_library_view_transition();
        let playing_track_path = self.current_playing_track_path.clone();
        let playing_track_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );
        self.request_library_view_data();
        self.sync_library_ui();
    }

    fn navigate_library_back(&mut self) {
        if self.library_view_stack.len() <= 1 {
            return;
        }
        let current_view = self.current_library_view();
        self.remember_scroll_position_for_view(
            &current_view,
            self.library_artist_prefetch_first_row,
        );
        let popped = self.library_view_stack.pop();
        let target_view = self.current_library_view();
        let restoring_from_detail = matches!(
            popped,
            Some(
                LibraryViewState::ArtistDetail { .. }
                    | LibraryViewState::AlbumDetail { .. }
                    | LibraryViewState::GenreDetail { .. }
                    | LibraryViewState::DecadeDetail { .. }
            )
        );
        if restoring_from_detail {
            let restore_row = self
                .saved_scroll_position_for_view(&target_view)
                .unwrap_or(0);
            self.pending_library_scroll_restore_row = Some(restore_row);
            self.library_artist_prefetch_first_row = restore_row;
        } else {
            self.pending_library_scroll_restore_row = None;
            self.library_artist_prefetch_first_row = 0;
        }
        self.library_artist_prefetch_row_count = 0;
        self.prepare_library_view_transition();
        let playing_track_path = self.current_playing_track_path.clone();
        let playing_track_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );
        self.request_library_view_data();
        self.sync_library_ui();
    }

    fn library_page_query_for_view(view: &LibraryViewState) -> protocol::LibraryViewQuery {
        match view {
            LibraryViewState::SongsRoot => protocol::LibraryViewQuery::Songs,
            LibraryViewState::ArtistsRoot => protocol::LibraryViewQuery::Artists,
            LibraryViewState::AlbumsRoot => protocol::LibraryViewQuery::Albums,
            LibraryViewState::GenresRoot => protocol::LibraryViewQuery::Genres,
            LibraryViewState::DecadesRoot => protocol::LibraryViewQuery::Decades,
            LibraryViewState::GlobalSearch => protocol::LibraryViewQuery::GlobalSearch,
            LibraryViewState::ArtistDetail { artist } => protocol::LibraryViewQuery::ArtistDetail {
                artist: artist.clone(),
            },
            LibraryViewState::AlbumDetail {
                album,
                album_artist,
            } => protocol::LibraryViewQuery::AlbumDetail {
                album: album.clone(),
                album_artist: album_artist.clone(),
            },
            LibraryViewState::GenreDetail { genre } => protocol::LibraryViewQuery::GenreDetail {
                genre: genre.clone(),
            },
            LibraryViewState::DecadeDetail { decade } => protocol::LibraryViewQuery::DecadeDetail {
                decade: decade.clone(),
            },
        }
    }

    fn reset_library_page_state(&mut self) {
        self.library_page_view = None;
        self.library_page_next_offset = 0;
        self.library_page_total = 0;
        self.library_page_entries.clear();
        self.library_page_query.clear();
    }

    fn library_entry_from_payload(payload: protocol::LibraryEntryPayload) -> LibraryEntry {
        match payload {
            protocol::LibraryEntryPayload::Song(song) => LibraryEntry::Song(song),
            protocol::LibraryEntryPayload::Artist(artist) => LibraryEntry::Artist(artist),
            protocol::LibraryEntryPayload::Album(album) => LibraryEntry::Album(album),
            protocol::LibraryEntryPayload::Genre(genre) => LibraryEntry::Genre(genre),
            protocol::LibraryEntryPayload::Decade(decade) => LibraryEntry::Decade(decade),
        }
    }

    fn request_next_library_page(&mut self) {
        let Some(view) = self.library_page_view.clone() else {
            return;
        };
        let request_id = self.library_page_request_id;
        let offset = self.library_page_next_offset;
        let query = self.library_page_query.clone();
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::RequestLibraryPage {
                request_id,
                view,
                offset,
                limit: LIBRARY_PAGE_FETCH_LIMIT,
                query,
            },
        ));
    }

    fn handle_library_page_result(
        &mut self,
        request_id: u64,
        total: usize,
        entries: Vec<protocol::LibraryEntryPayload>,
    ) {
        if request_id != self.library_page_request_id || self.library_page_view.is_none() {
            return;
        }
        self.library_page_total = total;
        self.library_page_next_offset = self.library_page_next_offset.saturating_add(entries.len());
        self.library_page_entries.extend(entries);

        if self.library_page_next_offset < self.library_page_total {
            self.request_next_library_page();
            return;
        }

        let final_entries: Vec<LibraryEntry> = std::mem::take(&mut self.library_page_entries)
            .into_iter()
            .map(Self::library_entry_from_payload)
            .collect();
        self.reset_library_page_state();
        self.set_library_entries(final_entries);
    }

    fn request_library_view_data(&mut self) {
        let view = self.current_library_view();
        if matches!(view, LibraryViewState::GlobalSearch)
            && self.library_search_query.trim().is_empty()
        {
            self.reset_library_page_state();
            self.set_library_entries(Vec::new());
            return;
        }
        self.library_page_request_id = self.library_page_request_id.wrapping_add(1);
        self.library_page_view = Some(Self::library_page_query_for_view(&view));
        self.library_page_next_offset = 0;
        self.library_page_total = 0;
        self.library_page_entries.clear();
        self.library_page_query = if matches!(view, LibraryViewState::GlobalSearch) {
            self.library_search_query.clone()
        } else {
            String::new()
        };
        self.request_next_library_page();
    }

    fn set_library_entries(&mut self, entries: Vec<LibraryEntry>) {
        self.library_entries = entries;
        self.reset_library_selection();
        self.library_add_to_dialog_visible = false;
        self.sync_library_add_to_playlist_ui();
        let playing_track_path = self.current_playing_track_path.clone();
        let playing_track_metadata = self.current_playing_track_metadata.clone();
        self.update_display_for_active_collection(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );
        self.sync_library_ui();
    }

    fn handle_scan_status_message(&mut self, message: protocol::LibraryMessage) {
        match message {
            protocol::LibraryMessage::ScanStarted => {
                self.library_scan_in_progress = true;
                self.library_status_text = "Scanning library...".to_string();
                self.library_cover_art_paths.clear();
                self.folder_cover_art_paths.clear();
                self.library_enrichment_pending.clear();
                self.library_enrichment_last_request_at.clear();
                self.library_enrichment_retry_counts.clear();
                self.library_enrichment_retry_not_before.clear();
                self.replace_prefetch_queue_if_changed(Vec::new());
                self.replace_background_queue_if_changed(Vec::new());
                self.library_last_detail_enrichment_entity = None;
                self.library_add_to_dialog_visible = false;
                self.sync_library_add_to_playlist_ui();
                self.sync_library_ui();
            }
            protocol::LibraryMessage::ScanProgress {
                discovered,
                indexed,
                metadata_pending,
            } => {
                self.library_status_text = format!(
                    "Scanning library: discovered {}, indexed {}, metadata pending {}",
                    discovered, indexed, metadata_pending
                );
                self.sync_library_ui();
            }
            protocol::LibraryMessage::ScanCompleted { indexed_tracks } => {
                self.library_scan_in_progress = false;
                self.library_status_text = format!("Indexed {} tracks", indexed_tracks);
                self.library_cover_art_paths.clear();
                self.folder_cover_art_paths.clear();
                self.library_enrichment.clear();
                self.library_enrichment_pending.clear();
                self.library_enrichment_last_request_at.clear();
                self.library_enrichment_retry_counts.clear();
                self.library_enrichment_retry_not_before.clear();
                self.replace_prefetch_queue_if_changed(Vec::new());
                self.replace_background_queue_if_changed(Vec::new());
                self.library_last_detail_enrichment_entity = None;
                self.request_library_view_data();
                self.sync_library_ui();
            }
            protocol::LibraryMessage::MetadataBackfillProgress { updated, remaining } => {
                if remaining == 0 {
                    self.library_status_text =
                        format!("Metadata backfill complete ({} updated)", updated);
                } else {
                    self.library_status_text = format!(
                        "Metadata backfill: {} updated, {} remaining",
                        updated, remaining
                    );
                }
                self.request_library_view_data();
                self.sync_library_ui();
            }
            protocol::LibraryMessage::ScanFailed(error_text) => {
                self.library_scan_in_progress = false;
                self.library_status_text = error_text;
                self.sync_library_ui();
            }
            _ => {}
        }
    }

    fn drain_scan_progress_queue(&mut self) {
        while let Ok(message) = self.library_scan_progress_rx.try_recv() {
            self.handle_scan_status_message(message);
        }
    }

    fn play_library_song_from_entries(&mut self, selected_song_id: &str) {
        let songs: Vec<protocol::LibrarySong> = self
            .library_entries
            .iter()
            .filter_map(|entry| match entry {
                LibraryEntry::Song(song) => Some(song.clone()),
                _ => None,
            })
            .collect();
        if songs.is_empty() {
            return;
        }
        let Some(start_index) = songs.iter().position(|song| song.id == selected_song_id) else {
            return;
        };

        let tracks: Vec<protocol::RestoredTrack> = songs
            .into_iter()
            .map(|song| protocol::RestoredTrack {
                id: song.id,
                path: song.path,
            })
            .collect();

        let _ = self.bus_sender.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::PlayLibraryQueue {
                tracks,
                start_index,
            },
        ));
    }

    fn activate_library_item(&mut self, index: usize) {
        let Some(entry) = self.library_entries.get(index).cloned() else {
            return;
        };

        match entry {
            LibraryEntry::Song(song) => {
                self.play_library_song_from_entries(&song.id);
            }
            LibraryEntry::Artist(artist) => {
                self.remember_current_library_scroll_position();
                self.library_view_stack
                    .push(LibraryViewState::ArtistDetail {
                        artist: artist.artist,
                    });
                self.library_artist_prefetch_first_row = 0;
                self.library_artist_prefetch_row_count = 0;
                self.prepare_library_view_transition();
                self.request_library_view_data();
                self.sync_library_ui();
            }
            LibraryEntry::Album(album) => {
                self.remember_current_library_scroll_position();
                self.library_view_stack.push(LibraryViewState::AlbumDetail {
                    album: album.album,
                    album_artist: album.album_artist,
                });
                self.library_artist_prefetch_first_row = 0;
                self.library_artist_prefetch_row_count = 0;
                self.prepare_library_view_transition();
                self.request_library_view_data();
                self.sync_library_ui();
            }
            LibraryEntry::Genre(genre) => {
                self.remember_current_library_scroll_position();
                self.library_view_stack
                    .push(LibraryViewState::GenreDetail { genre: genre.genre });
                self.library_artist_prefetch_first_row = 0;
                self.library_artist_prefetch_row_count = 0;
                self.prepare_library_view_transition();
                self.request_library_view_data();
                self.sync_library_ui();
            }
            LibraryEntry::Decade(decade) => {
                self.remember_current_library_scroll_position();
                self.library_view_stack
                    .push(LibraryViewState::DecadeDetail {
                        decade: decade.decade,
                    });
                self.library_artist_prefetch_first_row = 0;
                self.library_artist_prefetch_row_count = 0;
                self.prepare_library_view_transition();
                self.request_library_view_data();
                self.sync_library_ui();
            }
        }
    }

    fn fallback_track_metadata(path: &Path) -> TrackMetadata {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();

        TrackMetadata {
            title: filename,
            artist: "".to_string(),
            album: "".to_string(),
            album_artist: "".to_string(),
            date: "".to_string(),
            year: "".to_string(),
            genre: "".to_string(),
            track_number: "".to_string(),
        }
    }

    fn read_track_metadata_summary(path: &Path) -> protocol::TrackMetadataSummary {
        if let Some(parsed) = metadata_tags::read_common_track_metadata(path) {
            if !parsed.title.is_empty() || !parsed.artist.is_empty() || !parsed.album.is_empty() {
                return protocol::TrackMetadataSummary {
                    title: parsed.title,
                    artist: parsed.artist,
                    album: parsed.album,
                    album_artist: parsed.album_artist,
                    date: parsed.date,
                    genre: parsed.genre,
                    year: parsed.year,
                    track_number: parsed.track_number,
                };
            }
        }

        let fallback = Self::fallback_track_metadata(path);
        protocol::TrackMetadataSummary {
            title: fallback.title,
            artist: fallback.artist,
            album: fallback.album,
            album_artist: fallback.album_artist,
            date: fallback.date,
            genre: fallback.genre,
            year: fallback.year,
            track_number: fallback.track_number,
        }
    }

    fn track_metadata_from_summary(summary: &protocol::TrackMetadataSummary) -> TrackMetadata {
        TrackMetadata {
            title: summary.title.clone(),
            artist: summary.artist.clone(),
            album: summary.album.clone(),
            album_artist: summary.album_artist.clone(),
            date: summary.date.clone(),
            year: summary.year.clone(),
            genre: summary.genre.clone(),
            track_number: summary.track_number.clone(),
        }
    }

    fn queue_track_metadata_lookup(&self, track_id: String, track_path: PathBuf) {
        let _ = self.metadata_lookup_tx.send(MetadataLookupRequest {
            track_id,
            track_path,
        });
    }

    fn queue_track_metadata_lookup_batch(&self, tracks: &[protocol::RestoredTrack]) {
        for track in tracks {
            self.queue_track_metadata_lookup(track.id.clone(), track.path.clone());
        }
    }

    fn apply_track_metadata_batch_updates(
        &mut self,
        updates: Vec<protocol::TrackMetadataPatch>,
    ) -> bool {
        if updates.is_empty() {
            return false;
        }
        let index_by_track_id: HashMap<String, usize> = self
            .track_ids
            .iter()
            .enumerate()
            .map(|(index, track_id)| (track_id.clone(), index))
            .collect();
        let mut changed = false;
        for update in updates {
            let Some(index) = index_by_track_id.get(&update.track_id).copied() else {
                continue;
            };
            let next = Self::track_metadata_from_summary(&update.summary);
            if let Some(current) = self.track_metadata.get_mut(index) {
                if current.title != next.title
                    || current.artist != next.artist
                    || current.album != next.album
                    || current.album_artist != next.album_artist
                    || current.date != next.date
                    || current.year != next.year
                    || current.genre != next.genre
                    || current.track_number != next.track_number
                {
                    *current = next;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Handles selection gesture start from the UI overlay.
    pub fn on_pointer_down(&mut self, pressed_index: usize, ctrl: bool, shift: bool) {
        debug!(
            "on_pointer_down: index={}, ctrl={}, shift={}",
            pressed_index, ctrl, shift
        );
        let Some(source_index) = self.map_view_to_source_index(pressed_index) else {
            self.pressed_index = None;
            return;
        };
        self.pressed_index = Some(source_index);

        let is_already_selected = self.selected_indices.contains(&source_index);
        if !is_already_selected || ctrl || shift {
            if shift && !self.view_indices.is_empty() {
                let shift_selected_indices = Self::build_shift_selection_from_view_order(
                    &self.view_indices,
                    self.selection_anchor_source_index(),
                    source_index,
                );
                let _ = self.bus_sender.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::SelectionChanged(shift_selected_indices),
                ));
            } else {
                let _ = self.bus_sender.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::SelectTrackMulti {
                        index: source_index,
                        ctrl,
                        shift,
                    },
                ));
            }
            if !shift {
                self.set_selection_anchor_from_source_index(source_index);
            }
        }
    }

    /// Starts drag state for track row reordering.
    pub fn on_drag_start(&mut self, pressed_index: usize) {
        if self.is_filter_view_active() {
            self.drag_indices.clear();
            self.is_dragging = false;
            return;
        }

        let Some(source_index) = self.map_view_to_source_index(pressed_index) else {
            return;
        };
        debug!(
            ">>> on_drag_start START: pressed_index={}, self.selected_indices={:?}",
            source_index, self.selected_indices
        );
        if self.selected_indices.contains(&source_index) {
            self.drag_indices = self.selected_indices.clone();
            debug!(
                ">>> MULTI-SELECT DRAG: using drag_indices {:?}",
                self.drag_indices
            );
        } else {
            self.drag_indices = vec![source_index];
            debug!(">>> SINGLE DRAG: using index {:?}", self.drag_indices);
        }
        self.drag_indices.sort();
        debug!(
            ">>> on_drag_start END: drag_indices={:?}",
            self.drag_indices
        );
        self.is_dragging = true;
    }

    /// Updates the visual drag target gap during row drag.
    pub fn on_drag_move(&mut self, drop_gap: usize) {
        if self.is_filter_view_active() {
            return;
        }
        if self.is_dragging {
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                ui.set_drop_index(drop_gap as i32);
            });
        }
    }

    /// Finalizes drag state and emits track reorder command when applicable.
    pub fn on_drag_end(&mut self, drop_gap: usize) {
        if self.is_filter_view_active() {
            self.drag_indices.clear();
            self.is_dragging = false;
            self.pressed_index = None;
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                ui.set_is_dragging(false);
                ui.set_drop_index(-1);
                ui.set_pressed_index(-1);
            });
            return;
        }

        debug!(
            ">>> on_drag_end START: is_dragging={}, drop_gap={}, drag_indices={:?}",
            self.is_dragging, drop_gap, self.drag_indices
        );
        if self.is_dragging && !self.drag_indices.is_empty() {
            let indices = self.drag_indices.clone();
            let to = drop_gap;
            debug!(
                ">>> on_drag_end SENDING ReorderTracks: indices={:?}, to={}",
                indices, to
            );

            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::ReorderTracks { indices, to },
            ));
        }

        self.drag_indices.clear();
        self.is_dragging = false;
        self.pressed_index = None;

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_is_dragging(false);
            ui.set_drop_index(-1);
            ui.set_pressed_index(-1);
        });
    }

    /// Starts the blocking UI event loop that listens for bus messages.
    pub fn run(&mut self) {
        self.sync_library_ui();
        self.sync_library_add_to_playlist_ui();
        self.sync_properties_action_state();
        self.sync_properties_dialog_ui();
        loop {
            self.drain_scan_progress_queue();
            match self.bus_receiver.blocking_recv() {
                Ok(message) => {
                    self.on_message_received();
                    match message {
                        protocol::Message::Library(library_message) => match library_message {
                            protocol::LibraryMessage::SetCollectionMode(mode) => {
                                self.set_collection_mode(mode);
                            }
                            protocol::LibraryMessage::SelectRootSection(section) => {
                                self.set_collection_mode(COLLECTION_MODE_LIBRARY);
                                self.set_library_root_section(section);
                            }
                            protocol::LibraryMessage::OpenGlobalSearch => {
                                self.open_global_library_search();
                            }
                            protocol::LibraryMessage::SelectListItem {
                                index,
                                ctrl,
                                shift,
                                context_click,
                            } => {
                                self.select_library_list_item(index, ctrl, shift, context_click);
                            }
                            protocol::LibraryMessage::NavigateBack => {
                                self.navigate_library_back();
                            }
                            protocol::LibraryMessage::ActivateListItem(index) => {
                                if let Some(source_index) =
                                    self.map_library_view_to_source_index(index)
                                {
                                    self.activate_library_item(source_index);
                                }
                            }
                            protocol::LibraryMessage::PrepareAddToPlaylists => {
                                self.prepare_library_add_to_playlists();
                            }
                            protocol::LibraryMessage::ToggleAddToPlaylist(index) => {
                                self.toggle_library_add_to_playlist(index);
                            }
                            protocol::LibraryMessage::ConfirmAddToPlaylists => {
                                self.confirm_library_add_to_playlists();
                            }
                            protocol::LibraryMessage::CancelAddToPlaylists => {
                                self.cancel_library_add_to_playlists();
                            }
                            protocol::LibraryMessage::OpenSearch => {
                                self.open_library_search();
                            }
                            protocol::LibraryMessage::CloseSearch => {
                                self.close_library_search();
                            }
                            protocol::LibraryMessage::SetSearchQuery(query) => {
                                self.set_library_search_query(query);
                            }
                            protocol::LibraryMessage::LibraryViewportChanged {
                                first_row,
                                row_count,
                            } => {
                                debug!(
                                    "Library viewport changed: first_row={}, row_count={}",
                                    first_row, row_count
                                );
                                self.library_artist_prefetch_first_row = first_row;
                                let current_view = self.current_library_view();
                                self.remember_scroll_position_for_view(&current_view, first_row);
                                if row_count > 0 {
                                    self.library_artist_prefetch_row_count = row_count;
                                }
                                let effective_row_count = if row_count > 0 {
                                    row_count
                                } else if self.library_artist_prefetch_row_count == 0 {
                                    LIBRARY_PREFETCH_FALLBACK_VISIBLE_ROWS
                                } else {
                                    self.library_artist_prefetch_row_count
                                };
                                self.prefetch_artist_enrichment_window(
                                    first_row,
                                    effective_row_count,
                                );
                            }
                            protocol::LibraryMessage::EnrichmentPrefetchTick => {
                                self.on_enrichment_prefetch_tick();
                            }
                            protocol::LibraryMessage::ScanStarted
                            | protocol::LibraryMessage::ScanProgress { .. }
                            | protocol::LibraryMessage::ScanCompleted { .. }
                            | protocol::LibraryMessage::MetadataBackfillProgress { .. }
                            | protocol::LibraryMessage::ScanFailed(_) => {
                                self.handle_scan_status_message(library_message);
                            }
                            protocol::LibraryMessage::SongsResult(songs) => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::SongsRoot
                                ) {
                                    self.set_library_entries(
                                        songs.into_iter().map(LibraryEntry::Song).collect(),
                                    );
                                }
                            }
                            protocol::LibraryMessage::ArtistsResult(artists) => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::ArtistsRoot
                                ) {
                                    self.set_library_entries(
                                        artists.into_iter().map(LibraryEntry::Artist).collect(),
                                    );
                                }
                            }
                            protocol::LibraryMessage::AlbumsResult(albums) => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::AlbumsRoot
                                ) {
                                    self.set_library_entries(
                                        albums.into_iter().map(LibraryEntry::Album).collect(),
                                    );
                                }
                            }
                            protocol::LibraryMessage::GenresResult(genres) => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::GenresRoot
                                ) {
                                    self.set_library_entries(
                                        genres.into_iter().map(LibraryEntry::Genre).collect(),
                                    );
                                }
                            }
                            protocol::LibraryMessage::DecadesResult(decades) => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::DecadesRoot
                                ) {
                                    self.set_library_entries(
                                        decades.into_iter().map(LibraryEntry::Decade).collect(),
                                    );
                                }
                            }
                            protocol::LibraryMessage::GlobalSearchDataResult {
                                songs,
                                artists,
                                albums,
                            } => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::GlobalSearch
                                ) {
                                    self.set_library_entries(Self::build_global_search_entries(
                                        songs, artists, albums,
                                    ));
                                }
                            }
                            protocol::LibraryMessage::ArtistDetailResult {
                                artist,
                                albums,
                                songs,
                            } => {
                                if let LibraryViewState::ArtistDetail {
                                    artist: requested_artist,
                                } = self.current_library_view()
                                {
                                    if requested_artist == artist {
                                        self.set_library_entries(
                                            Self::build_artist_detail_entries(albums, songs),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::AlbumSongsResult {
                                album,
                                album_artist,
                                songs,
                            } => {
                                if let LibraryViewState::AlbumDetail {
                                    album: requested_album,
                                    album_artist: requested_album_artist,
                                } = self.current_library_view()
                                {
                                    if requested_album == album
                                        && requested_album_artist == album_artist
                                    {
                                        self.set_library_entries(
                                            songs.into_iter().map(LibraryEntry::Song).collect(),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::GenreSongsResult { genre, songs } => {
                                if let LibraryViewState::GenreDetail {
                                    genre: requested_genre,
                                } = self.current_library_view()
                                {
                                    if requested_genre == genre {
                                        self.set_library_entries(
                                            songs.into_iter().map(LibraryEntry::Song).collect(),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::DecadeSongsResult { decade, songs } => {
                                if let LibraryViewState::DecadeDetail {
                                    decade: requested_decade,
                                } = self.current_library_view()
                                {
                                    if requested_decade == decade {
                                        self.set_library_entries(
                                            songs.into_iter().map(LibraryEntry::Song).collect(),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::LibraryPageResult {
                                request_id,
                                total,
                                entries,
                            } => {
                                self.handle_library_page_result(request_id, total, entries);
                            }
                            protocol::LibraryMessage::EnrichmentResult(payload) => {
                                self.library_enrichment_pending.remove(&payload.entity);
                                self.library_enrichment_last_request_at
                                    .remove(&payload.entity);
                                let entity = payload.entity.clone();
                                let view = self.current_library_view();
                                let is_current_detail_entity =
                                    Self::detail_enrichment_entity_for_view(&view)
                                        .as_ref()
                                        .is_some_and(|current| current == &entity);
                                let is_visible_prefetch_artist =
                                    matches!(
                                        &entity,
                                        protocol::LibraryEnrichmentEntity::Artist { .. }
                                    ) && self.library_last_prefetch_entities.contains(&entity);
                                if Self::is_retryable_enrichment_result(&payload)
                                    && (is_current_detail_entity || is_visible_prefetch_artist)
                                {
                                    self.schedule_enrichment_retry_backoff(&entity, &payload);
                                } else {
                                    self.clear_enrichment_retry_state(&entity);
                                }
                                self.library_enrichment.insert(entity.clone(), payload);

                                let mut updated = false;
                                if let protocol::LibraryEnrichmentEntity::Artist { artist } =
                                    &entity
                                {
                                    updated = self.refresh_visible_artist_rows(artist);
                                }

                                if Self::detail_enrichment_entity_for_view(&view)
                                    .as_ref()
                                    .is_some_and(|current| current == &entity)
                                {
                                    self.sync_library_ui();
                                    updated = true;
                                }

                                if !updated && matches!(view, LibraryViewState::GlobalSearch) {
                                    self.sync_library_search_state_to_ui();
                                }
                            }
                            protocol::LibraryMessage::EnrichmentCacheCleared {
                                cleared_rows,
                                deleted_images,
                            } => {
                                self.library_enrichment.clear();
                                self.library_enrichment_pending.clear();
                                self.library_enrichment_last_request_at.clear();
                                self.library_enrichment_retry_counts.clear();
                                self.library_enrichment_retry_not_before.clear();
                                self.replace_prefetch_queue_if_changed(Vec::new());
                                self.replace_background_queue_if_changed(Vec::new());
                                self.library_last_detail_enrichment_entity = None;
                                TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| {
                                    cache.borrow_mut().clear();
                                });
                                TRACK_ROW_COVER_ART_FAILED_PATHS.with(|failed| {
                                    failed.borrow_mut().clear();
                                });
                                let toast_text = format!(
                                    "Cleared internet metadata cache ({} entries, {} images)",
                                    cleared_rows, deleted_images
                                );
                                self.library_status_text = toast_text.clone();
                                self.show_library_toast(toast_text);
                                self.sync_library_ui();
                            }
                            protocol::LibraryMessage::AddToPlaylistsCompleted {
                                playlist_count,
                                track_count,
                            } => {
                                if self.pending_paste_feedback {
                                    self.pending_paste_feedback = false;
                                    let toast_text = format!("Pasted {} tracks", track_count);
                                    self.library_status_text = toast_text.clone();
                                    self.show_library_toast(toast_text);
                                } else {
                                    let toast_text = format!(
                                        "Added {} track(s) to {} playlist(s)",
                                        track_count, playlist_count
                                    );
                                    self.library_status_text = toast_text.clone();
                                    self.show_library_toast(toast_text);
                                }
                            }
                            protocol::LibraryMessage::AddToPlaylistsFailed(error_text) => {
                                let toast_text = if self.pending_paste_feedback {
                                    self.pending_paste_feedback = false;
                                    format!("Paste failed: {}", error_text)
                                } else {
                                    format!("Failed to add to playlists: {}", error_text)
                                };
                                self.library_status_text = toast_text.clone();
                                self.show_library_toast(toast_text);
                            }
                            protocol::LibraryMessage::ToastTimeout { generation } => {
                                if generation == self.library_toast_generation {
                                    self.hide_library_toast();
                                }
                            }
                            protocol::LibraryMessage::DrainScanProgressQueue => {
                                self.drain_scan_progress_queue();
                            }
                            protocol::LibraryMessage::RequestScan
                            | protocol::LibraryMessage::RequestSongs
                            | protocol::LibraryMessage::RequestArtists
                            | protocol::LibraryMessage::RequestAlbums
                            | protocol::LibraryMessage::RequestGenres
                            | protocol::LibraryMessage::RequestDecades
                            | protocol::LibraryMessage::RequestGlobalSearchData
                            | protocol::LibraryMessage::RequestArtistDetail { .. }
                            | protocol::LibraryMessage::RequestAlbumSongs { .. }
                            | protocol::LibraryMessage::RequestGenreSongs { .. }
                            | protocol::LibraryMessage::RequestDecadeSongs { .. }
                            | protocol::LibraryMessage::RequestLibraryPage { .. }
                            | protocol::LibraryMessage::RequestEnrichment { .. }
                            | protocol::LibraryMessage::ReplaceEnrichmentPrefetchQueue { .. }
                            | protocol::LibraryMessage::ReplaceEnrichmentBackgroundQueue {
                                ..
                            }
                            | protocol::LibraryMessage::ClearEnrichmentCache
                            | protocol::LibraryMessage::AddSelectionToPlaylists { .. } => {}
                        },
                        protocol::Message::Metadata(metadata_message) => match metadata_message {
                            protocol::MetadataMessage::OpenPropertiesForCurrentSelection => {
                                self.open_properties_for_current_selection();
                            }
                            protocol::MetadataMessage::EditPropertiesField { index, value } => {
                                self.edit_properties_field(index, value);
                            }
                            protocol::MetadataMessage::SaveProperties => {
                                self.save_properties();
                            }
                            protocol::MetadataMessage::CancelProperties => {
                                self.cancel_properties();
                            }
                            protocol::MetadataMessage::TrackPropertiesLoaded {
                                request_id,
                                path,
                                display_name,
                                fields,
                            } => {
                                self.handle_properties_loaded(
                                    request_id,
                                    path,
                                    display_name,
                                    fields,
                                );
                            }
                            protocol::MetadataMessage::TrackPropertiesLoadFailed {
                                request_id,
                                path,
                                error,
                            } => {
                                self.handle_properties_load_failed(request_id, path, error);
                            }
                            protocol::MetadataMessage::TrackPropertiesSaved {
                                request_id,
                                path,
                                summary,
                                db_sync_warning,
                            } => {
                                self.handle_properties_saved(
                                    request_id,
                                    path,
                                    summary,
                                    db_sync_warning,
                                );
                            }
                            protocol::MetadataMessage::TrackPropertiesSaveFailed {
                                request_id,
                                path,
                                error,
                            } => {
                                self.handle_properties_save_failed(request_id, path, error);
                            }
                            protocol::MetadataMessage::RequestTrackProperties { .. }
                            | protocol::MetadataMessage::SaveTrackProperties { .. } => {}
                        },
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistsRestored(playlists),
                        ) => {
                            let old_len = self.playlist_ids.len();
                            self.playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
                            self.playlist_names =
                                playlists.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
                            self.library_add_to_playlist_checked =
                                vec![false; self.playlist_ids.len()];
                            self.sync_library_add_to_playlist_ui();
                            let new_len = self.playlist_ids.len();
                            let mut slint_playlists = Vec::new();
                            for p in playlists {
                                slint_playlists.push(StandardListViewItem::from(p.name.as_str()));
                            }

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_playlists(ModelRc::from(Rc::new(VecModel::from(
                                    slint_playlists,
                                ))));
                                if new_len > old_len && old_len > 0 {
                                    ui.set_editing_playlist_index((new_len - 1) as i32);
                                }
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistChanged(id),
                        ) => {
                            self.active_playlist_id = id.clone();
                            self.selection_anchor_track_id = None;
                            self.playlist_column_width_overrides_px.clear();
                            self.playlist_column_target_widths_px.clear();
                            self.apply_playlist_column_layout();
                            if let Some(index) =
                                self.playlist_ids.iter().position(|p_id| p_id == &id)
                            {
                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_active_playlist_index(index as i32);
                                });
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistRestored(tracks),
                        ) => {
                            // Switching playlists should always start in the playlist's natural order
                            // with no active read-only filter/search view state.
                            self.reset_filter_state();
                            self.selection_anchor_track_id = None;
                            self.track_ids.clear();
                            self.track_paths.clear();
                            self.track_cover_art_paths.clear();
                            self.track_metadata.clear();
                            for track in &tracks {
                                self.track_ids.push(track.id.clone());
                                self.track_paths.push(track.path.clone());
                                self.track_cover_art_paths.push(None);
                                self.track_metadata
                                    .push(Self::fallback_track_metadata(track.path.as_path()));
                            }
                            self.queue_track_metadata_lookup_batch(&tracks);
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::TrackAdded {
                            id,
                            path,
                        }) => {
                            self.track_ids.push(id.clone());
                            self.track_paths.push(path.clone());
                            self.track_cover_art_paths.push(None);
                            self.track_metadata
                                .push(Self::fallback_track_metadata(path.as_path()));
                            self.queue_track_metadata_lookup(id, path);
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTracks(
                            mut indices,
                        )) => {
                            indices.sort_by(|a, b| b.cmp(a));

                            for index in indices {
                                if index < self.track_ids.len() {
                                    self.track_ids.remove(index);
                                }
                                if index < self.track_paths.len() {
                                    self.track_paths.remove(index);
                                }
                                if index < self.track_cover_art_paths.len() {
                                    self.track_cover_art_paths.remove(index);
                                }
                                if index < self.track_metadata.len() {
                                    self.track_metadata.remove(index);
                                }
                            }
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::DeleteSelected) => {
                            if self.is_filter_view_active() {
                                continue;
                            }
                            if self.selected_indices.is_empty() {
                                continue;
                            }
                            let indices = self.selected_indices.clone();
                            let _ = self.bus_sender.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::DeleteTracks(indices),
                            ));
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::CopySelectedTracks,
                        ) => {
                            self.copy_selected_tracks();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::CutSelectedTracks,
                        ) => {
                            self.cut_selected_tracks();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PasteCopiedTracks,
                        ) => {
                            if self.is_filter_view_active() {
                                continue;
                            }
                            self.paste_copied_tracks();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::TracksInserted { tracks, insert_at },
                        ) => {
                            let inserted_count = tracks.len();
                            let mut insert_cursor = insert_at.min(self.track_ids.len());
                            for track in &tracks {
                                self.track_ids.insert(insert_cursor, track.id.clone());
                                self.track_paths.insert(insert_cursor, track.path.clone());
                                self.track_cover_art_paths.insert(insert_cursor, None);
                                self.track_metadata.insert(
                                    insert_cursor,
                                    Self::fallback_track_metadata(track.path.as_path()),
                                );
                                insert_cursor += 1;
                            }
                            self.queue_track_metadata_lookup_batch(&tracks);
                            if self.pending_paste_feedback {
                                self.pending_paste_feedback = false;
                                let toast_text = format!("Pasted {} tracks", inserted_count);
                                self.library_status_text = toast_text.clone();
                                self.show_library_toast(toast_text);
                            }
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::TracksInsertedBatch { tracks, insert_at },
                        ) => {
                            let inserted_count = tracks.len();
                            let mut insert_cursor = insert_at.min(self.track_ids.len());
                            for track in &tracks {
                                self.track_ids.insert(insert_cursor, track.id.clone());
                                self.track_paths.insert(insert_cursor, track.path.clone());
                                self.track_cover_art_paths.insert(insert_cursor, None);
                                self.track_metadata.insert(
                                    insert_cursor,
                                    Self::fallback_track_metadata(track.path.as_path()),
                                );
                                insert_cursor += 1;
                            }
                            self.queue_track_metadata_lookup_batch(&tracks);
                            if self.pending_paste_feedback {
                                self.pending_paste_feedback = false;
                                let toast_text = format!("Pasted {} tracks", inserted_count);
                                self.library_status_text = toast_text.clone();
                                self.show_library_toast(toast_text);
                            }
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::TrackMetadataBatchUpdated { updates },
                        ) => {
                            if self.apply_track_metadata_batch_updates(updates) {
                                self.refresh_playlist_column_content_targets();
                                self.apply_playlist_column_layout();
                                let playing_track_path = self.current_playing_track_path.clone();
                                let playing_track_metadata =
                                    self.current_playing_track_metadata.clone();
                                self.update_display_for_active_collection(
                                    playing_track_path.as_ref(),
                                    playing_track_metadata.as_ref(),
                                );
                                self.rebuild_track_model();
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlayTrackByViewIndex(view_index),
                        ) => {
                            if let Some(source_index) = self.map_view_to_source_index(view_index) {
                                let _ = self.bus_sender.send(protocol::Message::Playback(
                                    protocol::PlaybackMessage::PlayTrackByIndex(source_index),
                                ));
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::OpenPlaylistSearch,
                        ) => {
                            self.open_playlist_search();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ClosePlaylistSearch,
                        ) => {
                            self.close_playlist_search();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::SetPlaylistSearchQuery(query),
                        ) => {
                            self.set_playlist_search_query(query);
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ClearPlaylistFilterView,
                        ) => {
                            self.clear_playlist_filter_view();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::CyclePlaylistSortByColumn(column_index),
                        ) => {
                            self.cycle_playlist_sort_by_column(column_index);
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::RequestApplyFilterView,
                        ) => {
                            if !self.is_filter_applied() {
                                continue;
                            }
                            let snapshot = self.view_indices.clone();
                            let _ = self.bus_sender.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::ApplyFilterViewSnapshot(snapshot),
                            ));
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ApplyFilterViewSnapshot(source_indices),
                        ) => {
                            self.apply_filter_view_snapshot_locally(source_indices);
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::PlayTrackByIndex(index),
                        ) => {
                            self.active_playing_index = Some(index);
                            self.playback_active = true;
                            let status_text = self
                                .track_metadata
                                .get(index)
                                .map(Self::status_text_from_track_metadata)
                                .unwrap_or_else(|| "".into());
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_status_text(status_text);
                            });
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                            self.playback_active = true;
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                            self.playback_active = false;
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::PlaybackProgress {
                                elapsed_ms,
                                total_ms,
                            },
                        ) => {
                            self.last_progress_at = Some(Instant::now());
                            if self.progress_rl.check().is_ok() {
                                // Check if the displayed second has changed (for text updates)
                                let elapsed_secs = elapsed_ms / 1000;
                                let last_elapsed_secs = self.last_elapsed_ms / 1000;
                                let time_text_changed = elapsed_secs != last_elapsed_secs
                                    || total_ms != self.last_total_ms;

                                // Always compute percentage for smooth progress bar
                                let percentage = if total_ms > 0 {
                                    elapsed_ms as f32 / total_ms as f32
                                } else {
                                    0.0
                                };

                                if time_text_changed {
                                    // Update cached values and set integer ms properties
                                    // This triggers Slint's pure functions to recompute text
                                    self.last_elapsed_ms = elapsed_ms;
                                    self.last_total_ms = total_ms;

                                    let elapsed_ms_i32 = elapsed_ms as i32;
                                    let total_ms_i32 = total_ms as i32;

                                    let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_elapsed_ms(elapsed_ms_i32);
                                        ui.set_total_ms(total_ms_i32);
                                        ui.set_position_percentage(percentage);
                                    });
                                } else {
                                    // Just update the progress bar percentage for smooth animation
                                    let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_position_percentage(percentage);
                                    });
                                }
                            }
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::TechnicalMetadataChanged(meta),
                        ) => {
                            debug!("UiManager: Technical metadata changed: {:?}", meta);
                            self.current_technical_metadata = Some(meta);
                            self.refresh_technical_info_ui();
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::OutputPathChanged(path_info),
                        ) => {
                            self.current_output_path_info = Some(path_info);
                            self.refresh_technical_info_ui();
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                            self.playback_active = false;
                            self.active_playing_index = None;
                            self.last_progress_at = None;
                            let had_playing_track = self.current_playing_track_path.is_some();
                            self.current_playing_track_path = None;
                            self.current_playing_track_metadata = None;
                            self.current_technical_metadata = None;
                            self.current_output_path_info = None;
                            self.update_display_for_active_collection(None, None);

                            // Reset cached progress values
                            self.last_elapsed_ms = 0;
                            self.last_total_ms = 0;

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_technical_info("".into());
                                ui.set_status_text("No track selected".into());
                                ui.set_position_percentage(0.0);
                                ui.set_elapsed_ms(0);
                                ui.set_total_ms(0);
                            });
                            if had_playing_track {
                                self.sync_library_ui();
                            }
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted {
                            index,
                            playlist_id,
                        }) => {
                            let is_active_playlist = playlist_id == self.active_playlist_id;
                            self.active_playing_index = if is_active_playlist {
                                Some(index)
                            } else {
                                None
                            };
                            self.playback_active = is_active_playlist;
                            let status_text = self
                                .track_metadata
                                .get(index)
                                .map(Self::status_text_from_track_metadata)
                                .unwrap_or_else(|| "".into());

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_status_text(status_text);
                            });
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistIndicesChanged {
                                playing_playlist_id,
                                playing_index,
                                playing_track_path,
                                playing_track_metadata,
                                selected_indices,
                                is_playing,
                                playback_order,
                                repeat_mode,
                            },
                        ) => {
                            self.playback_active = is_playing;
                            if !is_playing {
                                self.last_progress_at = None;
                            }
                            let selected_indices_clone = selected_indices.clone();
                            self.selected_indices = selected_indices_clone.clone();
                            let is_playing_active_playlist =
                                playing_playlist_id.as_ref() == Some(&self.active_playlist_id);
                            self.active_playing_index = if is_playing_active_playlist {
                                playing_index
                            } else {
                                None
                            };

                            let previous_playing_track_path =
                                self.current_playing_track_path.clone();
                            self.current_playing_track_path = playing_track_path.clone();
                            self.current_playing_track_metadata = playing_track_metadata.clone();
                            self.update_display_for_active_collection(
                                playing_track_path.as_ref(),
                                playing_track_metadata.as_ref(),
                            );
                            if previous_playing_track_path != self.current_playing_track_path {
                                self.sync_library_ui();
                            }

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let repeat_int = match repeat_mode {
                                    protocol::RepeatMode::Off => 0,
                                    protocol::RepeatMode::Playlist => 1,
                                    protocol::RepeatMode::Track => 2,
                                };
                                ui.set_repeat_mode(repeat_int);

                                let order_int = match playback_order {
                                    protocol::PlaybackOrder::Default => 0,
                                    protocol::PlaybackOrder::Shuffle => 1,
                                    protocol::PlaybackOrder::Random => 2,
                                };
                                ui.set_playback_order_index(order_int);
                            });
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::SelectionChanged(indices),
                        ) => {
                            debug!(
                                "SelectionChanged: setting selected_indices to {:?}",
                                indices
                            );
                            let indices_clone = indices.clone();
                            self.selected_indices = indices_clone.clone();
                            if indices_clone.is_empty() {
                                self.selection_anchor_track_id = None;
                            }
                            debug!(
                                "After SelectionChanged: self.selected_indices = {:?}",
                                self.selected_indices
                            );

                            // Update cover art and metadata display based on selection.
                            // If nothing is selected, fall back to the currently playing track.
                            let playing_track_path = self.current_playing_track_path.clone();
                            let playing_track_metadata =
                                self.current_playing_track_metadata.clone();
                            self.update_display_for_active_collection(
                                playing_track_path.as_ref(),
                                playing_track_metadata.as_ref(),
                            );
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnPointerDown {
                            index,
                            ctrl,
                            shift,
                        }) => {
                            self.on_pointer_down(index, ctrl, shift);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnDragStart {
                            pressed_index,
                        }) => {
                            self.on_drag_start(pressed_index);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnDragMove {
                            drop_gap,
                        }) => {
                            self.on_drag_move(drop_gap);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnDragEnd {
                            drop_gap,
                        }) => {
                            self.on_drag_end(drop_gap);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                            indices,
                            to,
                        }) => {
                            if self.is_filter_view_active() {
                                continue;
                            }
                            debug!("ReorderTracks: indices={:?}, to={}", indices, to);

                            let mut sorted_indices = indices.clone();
                            sorted_indices.sort_unstable();
                            sorted_indices.dedup();
                            sorted_indices.retain(|&i| i < self.track_paths.len());

                            if sorted_indices.is_empty() {
                                continue;
                            }

                            let to = to.min(self.track_paths.len());

                            let first = sorted_indices[0];
                            let last = *sorted_indices.last().unwrap();
                            let block_len = sorted_indices.len();

                            let is_contiguous = sorted_indices
                                .iter()
                                .enumerate()
                                .all(|(k, &i)| i == first + k);

                            if is_contiguous && to >= first && to <= last + 1 {
                                continue;
                            }

                            let mut moved_paths = Vec::new();
                            let mut moved_ids = Vec::new();
                            let mut moved_cover_art_paths = Vec::new();
                            let mut moved_metadata = Vec::new();

                            for &idx in sorted_indices.iter().rev() {
                                if idx < self.track_paths.len() {
                                    moved_paths.push(self.track_paths.remove(idx));
                                }
                                if idx < self.track_ids.len() {
                                    moved_ids.push(self.track_ids.remove(idx));
                                }
                                if idx < self.track_cover_art_paths.len() {
                                    moved_cover_art_paths
                                        .push(self.track_cover_art_paths.remove(idx));
                                }
                                if idx < self.track_metadata.len() {
                                    moved_metadata.push(self.track_metadata.remove(idx));
                                }
                            }
                            moved_paths.reverse();
                            moved_ids.reverse();
                            moved_cover_art_paths.reverse();
                            moved_metadata.reverse();

                            let removed_before = sorted_indices.iter().filter(|&&i| i < to).count();
                            let insert_at = to.saturating_sub(removed_before);

                            for (i, path) in moved_paths.into_iter().enumerate() {
                                self.track_paths.insert(insert_at + i, path);
                            }
                            for (i, id) in moved_ids.into_iter().enumerate() {
                                self.track_ids.insert(insert_at + i, id);
                            }
                            for (i, cover_art_path) in moved_cover_art_paths.into_iter().enumerate()
                            {
                                self.track_cover_art_paths
                                    .insert(insert_at + i, cover_art_path);
                            }
                            for (i, metadata) in moved_metadata.into_iter().enumerate() {
                                self.track_metadata.insert(insert_at + i, metadata);
                            }

                            self.selected_indices = (insert_at..insert_at + block_len).collect();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::CoverArtChanged(path),
                        ) => {
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if let Some(path) = path {
                                    if let Some(img) = UiManager::try_load_cover_art_image(&path) {
                                        ui.set_current_cover_art(img);
                                        ui.set_current_cover_art_available(true);
                                    } else {
                                        ui.set_current_cover_art(slint::Image::default());
                                        ui.set_current_cover_art_available(false);
                                    }
                                } else {
                                    ui.set_current_cover_art(slint::Image::default());
                                    ui.set_current_cover_art_available(false);
                                }
                            });
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::MetadataDisplayChanged(meta),
                        ) => {
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if let Some(meta) = meta {
                                    ui.set_display_title(meta.title.into());
                                    ui.set_display_artist(meta.artist.into());
                                    ui.set_display_album(meta.album.into());
                                    ui.set_display_date(meta.date.into());
                                    ui.set_display_genre(meta.genre.into());
                                } else {
                                    ui.set_display_title("".into());
                                    ui.set_display_artist("".into());
                                    ui.set_display_album("".into());
                                    ui.set_display_date("".into());
                                    ui.set_display_genre("".into());
                                }
                            });
                        }
                        protocol::Message::Config(protocol::ConfigMessage::ConfigChanged(
                            config,
                        )) => {
                            self.library_online_metadata_enabled =
                                config.library.online_metadata_enabled;
                            self.library_online_metadata_prompt_pending =
                                config.library.online_metadata_prompt_pending;
                            if !self.library_online_metadata_enabled {
                                self.library_enrichment_pending.clear();
                                self.library_enrichment_last_request_at.clear();
                                self.library_enrichment_retry_counts.clear();
                                self.library_enrichment_retry_not_before.clear();
                                self.replace_prefetch_queue_if_changed(Vec::new());
                                self.replace_background_queue_if_changed(Vec::new());
                                self.library_last_detail_enrichment_entity = None;
                            }
                            self.playlist_columns = config.ui.playlist_columns.clone();
                            self.album_art_column_min_width_px =
                                config.ui.playlist_album_art_column_min_width_px;
                            self.album_art_column_max_width_px =
                                config.ui.playlist_album_art_column_max_width_px;
                            let valid_column_keys: HashSet<String> = self
                                .playlist_columns
                                .iter()
                                .map(Self::playlist_column_key)
                                .collect();
                            self.playlist_column_width_overrides_px
                                .retain(|key, _| valid_column_keys.contains(key));
                            self.playlist_column_target_widths_px
                                .retain(|key, _| valid_column_keys.contains(key));
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            let playlist_columns = self.playlist_columns.clone();

                            let visible_headers: Vec<slint::SharedString> = playlist_columns
                                .iter()
                                .filter(|column| column.enabled)
                                .map(|column| column.name.as_str().into())
                                .collect();
                            let visible_kinds =
                                Self::visible_playlist_column_kinds(&playlist_columns);
                            let menu_labels: Vec<slint::SharedString> = playlist_columns
                                .iter()
                                .map(|column| column.name.as_str().into())
                                .collect();
                            let menu_checked: Vec<bool> = playlist_columns
                                .iter()
                                .map(|column| column.enabled)
                                .collect();
                            let menu_is_custom: Vec<bool> = playlist_columns
                                .iter()
                                .map(|column| column.custom)
                                .collect();
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_playlist_visible_column_headers(ModelRc::from(Rc::new(
                                    VecModel::from(visible_headers),
                                )));
                                ui.set_playlist_visible_column_kinds(ModelRc::from(Rc::new(
                                    VecModel::from(visible_kinds),
                                )));
                                ui.set_playlist_column_menu_labels(ModelRc::from(Rc::new(
                                    VecModel::from(menu_labels),
                                )));
                                ui.set_playlist_column_menu_checked(ModelRc::from(Rc::new(
                                    VecModel::from(menu_checked),
                                )));
                                ui.set_playlist_column_menu_is_custom(ModelRc::from(Rc::new(
                                    VecModel::from(menu_is_custom),
                                )));
                            });
                            self.sync_library_ui();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistViewportWidthChanged(width_px),
                        ) => {
                            if self.playlist_columns_available_width_px != width_px {
                                self.playlist_columns_available_width_px = width_px;
                                self.apply_playlist_column_layout_preserving_current_widths();
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistColumnWidthOverrides(
                                overrides,
                            ),
                        ) => {
                            self.playlist_column_width_overrides_px.clear();
                            if let Some(overrides) = overrides {
                                for override_item in overrides {
                                    if override_item.column_key.trim().is_empty() {
                                        continue;
                                    }
                                    let clamped_width_px = self.clamp_column_override_width_px(
                                        &override_item.column_key,
                                        override_item.width_px,
                                    );
                                    self.playlist_column_width_overrides_px
                                        .insert(override_item.column_key, clamped_width_px);
                                }
                            }
                            self.apply_playlist_column_layout();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                                column_key,
                                width_px,
                                ..
                            },
                        ) => {
                            if column_key.trim().is_empty() {
                                continue;
                            }
                            let clamped_width_px =
                                self.clamp_column_override_width_px(&column_key, width_px);
                            self.playlist_column_width_overrides_px
                                .insert(column_key, clamped_width_px);
                            self.apply_playlist_column_layout();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ClearActivePlaylistColumnWidthOverride {
                                column_key,
                                ..
                            },
                        ) => {
                            self.playlist_column_width_overrides_px.remove(&column_key);
                            self.apply_playlist_column_layout();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::RepeatModeChanged(repeat_mode),
                        ) => {
                            debug!("UiManager: Repeat mode changed: {:?}", repeat_mode);
                            let repeat_int = match repeat_mode {
                                protocol::RepeatMode::Off => 0,
                                protocol::RepeatMode::Playlist => 1,
                                protocol::RepeatMode::Track => 2,
                            };
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_repeat_mode(repeat_int);
                            });
                        }
                        _ => {}
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    self.on_message_lagged(skipped);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fit_column_widths_to_available_space, ColumnWidthProfile, CoverArtLookupRequest,
        LibraryEntry, LibraryViewState, PlaylistSortDirection, TrackMetadata, UiManager,
    };
    use crate::{config::PlaylistColumnConfig, protocol};
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn make_meta(title: &str) -> TrackMetadata {
        TrackMetadata {
            title: title.to_string(),
            artist: format!("{title}-artist"),
            album: format!("{title}-album"),
            album_artist: format!("{title}-album-artist"),
            date: "2026".to_string(),
            year: "2026".to_string(),
            genre: "test".to_string(),
            track_number: "1".to_string(),
        }
    }

    fn make_library_song(id: &str, title: &str, path: &str) -> protocol::LibrarySong {
        protocol::LibrarySong {
            id: id.to_string(),
            path: PathBuf::from(path),
            title: title.to_string(),
            artist: format!("{title}-artist"),
            album: format!("{title}-album"),
            album_artist: format!("{title}-album-artist"),
            genre: "test-genre".to_string(),
            year: "2025".to_string(),
            track_number: "1".to_string(),
        }
    }

    fn make_library_song_in_album(
        id: &str,
        title: &str,
        path: &str,
        album: &str,
        album_artist: &str,
        year: &str,
        track_number: &str,
    ) -> protocol::LibrarySong {
        protocol::LibrarySong {
            id: id.to_string(),
            path: PathBuf::from(path),
            title: title.to_string(),
            artist: album_artist.to_string(),
            album: album.to_string(),
            album_artist: album_artist.to_string(),
            genre: "test-genre".to_string(),
            year: year.to_string(),
            track_number: track_number.to_string(),
        }
    }

    fn make_library_album(album: &str, album_artist: &str) -> protocol::LibraryAlbum {
        protocol::LibraryAlbum {
            album: album.to_string(),
            album_artist: album_artist.to_string(),
            song_count: 3,
            representative_track_path: Some(PathBuf::from(format!("{album}.mp3"))),
        }
    }

    fn make_library_artist(
        name: &str,
        album_count: u32,
        song_count: u32,
    ) -> protocol::LibraryArtist {
        protocol::LibraryArtist {
            artist: name.to_string(),
            album_count,
            song_count,
        }
    }

    fn default_album_art_profile() -> ColumnWidthProfile {
        ColumnWidthProfile {
            min_px: crate::config::default_playlist_album_art_column_min_width_px(),
            preferred_px: 72,
            max_px: crate::config::default_playlist_album_art_column_max_width_px(),
        }
    }

    #[test]
    fn test_coalesce_cover_art_requests_keeps_latest() {
        let (tx, rx) = mpsc::channel::<CoverArtLookupRequest>();
        tx.send(CoverArtLookupRequest {
            track_path: Some(PathBuf::from("first.mp3")),
        })
        .expect("failed to queue request");
        tx.send(CoverArtLookupRequest {
            track_path: Some(PathBuf::from("second.mp3")),
        })
        .expect("failed to queue request");
        tx.send(CoverArtLookupRequest { track_path: None })
            .expect("failed to queue request");

        let first = rx.recv().expect("expected first request");
        let latest = UiManager::coalesce_cover_art_requests(first, &rx);
        assert_eq!(latest.track_path, None);
    }

    #[test]
    fn test_reset_filter_state_fields_clears_sort_and_search() {
        let mut sort_key = Some("title".to_string());
        let mut sort_direction = Some(PlaylistSortDirection::Descending);
        let mut search_query = "beatles".to_string();
        let mut search_visible = true;

        UiManager::reset_filter_state_fields(
            &mut sort_key,
            &mut sort_direction,
            &mut search_query,
            &mut search_visible,
        );

        assert!(sort_key.is_none());
        assert!(sort_direction.is_none());
        assert!(search_query.is_empty());
        assert!(!search_visible);
    }

    #[test]
    fn test_build_copied_track_paths_preserves_playlist_order() {
        let track_paths = vec![
            PathBuf::from("a.mp3"),
            PathBuf::from("b.mp3"),
            PathBuf::from("c.mp3"),
            PathBuf::from("d.mp3"),
        ];
        let selected_indices = vec![3usize, 1, 3, 0];

        let copied = UiManager::build_copied_track_paths(&track_paths, &selected_indices, &[]);

        assert_eq!(
            copied,
            vec![
                PathBuf::from("a.mp3"),
                PathBuf::from("b.mp3"),
                PathBuf::from("d.mp3")
            ]
        );
    }

    #[test]
    fn test_build_copied_track_paths_preserves_rendered_view_order() {
        let track_paths = vec![
            PathBuf::from("a.mp3"),
            PathBuf::from("b.mp3"),
            PathBuf::from("c.mp3"),
            PathBuf::from("d.mp3"),
        ];
        let selected_indices = vec![3usize, 1, 0];
        let view_indices = vec![2usize, 0, 3, 1];

        let copied =
            UiManager::build_copied_track_paths(&track_paths, &selected_indices, &view_indices);

        assert_eq!(
            copied,
            vec![
                PathBuf::from("a.mp3"),
                PathBuf::from("d.mp3"),
                PathBuf::from("b.mp3")
            ]
        );
    }

    #[test]
    fn test_build_shift_selection_from_view_order_uses_rendered_range() {
        let view_indices = vec![2usize, 0, 3, 1];
        let selected = UiManager::build_shift_selection_from_view_order(&view_indices, Some(0), 1);
        assert_eq!(selected, vec![0, 3, 1]);
    }

    #[test]
    fn test_build_shift_selection_from_view_order_without_anchor_selects_clicked_only() {
        let view_indices = vec![2usize, 0, 3, 1];
        let selected = UiManager::build_shift_selection_from_view_order(&view_indices, None, 3);
        assert_eq!(selected, vec![3]);
    }

    #[test]
    fn test_resolve_display_target_prefers_selection() {
        let selected = vec![1usize];
        let paths = vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")];
        let metadata = vec![make_meta("A"), make_meta("B")];
        let playing_path = Some(PathBuf::from("playing.mp3"));
        let playing_meta = crate::protocol::DetailedMetadata {
            title: "Playing".to_string(),
            artist: "P".to_string(),
            album: "P".to_string(),
            date: "".to_string(),
            genre: "".to_string(),
        };

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            Some(&playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("selected metadata should exist");
        assert_eq!(meta.title, "B");
    }

    #[test]
    fn test_resolve_display_target_falls_back_to_playing_when_no_selection() {
        let selected = vec![];
        let paths = vec![PathBuf::from("a.mp3")];
        let metadata = vec![make_meta("A")];
        let playing_path = Some(PathBuf::from("playing.mp3"));
        let playing_meta = crate::protocol::DetailedMetadata {
            title: "Playing".to_string(),
            artist: "P".to_string(),
            album: "P".to_string(),
            date: "".to_string(),
            genre: "".to_string(),
        };

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            Some(&playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("playing.mp3")));
        let meta = meta.expect("playing metadata should exist");
        assert_eq!(meta.title, "Playing");
    }

    #[test]
    fn test_resolve_display_target_prefers_cached_metadata_for_playing_path_match() {
        let selected = vec![];
        let paths = vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")];
        let metadata = vec![make_meta("A"), make_meta("B")];
        let playing_path = Some(PathBuf::from("b.mp3"));
        let stale_playing_meta = crate::protocol::DetailedMetadata {
            title: "stale".to_string(),
            artist: "stale".to_string(),
            album: "stale".to_string(),
            date: String::new(),
            genre: String::new(),
        };

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            Some(&stale_playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("playing metadata should exist");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist, "B-artist");
    }

    #[test]
    fn test_resolve_display_target_returns_none_without_selection_or_playing() {
        let selected = vec![];
        let paths = vec![PathBuf::from("a.mp3")];
        let metadata = vec![make_meta("A")];

        let (path, meta) =
            UiManager::resolve_display_target(&selected, &paths, &metadata, None, None);

        assert!(path.is_none());
        assert!(meta.is_none());
    }

    #[test]
    fn test_resolve_library_display_target_prefers_selected_song() {
        let selected = vec![1usize];
        let entries = vec![
            LibraryEntry::Song(make_library_song("song-a", "A", "a.mp3")),
            LibraryEntry::Song(make_library_song("song-b", "B", "b.mp3")),
        ];
        let playing_path = Some(PathBuf::from("playing.mp3"));
        let playing_meta = protocol::DetailedMetadata {
            title: "Playing".to_string(),
            artist: "P".to_string(),
            album: "P".to_string(),
            date: String::new(),
            genre: String::new(),
        };

        let (path, meta) = UiManager::resolve_library_display_target(
            &selected,
            &entries,
            playing_path.as_ref(),
            Some(&playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("selected library song metadata should exist");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist, "B-artist");
    }

    #[test]
    fn test_resolve_library_display_target_falls_back_when_selection_has_no_song() {
        let selected = vec![0usize];
        let entries = vec![LibraryEntry::Artist(protocol::LibraryArtist {
            artist: "Artist".to_string(),
            album_count: 2,
            song_count: 10,
        })];
        let playing_path = Some(PathBuf::from("playing.mp3"));
        let playing_meta = protocol::DetailedMetadata {
            title: "Playing".to_string(),
            artist: "P".to_string(),
            album: "P".to_string(),
            date: String::new(),
            genre: String::new(),
        };

        let (path, meta) = UiManager::resolve_library_display_target(
            &selected,
            &entries,
            playing_path.as_ref(),
            Some(&playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("playing.mp3")));
        let meta = meta.expect("playing metadata should be preserved");
        assert_eq!(meta.title, "Playing");
    }

    #[test]
    fn test_build_artist_detail_entries_groups_songs_by_album_and_sorts_year_desc() {
        let albums = vec![
            make_library_album("Alpha", "Artist"),
            make_library_album("No Year", "Artist"),
            make_library_album("Zeta", "Artist"),
        ];
        let songs = vec![
            make_library_song_in_album(
                "alpha-1",
                "Alpha Song 1",
                "alpha-1.mp3",
                "Alpha",
                "Artist",
                "1998",
                "1",
            ),
            make_library_song_in_album(
                "alpha-2",
                "Alpha Song 2",
                "alpha-2.mp3",
                "Alpha",
                "Artist",
                "1998",
                "2",
            ),
            make_library_song_in_album(
                "noyear-1",
                "No Year Song 1",
                "noyear-1.mp3",
                "No Year",
                "Artist",
                "",
                "1",
            ),
            make_library_song_in_album(
                "zeta-1",
                "Zeta Song 1",
                "zeta-1.mp3",
                "Zeta",
                "Artist",
                "2024",
                "1",
            ),
            make_library_song_in_album(
                "zeta-2",
                "Zeta Song 2",
                "zeta-2.mp3",
                "Zeta",
                "Artist",
                "2024",
                "2",
            ),
        ];

        let entries = UiManager::build_artist_detail_entries(albums, songs);
        let summary: Vec<String> = entries
            .iter()
            .map(|entry| match entry {
                LibraryEntry::Album(album) => {
                    format!("album:{}\u{001f}{}", album.album, album.album_artist)
                }
                LibraryEntry::Song(song) => format!("song:{}\u{001f}{}", song.album, song.title),
                LibraryEntry::Artist(_) | LibraryEntry::Genre(_) | LibraryEntry::Decade(_) => {
                    "unexpected".to_string()
                }
            })
            .collect();

        assert_eq!(
            summary,
            vec![
                "album:Zeta\u{001f}Artist",
                "song:Zeta\u{001f}Zeta Song 1",
                "song:Zeta\u{001f}Zeta Song 2",
                "album:Alpha\u{001f}Artist",
                "song:Alpha\u{001f}Alpha Song 1",
                "song:Alpha\u{001f}Alpha Song 2",
                "album:No Year\u{001f}Artist",
                "song:No Year\u{001f}No Year Song 1",
            ]
        );
    }

    #[test]
    fn test_build_artist_detail_entries_creates_album_header_for_song_only_album() {
        let albums = vec![];
        let songs = vec![make_library_song_in_album(
            "beta-1",
            "Beta Song 1",
            "beta-1.mp3",
            "Beta",
            "Artist",
            "2019",
            "1",
        )];

        let entries = UiManager::build_artist_detail_entries(albums, songs);
        assert_eq!(entries.len(), 2);

        match &entries[0] {
            LibraryEntry::Album(album) => {
                assert_eq!(album.album, "Beta");
                assert_eq!(album.album_artist, "Artist");
            }
            other => panic!("expected synthetic album header, got {:?}", other),
        }
        match &entries[1] {
            LibraryEntry::Song(song) => {
                assert_eq!(song.album, "Beta");
                assert_eq!(song.title, "Beta Song 1");
            }
            other => panic!(
                "expected song row under synthetic album header, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_build_global_search_entries_orders_by_name_then_kind() {
        let songs = vec![
            make_library_song("song-alpha", "Alpha", "alpha.mp3"),
            make_library_song("song-beta", "Beta", "beta.mp3"),
        ];
        let artists = vec![make_library_artist("Alpha", 3, 25)];
        let albums = vec![make_library_album("Alpha", "Artist One")];

        let entries = UiManager::build_global_search_entries(songs, artists, albums);
        let summary: Vec<String> = entries
            .into_iter()
            .map(|entry| match entry {
                LibraryEntry::Song(song) => format!("song:{}", song.title),
                LibraryEntry::Artist(artist) => format!("artist:{}", artist.artist),
                LibraryEntry::Album(album) => format!("album:{}", album.album),
                LibraryEntry::Genre(_) | LibraryEntry::Decade(_) => "unexpected".to_string(),
            })
            .collect();

        assert_eq!(
            summary,
            vec!["artist:Alpha", "album:Alpha", "song:Alpha", "song:Beta"]
        );
    }

    #[test]
    fn test_library_view_labels_for_global_search() {
        let (title, subtitle) = UiManager::library_view_labels(&LibraryViewState::GlobalSearch);
        assert_eq!(title, "Global Search");
        assert_eq!(subtitle, "Type to search songs, artists, and albums");
    }

    #[test]
    fn test_build_library_view_indices_for_query_matches_current_view_entries() {
        let entries = vec![
            LibraryEntry::Song(make_library_song("song-a", "Midnight Train", "train.mp3")),
            LibraryEntry::Artist(protocol::LibraryArtist {
                artist: "Daft Punk".to_string(),
                album_count: 4,
                song_count: 30,
            }),
            LibraryEntry::Album(make_library_album("Discovery", "Daft Punk")),
            LibraryEntry::Genre(protocol::LibraryGenre {
                genre: "Electronic".to_string(),
                song_count: 12,
            }),
            LibraryEntry::Decade(protocol::LibraryDecade {
                decade: "2000s".to_string(),
                song_count: 99,
            }),
        ];

        let song_match = UiManager::build_library_view_indices_for_query(&entries, "train");
        assert_eq!(song_match, vec![0]);

        let artist_match = UiManager::build_library_view_indices_for_query(&entries, "daft");
        assert_eq!(artist_match, vec![1, 2]);

        let decade_match = UiManager::build_library_view_indices_for_query(&entries, "2000");
        assert_eq!(decade_match, vec![4]);
    }

    #[test]
    fn test_build_library_view_indices_for_query_returns_all_when_query_empty() {
        let entries = vec![
            LibraryEntry::Song(make_library_song("song-a", "A", "a.mp3")),
            LibraryEntry::Artist(protocol::LibraryArtist {
                artist: "Artist".to_string(),
                album_count: 1,
                song_count: 1,
            }),
            LibraryEntry::Album(make_library_album("Album", "Artist")),
        ];

        let indices = UiManager::build_library_view_indices_for_query(&entries, "   ");
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn test_build_library_selection_specs_for_entries_expands_supported_item_types() {
        let entries = vec![
            LibraryEntry::Song(make_library_song("song-a", "A", "a.mp3")),
            LibraryEntry::Artist(protocol::LibraryArtist {
                artist: "Artist A".to_string(),
                album_count: 2,
                song_count: 20,
            }),
            LibraryEntry::Album(make_library_album("Album A", "Artist A")),
            LibraryEntry::Genre(protocol::LibraryGenre {
                genre: "Rock".to_string(),
                song_count: 15,
            }),
            LibraryEntry::Decade(protocol::LibraryDecade {
                decade: "1990s".to_string(),
                song_count: 40,
            }),
        ];
        let selected_indices = vec![1usize, 2, 0, 3, 4];

        let specs =
            UiManager::build_library_selection_specs_for_entries(&entries, &selected_indices);
        assert_eq!(specs.len(), 5);

        match &specs[0] {
            protocol::LibrarySelectionSpec::Artist { artist } => assert_eq!(artist, "Artist A"),
            other => panic!("expected artist spec, got {:?}", other),
        }
        match &specs[1] {
            protocol::LibrarySelectionSpec::Album {
                album,
                album_artist,
            } => {
                assert_eq!(album, "Album A");
                assert_eq!(album_artist, "Artist A");
            }
            other => panic!("expected album spec, got {:?}", other),
        }
        match &specs[2] {
            protocol::LibrarySelectionSpec::Song { path } => {
                assert_eq!(path, &PathBuf::from("a.mp3"));
            }
            other => panic!("expected song spec, got {:?}", other),
        }
        match &specs[3] {
            protocol::LibrarySelectionSpec::Genre { genre } => assert_eq!(genre, "Rock"),
            other => panic!("expected genre spec, got {:?}", other),
        }
        match &specs[4] {
            protocol::LibrarySelectionSpec::Decade { decade } => assert_eq!(decade, "1990s"),
            other => panic!("expected decade spec, got {:?}", other),
        }
    }

    #[test]
    fn test_build_library_selection_specs_for_entries_deduplicates_items() {
        let entries = vec![
            LibraryEntry::Song(make_library_song("song-a", "A", "a.mp3")),
            LibraryEntry::Song(make_library_song("song-b", "B", "b.mp3")),
            LibraryEntry::Album(make_library_album("Album A", "Artist A")),
        ];
        let selected_indices = vec![0usize, 2, 0, 1, 2, 99];

        let specs =
            UiManager::build_library_selection_specs_for_entries(&entries, &selected_indices);
        assert_eq!(specs.len(), 3);

        match &specs[0] {
            protocol::LibrarySelectionSpec::Song { path } => {
                assert_eq!(path, &PathBuf::from("a.mp3"));
            }
            other => panic!("expected first song spec, got {:?}", other),
        }
        match &specs[1] {
            protocol::LibrarySelectionSpec::Album {
                album,
                album_artist,
            } => {
                assert_eq!(album, "Album A");
                assert_eq!(album_artist, "Artist A");
            }
            other => panic!("expected album spec, got {:?}", other),
        }
        match &specs[2] {
            protocol::LibrarySelectionSpec::Song { path } => {
                assert_eq!(path, &PathBuf::from("b.mp3"));
            }
            other => panic!("expected second song spec, got {:?}", other),
        }
    }

    #[test]
    fn test_render_column_value_replaces_placeholders() {
        let metadata = make_meta("Song");
        let rendered = UiManager::render_column_value(&metadata, "{album} ({year})");
        assert_eq!(rendered, "Song-album (2026)");
    }

    #[test]
    fn test_render_column_value_handles_escaping_and_unknown_fields() {
        let metadata = make_meta("Song");
        let rendered = UiManager::render_column_value(&metadata, "{{{unknown}}} - {artist}");
        assert_eq!(rendered, "{} - Song-artist");
    }

    #[test]
    fn test_is_album_art_builtin_column_requires_builtin_column() {
        let builtin_album_art = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_album_art = PlaylistColumnConfig {
            name: "Custom Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: true,
        };

        assert!(UiManager::is_album_art_builtin_column(&builtin_album_art));
        assert!(!UiManager::is_album_art_builtin_column(&custom_album_art));
    }

    #[test]
    fn test_build_playlist_row_values_keeps_album_art_builtin_empty() {
        let metadata = make_meta("Song");
        let columns = vec![
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Custom Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: true,
            },
        ];

        let values = UiManager::build_playlist_row_values(&metadata, &columns);
        assert_eq!(values[0], "Song");
        assert_eq!(values[1], "");
        assert_eq!(values[2], "");
    }

    #[test]
    fn test_is_sortable_playlist_column_rejects_album_art_builtin() {
        let album_art = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let title = PlaylistColumnConfig {
            name: "Title".to_string(),
            format: "{title}".to_string(),
            enabled: true,
            custom: false,
        };

        assert!(!UiManager::is_sortable_playlist_column(&album_art));
        assert!(UiManager::is_sortable_playlist_column(&title));
    }

    #[test]
    fn test_compute_playlist_row_height_without_album_art_column_uses_base_height() {
        let columns = [
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Artist".to_string(),
                format: "{artist}".to_string(),
                enabled: true,
                custom: false,
            },
        ];
        let visible_columns: Vec<&PlaylistColumnConfig> = columns.iter().collect();

        assert_eq!(
            UiManager::compute_playlist_row_height_px_for_visible_columns(
                &visible_columns,
                &[140, 180],
                default_album_art_profile(),
            ),
            30
        );
    }

    #[test]
    fn test_compute_playlist_row_height_with_album_art_column_scales_with_width() {
        let columns = [
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: false,
            },
        ];
        let visible_columns: Vec<&PlaylistColumnConfig> = columns.iter().collect();

        assert_eq!(
            UiManager::compute_playlist_row_height_px_for_visible_columns(
                &visible_columns,
                &[140, 64],
                default_album_art_profile(),
            ),
            72
        );
    }

    #[test]
    fn test_compute_playlist_row_height_with_large_album_art_width_clamps_to_maximum() {
        let columns = [PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        }];
        let visible_columns: Vec<&PlaylistColumnConfig> = columns.iter().collect();

        assert_eq!(
            UiManager::compute_playlist_row_height_px_for_visible_columns(
                &visible_columns,
                &[700],
                default_album_art_profile(),
            ),
            488
        );
    }

    #[test]
    fn test_compute_playlist_row_height_ignores_custom_album_art_placeholder_column() {
        let columns = [PlaylistColumnConfig {
            name: "Custom Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: true,
        }];
        let visible_columns: Vec<&PlaylistColumnConfig> = columns.iter().collect();

        assert_eq!(
            UiManager::compute_playlist_row_height_px_for_visible_columns(
                &visible_columns,
                &[88],
                default_album_art_profile(),
            ),
            30
        );
    }

    #[test]
    fn test_fit_column_widths_shrinks_to_available_width() {
        let mut widths = vec![220u32, 180u32, 200u32];
        let mins = vec![100u32, 120u32, 130u32];
        let maxs = vec![400u32, 360u32, 380u32];

        fit_column_widths_to_available_space(&mut widths, &mins, &maxs, 420);

        assert_eq!(widths.iter().copied().sum::<u32>(), 420);
        assert!(widths[0] >= mins[0] && widths[0] <= maxs[0]);
        assert!(widths[1] >= mins[1] && widths[1] <= maxs[1]);
        assert!(widths[2] >= mins[2] && widths[2] <= maxs[2]);
    }

    #[test]
    fn test_fit_column_widths_keeps_targets_when_space_is_available() {
        let mut widths = vec![120u32, 140u32, 160u32];
        let mins = vec![90u32, 100u32, 120u32];
        let maxs = vec![260u32, 280u32, 300u32];

        fit_column_widths_to_available_space(&mut widths, &mins, &maxs, 640);

        assert_eq!(widths, vec![120u32, 140u32, 160u32]);
    }

    #[test]
    fn test_fit_column_widths_stops_at_minimums_when_viewport_too_small() {
        let mut widths = vec![150u32, 170u32, 190u32];
        let mins = vec![100u32, 120u32, 140u32];
        let maxs = vec![260u32, 280u32, 300u32];

        fit_column_widths_to_available_space(&mut widths, &mins, &maxs, 120);

        assert_eq!(widths, mins);
    }

    #[test]
    fn test_resolve_playlist_column_target_width_preserves_current_width_on_viewport_resize() {
        let profile = ColumnWidthProfile {
            min_px: 60,
            preferred_px: 160,
            max_px: 320,
        };

        let target = UiManager::resolve_playlist_column_target_width_px(
            None,
            Some(140),
            None,
            220,
            profile,
            true,
        );

        assert_eq!(target, 140);
    }

    #[test]
    fn test_resolve_playlist_column_target_width_prefers_override_and_clamps() {
        let profile = ColumnWidthProfile {
            min_px: 16,
            preferred_px: 72,
            max_px: 480,
        };

        let from_override = UiManager::resolve_playlist_column_target_width_px(
            Some(12),
            Some(200),
            Some(240),
            72,
            profile,
            true,
        );
        let from_content = UiManager::resolve_playlist_column_target_width_px(
            None,
            Some(200),
            Some(240),
            72,
            profile,
            false,
        );

        assert_eq!(from_override, 16);
        assert_eq!(from_content, 72);
    }

    #[test]
    fn test_resolve_playlist_column_target_width_prefers_stored_target_when_preserving() {
        let profile = ColumnWidthProfile {
            min_px: 16,
            preferred_px: 72,
            max_px: 480,
        };

        let target = UiManager::resolve_playlist_column_target_width_px(
            None,
            Some(24),
            Some(96),
            72,
            profile,
            true,
        );

        assert_eq!(target, 96);
    }

    #[test]
    fn test_layout_min_width_px_for_album_art_uses_target_width() {
        let column = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let profile = ColumnWidthProfile {
            min_px: 16,
            preferred_px: 72,
            max_px: 480,
        };

        assert_eq!(
            UiManager::layout_min_width_px_for_column(&column, profile, 128),
            128
        );
    }

    #[test]
    fn test_layout_min_width_px_for_text_column_uses_profile_minimum() {
        let column = PlaylistColumnConfig {
            name: "Title".to_string(),
            format: "{title}".to_string(),
            enabled: true,
            custom: false,
        };
        let profile = ColumnWidthProfile {
            min_px: 140,
            preferred_px: 230,
            max_px: 440,
        };

        assert_eq!(
            UiManager::layout_min_width_px_for_column(&column, profile, 220),
            140
        );
    }

    fn apply_reorder(paths: &mut Vec<String>, indices: &[usize], gap: usize) -> Vec<usize> {
        let len = paths.len();
        if len == 0 {
            return vec![];
        }

        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        indices.retain(|&i| i < len);

        if indices.is_empty() {
            return vec![];
        }

        let gap = gap.min(len);

        let first = indices[0];
        let last = *indices.last().unwrap();
        let block_len = indices.len();

        let is_contiguous = indices.iter().enumerate().all(|(k, &i)| i == first + k);

        if is_contiguous && gap >= first && gap <= last + 1 {
            return indices;
        }

        let mut moved = Vec::new();
        for &idx in indices.iter().rev() {
            if idx < paths.len() {
                moved.push(paths.remove(idx));
            }
        }
        moved.reverse();

        let removed_before = indices.iter().filter(|&&i| i < gap).count();
        let insert_at = gap.saturating_sub(removed_before);

        for (offset, item) in moved.into_iter().enumerate() {
            paths.insert(insert_at + offset, item);
        }

        (insert_at..insert_at + block_len).collect()
    }

    // Gap-index semantic tests
    // Gap 0 = before element 0
    // Gap 1 = between element 0 and 1
    // Gap 2 = between element 1 and 2
    // Gap N = after element N-1

    fn make_test_paths() -> Vec<String> {
        vec![
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            "D".to_string(),
        ]
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_0_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 0);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_2() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 2);
        assert_eq!(paths, vec!["B", "A", "C", "D"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_3() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 3);
        assert_eq!(paths, vec!["B", "C", "A", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 4);
        assert_eq!(paths, vec!["B", "C", "D", "A"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 0);
        assert_eq!(paths, vec!["C", "A", "B", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 1);
        assert_eq!(paths, vec!["A", "C", "B", "D"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 0);
        assert_eq!(paths, vec!["D", "A", "B", "C"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_1() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 1);
        assert_eq!(paths, vec!["A", "D", "B", "C"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_2() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 2);
        assert_eq!(paths, vec!["A", "B", "D", "C"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_4_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 4);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_0_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 0);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_3() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 3);
        assert_eq!(paths, vec!["C", "A", "B", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 4);
        assert_eq!(paths, vec!["C", "D", "A", "B"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 0);
        assert_eq!(paths, vec!["B", "C", "A", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 4);
        assert_eq!(paths, vec!["A", "D", "B", "C"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 0);
        assert_eq!(paths, vec!["C", "D", "A", "B"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_1() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 1);
        assert_eq!(paths, vec!["A", "C", "D", "B"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_4_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 4);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_all_three_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1, 2], 4);
        assert_eq!(paths, vec!["D", "A", "B", "C"]);
        assert_eq!(new_selection, vec![1, 2, 3]);
    }

    #[test]
    fn test_gap_reorder_all_three_from_middle_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2, 3], 0);
        assert_eq!(paths, vec!["B", "C", "D", "A"]);
        assert_eq!(new_selection, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_reorder_non_contiguous_not_noop() {
        // Select A, B, D (non-contiguous) and drop at gap 2 (after B)
        // Should bring D up before C, even though A and B don't move
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1, 3], 2);
        assert_eq!(paths, vec!["A", "B", "D", "C"]);
        assert_eq!(new_selection, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_reorder_non_contiguous_middle_gap() {
        // Select A, C (non-contiguous) and drop at gap 1 (after A)
        // Should move C to after A, resulting in [A, C, B, D]
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 2], 1);
        assert_eq!(paths, vec!["A", "C", "B", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }
}
