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
    process::Command,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver as StdReceiver, Sender as StdSender},
    },
    thread,
};

use governor::state::NotKeyed;
use log::{debug, info, trace, warn};
use slint::{Image, Model, ModelRc, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{
    config::{self, PlaylistColumnConfig},
    image_pipeline::{self, ManagedImageKind},
    integration_keyring::get_opensubsonic_password,
    integration_uri::{is_remote_track_path, parse_opensubsonic_track_uri},
    layout::PlaylistColumnWidthOverrideConfig,
    metadata_tags, protocol, AppWindow, LayoutAlbumArtViewerPanelModel,
    LayoutMetadataViewerPanelModel, LibraryRowData, MetadataEditorField as UiMetadataEditorField,
    TrackRowData,
};
use governor::{Quota, RateLimiter};

const OPENSUBSONIC_API_VERSION: &str = "1.16.1";
const OPENSUBSONIC_CLIENT_ID: &str = "roqtune";

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
    list_image_prepare_tx: StdSender<ListImagePrepareRequest>,
    embedded_cover_art_prepare_tx: StdSender<EmbeddedCoverArtPrepareRequest>,
    metadata_lookup_tx: StdSender<MetadataLookupRequest>,
    last_cover_art_lookup_path: Option<PathBuf>,
    active_playlist_id: String,
    playlist_ids: Vec<String>,
    playlist_names: Vec<String>,
    opensubsonic_sync_eligible_playlist_ids: HashSet<String>,
    unavailable_track_ids: HashSet<String>,
    track_ids: Vec<String>,
    track_paths: Vec<PathBuf>,
    track_cover_art_paths: Vec<Option<PathBuf>>,
    track_cover_art_missing_tracks: HashSet<PathBuf>,
    track_metadata: Vec<TrackMetadata>,
    /// Mapping from **view row position** to **source index** in the parallel
    /// arrays (`track_ids`, `track_paths`, `track_metadata`).
    ///
    /// When a sort or search filter is active, `view_indices` contains the
    /// filtered/sorted permutation of source indices — `view_indices[view_row]`
    /// gives the source index for that visible row.  When no filter is active
    /// the vector is **empty**, which means view index == source index (identity).
    ///
    /// # Coordinate spaces
    ///
    /// *Source index*: position in the editing playlist's underlying arrays.
    /// *View index*: position in the rendered (possibly filtered/sorted) list.
    ///
    /// All messages sent to `PlaylistManager` that reference track positions
    /// (e.g. `SelectTrackMulti`, `DeleteTracks`, `ReorderTracks`) use **source
    /// indices**.  The UI emits **view indices** which `UiManager` translates
    /// via `map_view_to_source_index` before forwarding.
    ///
    /// # Playback queue relationship
    ///
    /// `build_playlist_queue_snapshot` iterates `view_indices` to emit the
    /// playback queue in **view order**.  The `PlaylistManager` then operates
    /// on that queue using its own internal indices.  The `playing_index`
    /// returned in `PlaylistIndicesChanged` is therefore a *playback-queue*
    /// index, **not** a source index.  `resolve_active_playing_source_index`
    /// must resolve back to a source index via the track path, not by using
    /// the raw `playing_index`.
    view_indices: Vec<usize>,
    selected_indices: Vec<usize>,
    selection_anchor_track_id: Option<String>,
    copied_track_paths: Vec<PathBuf>,
    /// Source index of the currently playing track in the editing playlist's
    /// parallel arrays.  Resolved via `playing_track_path` (NOT from the raw
    /// `playing_index` in `PlaylistIndicesChanged`, which is a playback-queue
    /// index and does not correspond to source order when a filter/sort is active).
    active_playing_index: Option<usize>,
    /// Source index of the currently playing track in `library_entries`.
    /// Resolved via the stable track id stored in `playing_track.id`
    /// (with path-based fallback), consistent with how playlist mode resolves
    /// `active_playing_index`.  See `update_library_playing_index`.
    library_playing_index: Option<usize>,
    drag_indices: Vec<usize>,
    is_dragging: bool,
    pressed_index: Option<usize>,
    pending_single_select_on_click: Option<usize>,
    progress_rl:
        RateLimiter<NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>,
    // Cached progress values to avoid unnecessary UI updates
    last_elapsed_ms: u64,
    last_total_ms: u64,
    playing_track: PlayingTrackState,
    favorites_by_key: HashMap<String, protocol::FavoriteEntityRef>,
    display_target_priority: DisplayTargetPriority,
    current_technical_metadata: Option<protocol::TechnicalMetadata>,
    current_output_path_info: Option<protocol::OutputPathInfo>,
    cast_connected: bool,
    cast_connecting: bool,
    cast_discovering: bool,
    cast_device_name: String,
    cast_playback_path_kind: Option<protocol::CastPlaybackPathKind>,
    cast_transcode_output_metadata: Option<protocol::TechnicalMetadata>,
    cast_device_ids: Vec<String>,
    cast_device_names: Vec<String>,
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
    auto_scroll_to_playing_track: bool,
    playlist_prefetch_first_row: usize,
    playlist_prefetch_row_count: usize,
    playlist_scroll_center_token: i32,
    playback_active: bool,
    processed_message_count: u64,
    lagged_message_count: u64,
    last_message_at: Instant,
    last_progress_at: Option<Instant>,
    last_health_log_at: Instant,
    collection_mode: i32,
    library_view_stack: Vec<LibraryViewState>,
    library_entries: Vec<LibraryEntry>,
    /// Mapping from library view row to source index in `library_entries`.
    /// Semantics mirror `view_indices` for playlists: when a search filter is
    /// active the vector contains the filtered permutation; when empty, view
    /// index == source index (identity).  Library queues are built in view
    /// order so next/prev follows the visible row sequence.
    library_view_indices: Vec<usize>,
    library_selected_indices: Vec<usize>,
    library_selection_anchor: Option<usize>,
    library_root_counts: [usize; 6],
    library_cover_art_paths: HashMap<PathBuf, Option<PathBuf>>,
    folder_cover_art_paths: HashMap<PathBuf, Option<PathBuf>>,
    library_enrichment:
        HashMap<protocol::LibraryEnrichmentEntity, protocol::LibraryEnrichmentPayload>,
    library_enrichment_pending: HashSet<protocol::LibraryEnrichmentEntity>,
    library_enrichment_last_request_at: HashMap<protocol::LibraryEnrichmentEntity, Instant>,
    library_enrichment_retry_counts: HashMap<protocol::LibraryEnrichmentEntity, u32>,
    library_enrichment_retry_not_before: HashMap<protocol::LibraryEnrichmentEntity, Instant>,
    library_enrichment_not_found_not_before: HashMap<protocol::LibraryEnrichmentEntity, Instant>,
    library_enrichment_failed_attempt_counts: HashMap<protocol::LibraryEnrichmentEntity, u32>,
    library_last_detail_enrichment_entity: Option<protocol::LibraryEnrichmentEntity>,
    library_online_metadata_enabled: bool,
    library_online_metadata_prompt_pending: bool,
    list_image_max_edge_px: u32,
    cover_art_cache_max_size_mb: u32,
    artist_image_cache_max_size_mb: u32,
    cover_art_memory_cache_max_size_mb: u32,
    artist_image_memory_cache_max_size_mb: u32,
    pending_list_image_requests:
        HashSet<(PathBuf, protocol::UiImageKind, protocol::UiImageVariant)>,
    library_artist_prefetch_first_row: usize,
    library_artist_prefetch_row_count: usize,
    library_scroll_positions: HashMap<LibraryScrollViewKey, usize>,
    pending_library_scroll_restore_row: Option<usize>,
    library_scroll_restore_token: i32,
    library_scroll_center_token: i32,
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
    pending_library_remove_selections: Vec<protocol::LibrarySelectionSpec>,
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

#[derive(Clone, Default)]
struct PlayingTrackState {
    /// Stable track id of the currently playing track.  Used to resolve the
    /// playing source index in both playlist and library views, since it
    /// uniquely identifies an entry even when duplicate file paths exist.
    id: Option<String>,
    path: Option<PathBuf>,
    metadata: Option<protocol::DetailedMetadata>,
}

/// Width policy used by adaptive playlist column sizing.
#[derive(Clone, Copy, Debug)]
struct ColumnWidthProfile {
    min_px: u32,
    preferred_px: u32,
    max_px: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaylistColumnClass {
    AlbumArt,
    Favorite,
    TrackNumber,
    DiscNumber,
    YearDate,
    Title,
    ArtistFamily,
    Album,
    Genre,
    Duration,
    Custom,
    Generic,
}

#[derive(Clone, Copy, Debug)]
struct DeterministicColumnLayoutSpec {
    min_px: u32,
    max_px: u32,
    target_px: u32,
    semantic_floor_px: u32,
    emergency_floor_px: u32,
    shrink_priority: u8,
    fixed_width: bool,
}

/// Cover-art lookup request payload used by the internal worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverArtLookupRequest {
    track_path: Option<PathBuf>,
}

/// Deferred list-thumbnail preparation payload used by an internal worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListImagePrepareRequest {
    source_path: PathBuf,
    kind: protocol::UiImageKind,
    variant: protocol::UiImageVariant,
    max_edge_px: u32,
}

/// Deferred embedded cover-art extraction payload used by an internal worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddedCoverArtPrepareRequest {
    track_path: PathBuf,
    max_edge_px: u32,
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
    TracksRoot,
    ArtistsRoot,
    AlbumsRoot,
    GenresRoot,
    DecadesRoot,
    FavoritesRoot,
    FavoriteTracks,
    FavoriteArtists,
    FavoriteAlbums,
    GlobalSearch,
    ArtistDetail { artist: String },
    AlbumDetail { album: String, album_artist: String },
    GenreDetail { genre: String },
    DecadeDetail { decade: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LibraryScrollViewKey {
    TracksRoot,
    ArtistsRoot,
    AlbumsRoot,
    GenresRoot,
    DecadesRoot,
    FavoritesRoot,
    FavoriteTracks,
    FavoriteArtists,
    FavoriteAlbums,
    GlobalSearch,
}

#[derive(Clone, Debug)]
enum LibraryEntry {
    Track(protocol::LibraryTrack),
    Artist(protocol::LibraryArtist),
    Album(protocol::LibraryAlbum),
    Genre(protocol::LibraryGenre),
    Decade(protocol::LibraryDecade),
    FavoriteCategory(protocol::FavoriteCategory),
}

#[derive(Clone, Debug)]
struct LibraryRowPresentation {
    leading: String,
    primary: String,
    secondary: String,
    item_kind: i32,
    cover_art_path: Option<PathBuf>,
    source_badge: String,
    is_playing: bool,
    favoritable: bool,
    favorited: bool,
    selected: bool,
}

#[derive(Clone, Debug, Default)]
struct ViewerTrackContext {
    artist: String,
    album: String,
    album_artist: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayTargetPriority {
    Selection,
    Playing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayTargetResolutionMode {
    PreferSelection,
    PreferPlaying,
    SelectionOnly,
    PlayingOnly,
}

const PLAYLIST_COLUMN_KIND_TEXT: i32 = 0;
const PLAYLIST_COLUMN_KIND_ALBUM_ART: i32 = 1;
const PLAYLIST_COLUMN_KIND_FAVORITE: i32 = 2;
const BASE_ROW_HEIGHT_PX: u32 = 30;
const ALBUM_ART_ROW_PADDING_PX: u32 = 8;
const COLLECTION_MODE_PLAYLIST: i32 = 0;
const COLLECTION_MODE_LIBRARY: i32 = 1;
const VIEWER_DISPLAY_PRIORITY_DEFAULT: i32 = 0;
const VIEWER_DISPLAY_PRIORITY_PREFER_SELECTION: i32 = 1;
const VIEWER_DISPLAY_PRIORITY_PREFER_NOW_PLAYING: i32 = 2;
const VIEWER_DISPLAY_PRIORITY_SELECTION_ONLY: i32 = 3;
const VIEWER_DISPLAY_PRIORITY_NOW_PLAYING_ONLY: i32 = 4;
const VIEWER_METADATA_SOURCE_TRACK: i32 = 0;
const VIEWER_METADATA_SOURCE_ALBUM_DESCRIPTION: i32 = 1;
const VIEWER_METADATA_SOURCE_ARTIST_BIO: i32 = 2;
const VIEWER_IMAGE_SOURCE_ALBUM_ART: i32 = 0;
const VIEWER_IMAGE_SOURCE_ARTIST_IMAGE: i32 = 1;
const LIBRARY_ITEM_KIND_SONG: i32 = 0;
const LIBRARY_ITEM_KIND_ARTIST: i32 = 1;
const LIBRARY_ITEM_KIND_ALBUM: i32 = 2;
const LIBRARY_ITEM_KIND_GENRE: i32 = 3;
const LIBRARY_ITEM_KIND_DECADE: i32 = 4;
const DETAIL_ENRICHMENT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const PREFETCH_RETRY_MAX_ATTEMPTS: u32 = 4;
const PREFETCH_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const NOT_FOUND_ENRICHMENT_RETRY_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const ENRICHMENT_FAILED_ATTEMPT_CAP: u32 = 5;
const LIBRARY_PREFETCH_FALLBACK_VISIBLE_ROWS: usize = 24;
const LIBRARY_PREFETCH_TOP_OVERSCAN_ROWS: usize = 2;
const LIBRARY_PREFETCH_BOTTOM_OVERSCAN_ROWS: usize = 10;
const LIBRARY_BACKGROUND_WARM_QUEUE_SIZE: usize = 6;
const IMAGE_CACHE_MAX_ENTRIES: usize = 4096;
const COVER_ART_FAILED_PATHS_MAX_ENTRIES: usize = 4096;
const LIBRARY_PAGE_FETCH_LIMIT: usize = 512;
const REMOTE_TRACK_UNAVAILABLE_TITLE: &str = "Remote track unavailable";
const PLAYLIST_COLUMN_SPACING_PX: u32 = 10;
const DEFAULT_IMAGE_MEMORY_CACHE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const DETAIL_VIEWER_RENDER_MAX_EDGE_PX: u32 = 512;
const DETAIL_VIEWER_CONVERT_THRESHOLD_PX: u32 = 1024;
const DETAIL_COMPACT_RENDER_MAX_EDGE_PX: u32 = 384;

static COVER_ART_MEMORY_CACHE_BUDGET_BYTES: AtomicU64 =
    AtomicU64::new(DEFAULT_IMAGE_MEMORY_CACHE_MAX_BYTES);
static ARTIST_IMAGE_MEMORY_CACHE_BUDGET_BYTES: AtomicU64 =
    AtomicU64::new(DEFAULT_IMAGE_MEMORY_CACHE_MAX_BYTES);

#[derive(Default)]
struct PathImageCache {
    entries: HashMap<PathBuf, (Image, u64)>,
    lru: VecDeque<PathBuf>,
    total_bytes: u64,
}

impl PathImageCache {
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

    fn insert(&mut self, path: PathBuf, image: Image, approx_bytes: u64, max_bytes: u64) {
        if let Some((_, existing_bytes)) = self.entries.remove(&path) {
            self.total_bytes = self.total_bytes.saturating_sub(existing_bytes);
        }
        self.touch_lru(&path);
        self.total_bytes = self.total_bytes.saturating_add(approx_bytes);
        self.entries.insert(path, (image, approx_bytes));

        while self.total_bytes > max_bytes || self.entries.len() > IMAGE_CACHE_MAX_ENTRIES {
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

    fn remove(&mut self, path: &Path) {
        if !self.paths.remove(path) {
            return;
        }
        self.order.retain(|candidate| candidate != path);
    }

    fn clear(&mut self) {
        self.paths.clear();
        self.order.clear();
    }
}

thread_local! {
    static TRACK_ROW_COVER_ART_IMAGE_CACHE: RefCell<PathImageCache> =
        RefCell::new(PathImageCache::default());
    static TRACK_ROW_ARTIST_IMAGE_CACHE: RefCell<PathImageCache> =
        RefCell::new(PathImageCache::default());
    static TRACK_ROW_COVER_ART_FAILED_PATHS: RefCell<FailedCoverPathCache> =
        RefCell::new(FailedCoverPathCache::default());
}

fn apply_shrink_to_group(
    widths: &mut [u32],
    floors: &[u32],
    indices: &[usize],
    deficit_px: u32,
) -> u32 {
    if deficit_px == 0 || indices.is_empty() {
        return deficit_px;
    }

    let mut total_capacity = 0u32;
    for index in indices {
        total_capacity =
            total_capacity.saturating_add(widths[*index].saturating_sub(floors[*index]));
    }
    if total_capacity == 0 {
        return deficit_px;
    }

    let shrink_budget = deficit_px.min(total_capacity);
    if shrink_budget == total_capacity {
        for index in indices {
            widths[*index] = floors[*index];
        }
        return deficit_px - total_capacity;
    }

    let mut remaining_caps: Vec<u32> = indices
        .iter()
        .map(|index| widths[*index].saturating_sub(floors[*index]))
        .collect();
    let mut active_count = remaining_caps
        .iter()
        .copied()
        .filter(|capacity| *capacity > 0)
        .count();
    if active_count == 0 {
        return deficit_px;
    }
    let mut remaining_budget = shrink_budget;
    let mut cursor = 0usize;
    while remaining_budget > 0 && active_count > 0 {
        if remaining_caps[cursor] > 0 {
            let width_index = indices[cursor];
            widths[width_index] = widths[width_index].saturating_sub(1);
            remaining_caps[cursor] -= 1;
            remaining_budget -= 1;
            if remaining_caps[cursor] == 0 {
                active_count -= 1;
            }
        }
        cursor += 1;
        if cursor >= indices.len() {
            cursor = 0;
        }
    }

    deficit_px.saturating_sub(shrink_budget.saturating_sub(remaining_budget))
}

fn apply_grow_to_group(
    widths: &mut [u32],
    ceilings: &[u32],
    indices: &[usize],
    surplus_px: u32,
) -> u32 {
    if surplus_px == 0 || indices.is_empty() {
        return surplus_px;
    }

    let mut total_capacity = 0u32;
    for index in indices {
        total_capacity =
            total_capacity.saturating_add(ceilings[*index].saturating_sub(widths[*index]));
    }
    if total_capacity == 0 {
        return surplus_px;
    }

    let grow_budget = surplus_px.min(total_capacity);
    if grow_budget == total_capacity {
        for index in indices {
            widths[*index] = ceilings[*index];
        }
        return surplus_px - total_capacity;
    }

    let mut remaining_caps: Vec<u32> = indices
        .iter()
        .map(|index| ceilings[*index].saturating_sub(widths[*index]))
        .collect();
    let mut active_count = remaining_caps
        .iter()
        .copied()
        .filter(|capacity| *capacity > 0)
        .count();
    if active_count == 0 {
        return surplus_px;
    }

    let mut remaining_budget = grow_budget;
    let mut cursor = 0usize;
    while remaining_budget > 0 && active_count > 0 {
        if remaining_caps[cursor] > 0 {
            let width_index = indices[cursor];
            widths[width_index] = widths[width_index].saturating_add(1);
            remaining_caps[cursor] -= 1;
            remaining_budget -= 1;
            if remaining_caps[cursor] == 0 {
                active_count -= 1;
            }
        }
        cursor += 1;
        if cursor >= indices.len() {
            cursor = 0;
        }
    }

    surplus_px.saturating_sub(grow_budget.saturating_sub(remaining_budget))
}

fn shrink_widths_to_floors_by_priority(
    widths: &mut [u32],
    floors: &[u32],
    priorities: &[u8],
    fixed_width_flags: &[bool],
    mut deficit_px: u32,
) -> u32 {
    if deficit_px == 0 {
        return 0;
    }

    let mut priority_levels: Vec<u8> = priorities
        .iter()
        .enumerate()
        .filter_map(|(index, priority)| (!fixed_width_flags[index]).then_some(*priority))
        .collect();
    priority_levels.sort_unstable();
    priority_levels.dedup();

    for priority in priority_levels {
        if deficit_px == 0 {
            break;
        }
        let indices: Vec<usize> = priorities
            .iter()
            .enumerate()
            .filter_map(|(index, level)| {
                (!fixed_width_flags[index] && *level == priority).then_some(index)
            })
            .collect();
        deficit_px = apply_shrink_to_group(widths, floors, &indices, deficit_px);
    }

    deficit_px
}

fn grow_widths_to_ceilings_by_priority(
    widths: &mut [u32],
    ceilings: &[u32],
    priorities: &[u8],
    fixed_width_flags: &[bool],
    mut surplus_px: u32,
) -> u32 {
    if surplus_px == 0 {
        return 0;
    }

    let mut priority_levels: Vec<u8> = priorities
        .iter()
        .enumerate()
        .filter_map(|(index, priority)| (!fixed_width_flags[index]).then_some(*priority))
        .collect();
    priority_levels.sort_unstable();
    priority_levels.dedup();
    priority_levels.reverse();

    for priority in priority_levels {
        if surplus_px == 0 {
            break;
        }
        let indices: Vec<usize> = priorities
            .iter()
            .enumerate()
            .filter_map(|(index, level)| {
                (!fixed_width_flags[index] && *level == priority).then_some(index)
            })
            .collect();
        surplus_px = apply_grow_to_group(widths, ceilings, &indices, surplus_px);
    }

    surplus_px
}

fn fit_column_widths_deterministic(
    specs: &[DeterministicColumnLayoutSpec],
    available_width_px: u32,
) -> Vec<u32> {
    if specs.is_empty() {
        return Vec::new();
    }

    let mut widths: Vec<u32> = specs
        .iter()
        .map(|spec| spec.target_px.clamp(spec.min_px, spec.max_px))
        .collect();
    let total_width: u32 = widths.iter().copied().sum();
    if total_width < available_width_px {
        let ceilings: Vec<u32> = specs
            .iter()
            .map(|spec| {
                if spec.fixed_width {
                    spec.target_px
                } else {
                    spec.max_px.max(spec.target_px)
                }
            })
            .collect();
        let priorities: Vec<u8> = specs.iter().map(|spec| spec.shrink_priority).collect();
        let fixed_width_flags: Vec<bool> = specs.iter().map(|spec| spec.fixed_width).collect();
        let _ = grow_widths_to_ceilings_by_priority(
            &mut widths,
            &ceilings,
            &priorities,
            &fixed_width_flags,
            available_width_px - total_width,
        );
        return widths;
    }
    if total_width == available_width_px {
        return widths;
    }

    let mut deficit_px = total_width - available_width_px;
    let semantic_floors: Vec<u32> = specs
        .iter()
        .map(|spec| {
            if spec.fixed_width {
                spec.target_px
            } else {
                spec.semantic_floor_px
                    .clamp(spec.emergency_floor_px, spec.target_px)
            }
        })
        .collect();
    let priorities: Vec<u8> = specs.iter().map(|spec| spec.shrink_priority).collect();
    let fixed_width_flags: Vec<bool> = specs.iter().map(|spec| spec.fixed_width).collect();
    deficit_px = shrink_widths_to_floors_by_priority(
        &mut widths,
        &semantic_floors,
        &priorities,
        &fixed_width_flags,
        deficit_px,
    );

    if deficit_px == 0 {
        return widths;
    }

    let emergency_floors: Vec<u32> = specs
        .iter()
        .map(|spec| {
            if spec.fixed_width {
                spec.target_px
            } else {
                spec.emergency_floor_px.min(spec.target_px)
            }
        })
        .collect();
    let _ = shrink_widths_to_floors_by_priority(
        &mut widths,
        &emergency_floors,
        &priorities,
        &fixed_width_flags,
        deficit_px,
    );

    widths
}

impl UiManager {
    fn covers_cache_dir() -> Option<PathBuf> {
        image_pipeline::cover_originals_dir()
    }

    fn is_managed_cached_image_path(path: &Path) -> bool {
        let in_cover_cache = image_pipeline::kind_cache_root(ManagedImageKind::CoverArt)
            .map(|cache_dir| path.starts_with(cache_dir))
            .unwrap_or(false);
        let in_enrichment_cache = image_pipeline::kind_cache_root(ManagedImageKind::ArtistImage)
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

    fn clear_failed_cover_path(path: &Path) {
        TRACK_ROW_COVER_ART_FAILED_PATHS.with(|failed| {
            failed.borrow_mut().remove(path);
        });
    }

    fn source_badge_for_track_path(path: &Path) -> String {
        if is_remote_track_path(path) {
            "opensubsonic".to_string()
        } else {
            String::new()
        }
    }

    fn normalize_favorite_component(value: &str) -> String {
        value.trim().to_ascii_lowercase()
    }

    fn favorite_key_for_track_path(path: &Path) -> String {
        if let Some(locator) = parse_opensubsonic_track_uri(path) {
            return format!("os:{}:song:{}", locator.profile_id, locator.song_id);
        }
        if path.is_absolute() {
            return format!("file:{}", path.to_string_lossy());
        }
        format!("uri:{}", path.to_string_lossy())
    }

    fn favorite_key_for_artist(artist: &str) -> String {
        format!("artist:{}", Self::normalize_favorite_component(artist))
    }

    fn favorite_key_for_album(album: &str, album_artist: &str) -> String {
        format!(
            "album:{}\u{001f}{}",
            Self::normalize_favorite_component(album),
            Self::normalize_favorite_component(album_artist)
        )
    }

    fn item_kind_for_favorite_category(kind: protocol::FavoriteEntityKind) -> i32 {
        match kind {
            protocol::FavoriteEntityKind::Track => LIBRARY_ITEM_KIND_SONG,
            protocol::FavoriteEntityKind::Artist => LIBRARY_ITEM_KIND_ARTIST,
            protocol::FavoriteEntityKind::Album => LIBRARY_ITEM_KIND_ALBUM,
        }
    }

    fn library_view_supports_artist_enrichment_prefetch(view: &LibraryViewState) -> bool {
        matches!(
            view,
            LibraryViewState::ArtistsRoot | LibraryViewState::FavoriteArtists
        )
    }

    fn favorite_entity_for_library_entry(
        &self,
        entry: &LibraryEntry,
    ) -> Option<protocol::FavoriteEntityRef> {
        match entry {
            LibraryEntry::Track(track) => {
                let (remote_profile_id, remote_item_id) =
                    if let Some(locator) = parse_opensubsonic_track_uri(track.path.as_path()) {
                        (Some(locator.profile_id), Some(locator.song_id))
                    } else {
                        (None, None)
                    };
                Some(protocol::FavoriteEntityRef {
                    kind: protocol::FavoriteEntityKind::Track,
                    entity_key: Self::favorite_key_for_track_path(track.path.as_path()),
                    display_primary: track.title.clone(),
                    display_secondary: format!("{} · {}", track.artist, track.album),
                    track_path: Some(track.path.clone()),
                    remote_profile_id,
                    remote_item_id,
                })
            }
            LibraryEntry::Artist(artist) => Some(protocol::FavoriteEntityRef {
                kind: protocol::FavoriteEntityKind::Artist,
                entity_key: Self::favorite_key_for_artist(&artist.artist),
                display_primary: artist.artist.clone(),
                display_secondary: String::new(),
                track_path: None,
                remote_profile_id: None,
                remote_item_id: None,
            }),
            LibraryEntry::Album(album) => Some(protocol::FavoriteEntityRef {
                kind: protocol::FavoriteEntityKind::Album,
                entity_key: Self::favorite_key_for_album(&album.album, &album.album_artist),
                display_primary: album.album.clone(),
                display_secondary: album.album_artist.clone(),
                track_path: None,
                remote_profile_id: None,
                remote_item_id: None,
            }),
            LibraryEntry::Genre(_)
            | LibraryEntry::Decade(_)
            | LibraryEntry::FavoriteCategory(_) => None,
        }
    }

    fn favorite_entity_for_playlist_source_index(
        &self,
        source_index: usize,
    ) -> Option<protocol::FavoriteEntityRef> {
        let track_path = self.track_paths.get(source_index)?.clone();
        let metadata = self.track_metadata.get(source_index)?;
        let (remote_profile_id, remote_item_id) =
            if let Some(locator) = parse_opensubsonic_track_uri(track_path.as_path()) {
                (Some(locator.profile_id), Some(locator.song_id))
            } else {
                (None, None)
            };
        Some(protocol::FavoriteEntityRef {
            kind: protocol::FavoriteEntityKind::Track,
            entity_key: Self::favorite_key_for_track_path(track_path.as_path()),
            display_primary: metadata.title.clone(),
            display_secondary: format!("{} · {}", metadata.artist, metadata.album),
            track_path: Some(track_path),
            remote_profile_id,
            remote_item_id,
        })
    }

    fn current_track_favorite_entity(&self) -> Option<protocol::FavoriteEntityRef> {
        let playing_path = self.playing_track.path.as_ref()?;
        let (remote_profile_id, remote_item_id) =
            if let Some(locator) = parse_opensubsonic_track_uri(playing_path.as_path()) {
                (Some(locator.profile_id), Some(locator.song_id))
            } else {
                (None, None)
            };
        let metadata = self.playing_track.metadata.as_ref();
        Some(protocol::FavoriteEntityRef {
            kind: protocol::FavoriteEntityKind::Track,
            entity_key: Self::favorite_key_for_track_path(playing_path.as_path()),
            display_primary: metadata
                .map(|m| m.title.clone())
                .unwrap_or_else(|| "Unknown Title".to_string()),
            display_secondary: metadata
                .map(|m| format!("{} · {}", m.artist, m.album))
                .unwrap_or_default(),
            track_path: Some(playing_path.clone()),
            remote_profile_id,
            remote_item_id,
        })
    }

    fn is_pure_remote_playlist_id(playlist_id: &str) -> bool {
        playlist_id
            .strip_prefix("remote:opensubsonic:")
            .and_then(|suffix| suffix.split_once(':'))
            .map(|(profile_id, remote_playlist_id)| {
                !profile_id.trim().is_empty() && !remote_playlist_id.trim().is_empty()
            })
            .unwrap_or(false)
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
        let mut candidates = Vec::new();
        if let Some(normalized) = image_pipeline::cover_original_path_for_track(track_path) {
            candidates.push(normalized);
        }
        let cache_dir = image_pipeline::kind_cache_root(ManagedImageKind::CoverArt)?;
        let stem = Self::embedded_art_cache_stem(track_path)?;
        for ext in ["png", "jpg", "jpeg", "webp", "gif", "bmp"] {
            candidates.push(cache_dir.join(format!("{stem}.{ext}")));
        }
        Some(candidates)
    }

    fn embedded_art_cache_path_if_present(track_path: &Path) -> Option<PathBuf> {
        let candidates = Self::embedded_art_cache_candidates(track_path)?;
        let normalized_path = image_pipeline::cover_original_path_for_track(track_path);
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
            if normalized_path
                .as_ref()
                .is_some_and(|normalized| cache_path != *normalized)
            {
                if let Some(migrated) =
                    Self::migrate_legacy_embedded_art_cache(track_path, &cache_path)
                {
                    return Some(migrated);
                }
            }
            Self::clear_failed_cover_path(&cache_path);
            return Some(cache_path);
        }
        None
    }

    fn migrate_legacy_embedded_art_cache(track_path: &Path, legacy_path: &Path) -> Option<PathBuf> {
        let source_key = track_path.to_string_lossy().to_string();
        let legacy_bytes = std::fs::read(legacy_path).ok()?;
        let normalized_path = image_pipeline::normalize_and_cache_original_bytes(
            ManagedImageKind::CoverArt,
            source_key.as_str(),
            &legacy_bytes,
        )?;
        if normalized_path != legacy_path {
            let _ = std::fs::remove_file(legacy_path);
        }
        let _ = image_pipeline::prune_kind_disk_cache(
            ManagedImageKind::CoverArt,
            image_pipeline::runtime_cover_disk_budget_bytes(),
        );
        Some(normalized_path)
    }

    fn make_opensubsonic_salt() -> String {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes);
        bytes.iter().map(|value| format!("{value:02x}")).collect()
    }

    fn opensubsonic_cover_art_url(
        locator: &crate::integration_uri::OpenSubsonicTrackLocator,
        password: &str,
    ) -> String {
        let salt = Self::make_opensubsonic_salt();
        let token = format!("{:x}", md5::compute(format!("{}{}", password, salt)));
        format!(
            "{}/rest/getCoverArt.view?u={}&t={}&s={}&v={}&c={}&id={}",
            locator.endpoint.trim().trim_end_matches('/'),
            urlencoding::encode(locator.username.trim()),
            token,
            salt,
            OPENSUBSONIC_API_VERSION,
            OPENSUBSONIC_CLIENT_ID,
            urlencoding::encode(locator.song_id.as_str()),
        )
    }

    fn cache_cover_art_bytes(track_path: &Path, cover_bytes: &[u8]) -> Option<PathBuf> {
        Self::detect_image_extension(cover_bytes)?;
        let source_key = track_path.to_string_lossy().to_string();
        let cache_path = image_pipeline::normalize_and_cache_original_bytes(
            ManagedImageKind::CoverArt,
            source_key.as_str(),
            cover_bytes,
        )?;
        if Self::failed_cover_path(&cache_path) {
            return None;
        }
        let _ = image_pipeline::prune_kind_disk_cache(
            ManagedImageKind::CoverArt,
            image_pipeline::runtime_cover_disk_budget_bytes(),
        );
        Some(cache_path)
    }

    fn fetch_remote_cover_art(
        track_path: &Path,
        password_cache: &mut HashMap<String, Option<String>>,
    ) -> Option<PathBuf> {
        if let Some(cached) = Self::embedded_art_cache_path_if_present(track_path) {
            return Some(cached);
        }
        let locator = parse_opensubsonic_track_uri(track_path)?;
        let password = if let Some(cached) = password_cache.get(&locator.profile_id) {
            cached.clone()
        } else {
            match get_opensubsonic_password(&locator.profile_id) {
                Ok(password) => {
                    password_cache.insert(locator.profile_id.clone(), password.clone());
                    password
                }
                Err(error) => {
                    warn!(
                        "Failed to read OpenSubsonic credential for cover art (profile '{}'): {}",
                        locator.profile_id, error
                    );
                    password_cache.insert(locator.profile_id.clone(), None);
                    None
                }
            }
        }?;

        let url = Self::opensubsonic_cover_art_url(&locator, password.as_str());
        let client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(20))
            .timeout_write(Duration::from_secs(20))
            .build();
        let response = client.get(url.as_str()).call().ok()?;
        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body).ok()?;
        if body.is_empty() {
            return None;
        }
        let preview = String::from_utf8_lossy(&body[..body.len().min(512)]).to_ascii_lowercase();
        if preview.contains("subsonic-response")
            || preview.contains("<error")
            || preview.contains("\"error\"")
        {
            return None;
        }
        Self::cache_cover_art_bytes(track_path, &body)
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
        tracks: Vec<protocol::LibraryTrack>,
    ) -> Vec<LibraryEntry> {
        let mut tracks_by_album: HashMap<(String, String), Vec<protocol::LibraryTrack>> =
            HashMap::new();
        let mut album_year_by_key: HashMap<(String, String), i32> = HashMap::new();

        for track in tracks {
            let key = (track.album.clone(), track.album_artist.clone());
            if let Some(year) = Self::parse_library_year(&track.year) {
                album_year_by_key
                    .entry(key.clone())
                    .and_modify(|current_year| *current_year = (*current_year).max(year))
                    .or_insert(year);
            }
            tracks_by_album.entry(key).or_default().push(track);
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

        for (key, album_tracks) in &tracks_by_album {
            if seen_album_keys.contains(key) {
                continue;
            }
            let synthetic_album = protocol::LibraryAlbum {
                album: key.0.clone(),
                album_artist: key.1.clone(),
                track_count: album_tracks.len() as u32,
                representative_track_path: album_tracks.first().map(|track| track.path.clone()),
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
            if let Some(album_tracks) = tracks_by_album.remove(&key) {
                entries.extend(album_tracks.into_iter().map(LibraryEntry::Track));
            }
        }

        if !tracks_by_album.is_empty() {
            let mut remaining_tracks: Vec<protocol::LibraryTrack> =
                tracks_by_album.into_values().flatten().collect();
            remaining_tracks.sort_by(|left, right| {
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
            entries.extend(remaining_tracks.into_iter().map(LibraryEntry::Track));
        }

        entries
    }

    fn build_global_search_entries(
        tracks: Vec<protocol::LibraryTrack>,
        artists: Vec<protocol::LibraryArtist>,
        albums: Vec<protocol::LibraryAlbum>,
    ) -> Vec<LibraryEntry> {
        let mut entries = Vec::with_capacity(tracks.len() + artists.len() + albums.len());
        entries.extend(tracks.into_iter().map(LibraryEntry::Track));
        entries.extend(artists.into_iter().map(LibraryEntry::Artist));
        entries.extend(albums.into_iter().map(LibraryEntry::Album));
        entries.sort_by(|left, right| {
            let left_key = match left {
                LibraryEntry::Track(track) => track.title.to_ascii_lowercase(),
                LibraryEntry::Artist(artist) => artist.artist.to_ascii_lowercase(),
                LibraryEntry::Album(album) => album.album.to_ascii_lowercase(),
                LibraryEntry::Genre(genre) => genre.genre.to_ascii_lowercase(),
                LibraryEntry::Decade(decade) => decade.decade.to_ascii_lowercase(),
                LibraryEntry::FavoriteCategory(category) => category.title.to_ascii_lowercase(),
            };
            let right_key = match right {
                LibraryEntry::Track(track) => track.title.to_ascii_lowercase(),
                LibraryEntry::Artist(artist) => artist.artist.to_ascii_lowercase(),
                LibraryEntry::Album(album) => album.album.to_ascii_lowercase(),
                LibraryEntry::Genre(genre) => genre.genre.to_ascii_lowercase(),
                LibraryEntry::Decade(decade) => decade.decade.to_ascii_lowercase(),
                LibraryEntry::FavoriteCategory(category) => category.title.to_ascii_lowercase(),
            };
            let left_kind_rank = match left {
                LibraryEntry::Artist(_) => 0,
                LibraryEntry::Album(_) => 1,
                LibraryEntry::Track(_) => 2,
                LibraryEntry::Genre(_) => 3,
                LibraryEntry::Decade(_) => 4,
                LibraryEntry::FavoriteCategory(_) => 5,
            };
            let right_kind_rank = match right {
                LibraryEntry::Artist(_) => 0,
                LibraryEntry::Album(_) => 1,
                LibraryEntry::Track(_) => 2,
                LibraryEntry::Genre(_) => 3,
                LibraryEntry::Decade(_) => 4,
                LibraryEntry::FavoriteCategory(_) => 5,
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

    fn drain_list_image_prepare_requests(
        first: ListImagePrepareRequest,
        request_rx: &StdReceiver<ListImagePrepareRequest>,
    ) -> Vec<ListImagePrepareRequest> {
        let mut pending = vec![first];
        while let Ok(next) = request_rx.try_recv() {
            pending.push(next);
            if pending.len() >= 256 {
                break;
            }
        }
        pending
    }

    fn drain_embedded_cover_art_prepare_requests(
        first: EmbeddedCoverArtPrepareRequest,
        request_rx: &StdReceiver<EmbeddedCoverArtPrepareRequest>,
    ) -> Vec<EmbeddedCoverArtPrepareRequest> {
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
        initial_ui_config: config::UiConfig,
        initial_library_config: config::LibraryConfig,
        library_scan_progress_rx: StdReceiver<protocol::LibraryMessage>,
    ) -> Self {
        let (cover_art_lookup_tx, cover_art_lookup_rx) = mpsc::channel::<CoverArtLookupRequest>();
        let cover_art_bus_sender = bus_sender.clone();
        thread::spawn(move || {
            let mut opensubsonic_password_cache: HashMap<String, Option<String>> = HashMap::new();
            while let Ok(request) = cover_art_lookup_rx.recv() {
                let latest_request =
                    UiManager::coalesce_cover_art_requests(request, &cover_art_lookup_rx);
                let cover_art_path = latest_request.track_path.as_ref().and_then(|path| {
                    UiManager::find_cover_art(path.as_path(), &mut opensubsonic_password_cache)
                });
                let _ = cover_art_bus_sender.send(protocol::Message::Playback(
                    protocol::PlaybackMessage::CoverArtChanged(cover_art_path),
                ));
            }
        });
        let (list_image_prepare_tx, list_image_prepare_rx) =
            mpsc::channel::<ListImagePrepareRequest>();
        let list_image_bus_sender = bus_sender.clone();
        thread::spawn(move || {
            while let Ok(request) = list_image_prepare_rx.recv() {
                let pending =
                    UiManager::drain_list_image_prepare_requests(request, &list_image_prepare_rx);
                let mut deduped: HashSet<(
                    PathBuf,
                    protocol::UiImageKind,
                    protocol::UiImageVariant,
                    u32,
                )> = HashSet::new();
                for item in pending {
                    deduped.insert((item.source_path, item.kind, item.variant, item.max_edge_px));
                }
                for (source_path, kind, variant, max_edge_px) in deduped {
                    if variant != protocol::UiImageVariant::ListThumb {
                        continue;
                    }
                    let managed_kind = match kind {
                        protocol::UiImageKind::CoverArt => ManagedImageKind::CoverArt,
                        protocol::UiImageKind::ArtistImage => ManagedImageKind::ArtistImage,
                    };
                    if image_pipeline::ensure_list_thumbnail(
                        managed_kind,
                        &source_path,
                        max_edge_px,
                    )
                    .is_none()
                    {
                        continue;
                    }
                    let max_disk_bytes = match managed_kind {
                        ManagedImageKind::CoverArt => {
                            image_pipeline::runtime_cover_disk_budget_bytes()
                        }
                        ManagedImageKind::ArtistImage => {
                            image_pipeline::runtime_artist_disk_budget_bytes()
                        }
                    };
                    let _ = image_pipeline::prune_kind_disk_cache(managed_kind, max_disk_bytes);
                    let _ = list_image_bus_sender.send(protocol::Message::Playback(
                        protocol::PlaybackMessage::ListImageReady {
                            source_path: source_path.clone(),
                            kind,
                            variant,
                        },
                    ));
                }
            }
        });
        let (embedded_cover_art_prepare_tx, embedded_cover_art_prepare_rx) =
            mpsc::channel::<EmbeddedCoverArtPrepareRequest>();
        let embedded_cover_art_bus_sender = bus_sender.clone();
        thread::spawn(move || {
            while let Ok(request) = embedded_cover_art_prepare_rx.recv() {
                let pending = UiManager::drain_embedded_cover_art_prepare_requests(
                    request,
                    &embedded_cover_art_prepare_rx,
                );
                let mut deduped: HashSet<(PathBuf, u32)> = HashSet::new();
                for item in pending {
                    deduped.insert((item.track_path, item.max_edge_px));
                }
                for (track_path, max_edge_px) in deduped {
                    if is_remote_track_path(track_path.as_path()) {
                        continue;
                    }
                    let Some(cover_art_path) =
                        UiManager::extract_embedded_art(track_path.as_path())
                    else {
                        continue;
                    };
                    if image_pipeline::ensure_list_thumbnail(
                        ManagedImageKind::CoverArt,
                        cover_art_path.as_path(),
                        max_edge_px,
                    )
                    .is_none()
                    {
                        continue;
                    }
                    let _ = image_pipeline::prune_kind_disk_cache(
                        ManagedImageKind::CoverArt,
                        image_pipeline::runtime_cover_disk_budget_bytes(),
                    );
                    let _ = embedded_cover_art_bus_sender.send(protocol::Message::Playback(
                        protocol::PlaybackMessage::ListImageReady {
                            source_path: cover_art_path,
                            kind: protocol::UiImageKind::CoverArt,
                            variant: protocol::UiImageVariant::ListThumb,
                        },
                    ));
                }
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

        let initial_layout_width_overrides = initial_ui_config
            .layout
            .playlist_column_width_overrides
            .clone();

        let mut manager = Self {
            ui: ui.clone(),
            bus_receiver,
            bus_sender,
            library_scan_progress_rx,
            cover_art_lookup_tx,
            list_image_prepare_tx,
            embedded_cover_art_prepare_tx,
            metadata_lookup_tx,
            last_cover_art_lookup_path: None,
            active_playlist_id: String::new(),
            playlist_ids: Vec::new(),
            playlist_names: Vec::new(),
            opensubsonic_sync_eligible_playlist_ids: HashSet::new(),
            unavailable_track_ids: HashSet::new(),
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
            library_playing_index: None,
            drag_indices: Vec::new(),
            is_dragging: false,
            pressed_index: None,
            pending_single_select_on_click: None,
            progress_rl: RateLimiter::direct(
                Quota::with_period(Duration::from_millis(30)).unwrap(),
            ),
            last_elapsed_ms: 0,
            last_total_ms: 0,
            playing_track: PlayingTrackState::default(),
            favorites_by_key: HashMap::new(),
            display_target_priority: DisplayTargetPriority::Playing,
            current_technical_metadata: None,
            current_output_path_info: None,
            cast_connected: false,
            cast_connecting: false,
            cast_discovering: false,
            cast_device_name: String::new(),
            cast_playback_path_kind: None,
            cast_transcode_output_metadata: None,
            cast_device_ids: Vec::new(),
            cast_device_names: Vec::new(),
            playlist_columns: initial_ui_config.playlist_columns.clone(),
            playlist_column_content_targets_px: Vec::new(),
            playlist_column_target_widths_px: HashMap::new(),
            playlist_column_widths_px: Vec::new(),
            playlist_column_width_overrides_px: HashMap::new(),
            playlist_columns_available_width_px: 0,
            playlist_columns_content_width_px: 0,
            playlist_row_height_px: BASE_ROW_HEIGHT_PX,
            album_art_column_min_width_px: initial_ui_config.playlist_album_art_column_min_width_px,
            album_art_column_max_width_px: initial_ui_config.playlist_album_art_column_max_width_px,
            filter_sort_column_key: None,
            filter_sort_direction: None,
            filter_search_query: String::new(),
            filter_search_visible: false,
            auto_scroll_to_playing_track: initial_ui_config.auto_scroll_to_playing_track,
            playlist_prefetch_first_row: 0,
            playlist_prefetch_row_count: 0,
            playlist_scroll_center_token: 0,
            playback_active: false,
            processed_message_count: 0,
            lagged_message_count: 0,
            last_message_at: Instant::now(),
            last_progress_at: None,
            last_health_log_at: Instant::now(),
            collection_mode: COLLECTION_MODE_PLAYLIST,
            library_view_stack: vec![LibraryViewState::TracksRoot],
            library_entries: Vec::new(),
            library_view_indices: Vec::new(),
            library_selected_indices: Vec::new(),
            library_selection_anchor: None,
            library_root_counts: [0; 6],
            library_cover_art_paths: HashMap::new(),
            folder_cover_art_paths: HashMap::new(),
            library_enrichment: HashMap::new(),
            library_enrichment_pending: HashSet::new(),
            library_enrichment_last_request_at: HashMap::new(),
            library_enrichment_retry_counts: HashMap::new(),
            library_enrichment_retry_not_before: HashMap::new(),
            library_enrichment_not_found_not_before: HashMap::new(),
            library_enrichment_failed_attempt_counts: HashMap::new(),
            library_last_detail_enrichment_entity: None,
            library_online_metadata_enabled: initial_library_config.online_metadata_enabled,
            library_online_metadata_prompt_pending: initial_library_config
                .online_metadata_prompt_pending,
            list_image_max_edge_px: initial_library_config.list_image_max_edge_px.max(1),
            cover_art_cache_max_size_mb: initial_library_config.cover_art_cache_max_size_mb.max(1),
            artist_image_cache_max_size_mb: initial_library_config
                .artist_image_cache_max_size_mb
                .max(1),
            cover_art_memory_cache_max_size_mb: initial_library_config
                .cover_art_memory_cache_max_size_mb
                .max(1),
            artist_image_memory_cache_max_size_mb: initial_library_config
                .artist_image_memory_cache_max_size_mb
                .max(1),
            pending_list_image_requests: HashSet::new(),
            library_artist_prefetch_first_row: 0,
            library_artist_prefetch_row_count: 0,
            library_scroll_positions: HashMap::new(),
            pending_library_scroll_restore_row: None,
            library_scroll_restore_token: 0,
            library_scroll_center_token: 0,
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
            pending_library_remove_selections: Vec::new(),
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
        };
        // Seed column-width overrides from startup layout so playlist rendering does not depend on
        // racing the asynchronous `ConfigLoaded` bus message.
        manager.apply_layout_column_width_overrides(&initial_layout_width_overrides);
        manager.refresh_playlist_column_content_targets();
        manager.apply_playlist_column_layout();
        manager
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

    fn format_rate_hz_text(rate_hz: u32) -> String {
        if rate_hz >= 1000 {
            let khz = rate_hz as f32 / 1000.0;
            if khz == khz.round() {
                format!("{}kHz", khz as u32)
            } else {
                format!("{:.1}kHz", khz)
            }
        } else {
            format!("{}Hz", rate_hz)
        }
    }

    fn format_technical_metadata_text(meta: &protocol::TechnicalMetadata) -> String {
        format!(
            "{} ({} bit, {}, {}ch, {}kbps)",
            meta.format,
            meta.bits_per_sample,
            Self::format_rate_hz_text(meta.sample_rate_hz),
            meta.channel_count,
            meta.bitrate_kbps
        )
    }

    fn current_track_source_label(&self) -> Option<&'static str> {
        self.playing_track
            .path
            .as_ref()
            .and_then(|path| is_remote_track_path(path.as_path()).then_some("OpenSubsonic"))
    }

    fn render_local_transform_text(&self) -> String {
        let Some(path_info) = self.current_output_path_info.as_ref() else {
            return "Direct play".to_string();
        };
        let mut transforms = Vec::new();
        if path_info.resampled {
            transforms.push(format!(
                "Resample: {} -> {}",
                Self::format_rate_hz_text(path_info.source_sample_rate_hz),
                Self::format_rate_hz_text(path_info.output_stream.sample_rate_hz)
            ));
        }
        if let Some(channel_transform) = path_info.channel_transform {
            let label = match channel_transform {
                protocol::ChannelTransformKind::Downmix => "Downmix",
                protocol::ChannelTransformKind::ChannelMap => "Channel map",
            };
            transforms.push(format!(
                "{}: {}ch -> {}ch",
                label, path_info.source_channel_count, path_info.output_stream.channel_count
            ));
        }
        if path_info.dithered {
            transforms.push("Dither".to_string());
        }
        if transforms.is_empty() {
            "Direct play".to_string()
        } else {
            transforms.join(" / ")
        }
    }

    fn render_technical_info_text(&self) -> String {
        if self.current_technical_metadata.is_none()
            && !self.cast_connected
            && !self.cast_connecting
        {
            return String::new();
        }

        let mut sections = Vec::new();
        let source_section = if let Some(meta) = self.current_technical_metadata.as_ref() {
            let source_label = self.current_track_source_label();
            let source_format_text = Self::format_technical_metadata_text(meta);
            if let Some(source_label) = source_label {
                format!("Source: {} | {}", source_label, source_format_text)
            } else {
                format!("Source: {}", source_format_text)
            }
        } else if self.cast_connected || self.cast_connecting {
            "Source: Unknown".to_string()
        } else {
            String::new()
        };
        if !source_section.is_empty() {
            sections.push(source_section);
        }

        if self.cast_connected {
            sections.push("Casting".to_string());
            let cast_transform = match self.cast_playback_path_kind {
                Some(protocol::CastPlaybackPathKind::Direct) => Some("Direct play".to_string()),
                Some(protocol::CastPlaybackPathKind::TranscodeWavPcm) => {
                    if let Some(meta) = self.cast_transcode_output_metadata.as_ref() {
                        Some(format!(
                            "Transcode: {}",
                            Self::format_technical_metadata_text(meta)
                        ))
                    } else {
                        Some("Transcode: WAV".to_string())
                    }
                }
                None => None,
            };
            if let Some(cast_transform) = cast_transform {
                sections.push(cast_transform);
            }
        } else if self.cast_connecting {
            sections.push("Casting: Connecting...".to_string());
        } else {
            sections.push(self.render_local_transform_text());
        }

        sections
            .into_iter()
            .filter(|section| !section.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn refresh_technical_info_ui(&self) {
        let info_text = self.render_technical_info_text();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_technical_info(info_text.into());
        });
    }

    fn sync_cast_state_to_ui(&self) {
        let connected = self.cast_connected;
        let connecting = self.cast_connecting;
        let discovering = self.cast_discovering;
        let label = if connected {
            if self.cast_device_name.is_empty() {
                "Connected".to_string()
            } else {
                format!("Connected: {}", self.cast_device_name)
            }
        } else if connecting {
            "Connecting...".to_string()
        } else if discovering {
            "Searching...".to_string()
        } else {
            "Not Connected".to_string()
        };
        let device_names = self
            .cast_device_names
            .iter()
            .cloned()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>();
        let device_ids = self
            .cast_device_ids
            .iter()
            .cloned()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_cast_connected(connected);
            ui.set_cast_connecting(connecting);
            ui.set_cast_connection_label(label.into());
            ui.set_cast_device_names(ModelRc::from(Rc::new(VecModel::from(device_names))));
            ui.set_cast_device_ids(ModelRc::from(Rc::new(VecModel::from(device_ids))));
        });
    }

    fn find_local_cover_art(track_path: &Path) -> Option<PathBuf> {
        Self::find_external_cover_art(track_path).or_else(|| Self::extract_embedded_art(track_path))
    }

    fn find_cover_art(
        track_path: &Path,
        password_cache: &mut HashMap<String, Option<String>>,
    ) -> Option<PathBuf> {
        if is_remote_track_path(track_path) {
            return Self::fetch_remote_cover_art(track_path, password_cache);
        }
        Self::find_local_cover_art(track_path)
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
        Self::cache_cover_art_bytes(track_path, &cover_bytes)
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
        let is_remote = is_remote_track_path(track_path.as_path());
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

        let resolved_path = if is_remote {
            self.update_cover_art(Some(&track_path));
            None
        } else {
            self.find_external_cover_art_cached(&track_path)
                .or_else(|| Self::embedded_art_cache_path_if_present(&track_path))
        };
        if resolved_path.is_none() && !is_remote {
            // Cover-file lookup already failed; proactively process embedded art in background.
            self.queue_embedded_cover_art_prepare(track_path.as_path());
        }
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

    fn playlist_cover_decode_window(&self, total_rows: usize) -> (usize, usize) {
        if total_rows == 0 {
            return (0, 0);
        }
        let default_rows = 24usize;
        let lookahead_rows = 24usize;
        let row_count = if self.playlist_prefetch_row_count == 0 {
            default_rows
        } else {
            self.playlist_prefetch_row_count.min(128)
        };
        let start = self
            .playlist_prefetch_first_row
            .saturating_sub(6)
            .min(total_rows);
        let end = start
            .saturating_add(row_count)
            .saturating_add(lookahead_rows)
            .min(total_rows);
        (start, end)
    }

    fn managed_kind_for_ui_kind(kind: protocol::UiImageKind) -> ManagedImageKind {
        match kind {
            protocol::UiImageKind::CoverArt => ManagedImageKind::CoverArt,
            protocol::UiImageKind::ArtistImage => ManagedImageKind::ArtistImage,
        }
    }

    fn queue_list_thumbnail_prepare(
        &mut self,
        source_path: &Path,
        kind: protocol::UiImageKind,
        variant: protocol::UiImageVariant,
    ) {
        let request_key = (source_path.to_path_buf(), kind, variant);
        if self.pending_list_image_requests.contains(&request_key) {
            return;
        }
        self.pending_list_image_requests.insert(request_key.clone());
        if self
            .list_image_prepare_tx
            .send(ListImagePrepareRequest {
                source_path: request_key.0.clone(),
                kind,
                variant,
                max_edge_px: self.list_image_max_edge_px,
            })
            .is_err()
        {
            self.pending_list_image_requests.remove(&request_key);
            warn!(
                "Failed queuing list image prepare request for {}",
                source_path.display()
            );
        }
    }

    fn queue_embedded_cover_art_prepare(&mut self, track_path: &Path) {
        if is_remote_track_path(track_path) {
            return;
        }
        if self
            .embedded_cover_art_prepare_tx
            .send(EmbeddedCoverArtPrepareRequest {
                track_path: track_path.to_path_buf(),
                max_edge_px: self.list_image_max_edge_px,
            })
            .is_err()
        {
            warn!(
                "Failed queuing embedded cover art prepare request for {}",
                track_path.display()
            );
        }
    }

    fn list_thumbnail_path_if_ready(
        &mut self,
        source_path: &Path,
        kind: protocol::UiImageKind,
    ) -> Option<PathBuf> {
        let managed_kind = Self::managed_kind_for_ui_kind(kind);
        let thumb_path = image_pipeline::list_thumbnail_path_if_present(
            managed_kind,
            source_path,
            self.list_image_max_edge_px,
        );
        if thumb_path.is_some() {
            if let Some(path) = thumb_path.as_ref() {
                Self::clear_failed_cover_path(path);
            }
            return thumb_path;
        }
        self.queue_list_thumbnail_prepare(source_path, kind, protocol::UiImageVariant::ListThumb);
        None
    }

    fn list_thumbnail_for_library_presentation(
        &mut self,
        presentation: &LibraryRowPresentation,
    ) -> Option<PathBuf> {
        let source_path = presentation.cover_art_path.as_ref()?;
        let kind = if presentation.item_kind == LIBRARY_ITEM_KIND_ARTIST {
            protocol::UiImageKind::ArtistImage
        } else {
            protocol::UiImageKind::CoverArt
        };
        self.list_thumbnail_path_if_ready(source_path.as_path(), kind)
    }

    fn detail_render_path(
        path: &Path,
        kind: protocol::UiImageKind,
        max_edge_px: u32,
        convert_threshold_px: u32,
    ) -> PathBuf {
        let managed_kind = Self::managed_kind_for_ui_kind(kind);
        let resolved = if convert_threshold_px <= max_edge_px {
            image_pipeline::ensure_detail_preview(managed_kind, path, max_edge_px)
        } else {
            image_pipeline::ensure_detail_preview_with_threshold(
                managed_kind,
                path,
                max_edge_px,
                convert_threshold_px,
            )
        };
        resolved.unwrap_or_else(|| path.to_path_buf())
    }

    fn try_load_detail_cover_art_image_with_kind(
        path: &Path,
        kind: protocol::UiImageKind,
        max_edge_px: u32,
        convert_threshold_px: u32,
    ) -> Option<Image> {
        let detail_path = Self::detail_render_path(path, kind, max_edge_px, convert_threshold_px);
        if let Some(image) = Self::try_load_cover_art_image_with_kind(detail_path.as_path(), kind) {
            return Some(image);
        }
        if detail_path.as_path() != path {
            return Self::try_load_cover_art_image_with_kind(path, kind);
        }
        None
    }

    fn approximate_cover_art_size_bytes(path: &Path) -> u64 {
        image_pipeline::decoded_rgba_bytes(path).unwrap_or(256 * 1024)
    }

    fn image_cache_budget_bytes(kind: protocol::UiImageKind) -> u64 {
        match kind {
            protocol::UiImageKind::CoverArt => {
                COVER_ART_MEMORY_CACHE_BUDGET_BYTES.load(Ordering::Relaxed)
            }
            protocol::UiImageKind::ArtistImage => {
                ARTIST_IMAGE_MEMORY_CACHE_BUDGET_BYTES.load(Ordering::Relaxed)
            }
        }
    }

    fn try_load_cover_art_image_with_kind(
        path: &Path,
        kind: protocol::UiImageKind,
    ) -> Option<Image> {
        if Self::failed_cover_path(path) && !path.exists() {
            return None;
        }

        let image_path = path.to_path_buf();
        let cached = match kind {
            protocol::UiImageKind::CoverArt => {
                TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| cache.borrow_mut().get(&image_path))
            }
            protocol::UiImageKind::ArtistImage => {
                TRACK_ROW_ARTIST_IMAGE_CACHE.with(|cache| cache.borrow_mut().get(&image_path))
            }
        };
        if let Some(cached) = cached {
            Self::clear_failed_cover_path(path);
            return Some(cached);
        }

        match Image::load_from_path(path) {
            Ok(image) => {
                Self::clear_failed_cover_path(path);
                let approx_bytes = Self::approximate_cover_art_size_bytes(path);
                let cache_budget = Self::image_cache_budget_bytes(kind);
                match kind {
                    protocol::UiImageKind::CoverArt => {
                        TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| {
                            cache.borrow_mut().insert(
                                image_path.clone(),
                                image.clone(),
                                approx_bytes,
                                cache_budget,
                            );
                        });
                    }
                    protocol::UiImageKind::ArtistImage => {
                        TRACK_ROW_ARTIST_IMAGE_CACHE.with(|cache| {
                            cache.borrow_mut().insert(
                                image_path.clone(),
                                image.clone(),
                                approx_bytes,
                                cache_budget,
                            );
                        });
                    }
                }
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
        path.and_then(|path| {
            Self::try_load_cover_art_image_with_kind(path, protocol::UiImageKind::CoverArt)
        })
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

    fn apply_unavailable_title_override(
        values: &mut [String],
        playlist_columns: &[PlaylistColumnConfig],
    ) {
        for (visible_index, column) in playlist_columns
            .iter()
            .filter(|column| column.enabled)
            .enumerate()
        {
            let normalized_format = Self::normalize_column_format(&column.format);
            let normalized_name = column.name.trim().to_ascii_lowercase();
            if normalized_format == "{title}" || normalized_name == "title" {
                if let Some(value) = values.get_mut(visible_index) {
                    *value = REMOTE_TRACK_UNAVAILABLE_TITLE.to_string();
                }
                break;
            }
        }
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

    fn is_favorite_builtin_column(column: &PlaylistColumnConfig) -> bool {
        !column.custom && Self::normalize_column_format(&column.format) == "{favorite}"
    }

    fn playlist_column_kind(column: &PlaylistColumnConfig) -> i32 {
        if Self::is_album_art_builtin_column(column) {
            PLAYLIST_COLUMN_KIND_ALBUM_ART
        } else if Self::is_favorite_builtin_column(column) {
            PLAYLIST_COLUMN_KIND_FAVORITE
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

    fn apply_layout_column_width_overrides(
        &mut self,
        overrides: &[PlaylistColumnWidthOverrideConfig],
    ) {
        self.playlist_column_width_overrides_px.clear();
        for override_item in overrides {
            let key = override_item.column_key.trim();
            if key.is_empty() {
                continue;
            }
            let key_owned = key.to_string();
            if !self
                .playlist_columns
                .iter()
                .any(|column| Self::playlist_column_key(column) == key_owned)
            {
                continue;
            }
            let clamped_width_px = self.clamp_column_override_width_px(key, override_item.width_px);
            self.playlist_column_width_overrides_px
                .insert(key_owned, clamped_width_px);
        }
    }

    fn is_sortable_playlist_column(column: &PlaylistColumnConfig) -> bool {
        !Self::is_album_art_builtin_column(column) && !Self::is_favorite_builtin_column(column)
    }

    fn playlist_column_class(column: &PlaylistColumnConfig) -> PlaylistColumnClass {
        if Self::is_album_art_builtin_column(column) {
            return PlaylistColumnClass::AlbumArt;
        }
        if Self::is_favorite_builtin_column(column) {
            return PlaylistColumnClass::Favorite;
        }

        let normalized_format = column.format.trim().to_ascii_lowercase();
        let normalized_name = column.name.trim().to_ascii_lowercase();

        if normalized_format == "{track}"
            || normalized_format == "{track_number}"
            || normalized_format == "{tracknumber}"
            || normalized_name == "track #"
            || normalized_name == "track"
        {
            return PlaylistColumnClass::TrackNumber;
        }

        if normalized_format == "{disc}" || normalized_format == "{disc_number}" {
            return PlaylistColumnClass::DiscNumber;
        }

        if normalized_format == "{year}"
            || normalized_format == "{date}"
            || normalized_name == "year"
        {
            return PlaylistColumnClass::YearDate;
        }

        if normalized_format == "{title}" || normalized_name == "title" {
            return PlaylistColumnClass::Title;
        }

        if normalized_format == "{artist}"
            || normalized_format == "{album_artist}"
            || normalized_format == "{albumartist}"
            || normalized_name == "artist"
            || normalized_name == "album artist"
        {
            return PlaylistColumnClass::ArtistFamily;
        }

        if normalized_format == "{album}" || normalized_name == "album" {
            return PlaylistColumnClass::Album;
        }

        if normalized_format == "{genre}" || normalized_name == "genre" {
            return PlaylistColumnClass::Genre;
        }

        if normalized_name == "duration"
            || normalized_name == "time"
            || normalized_format == "{duration}"
        {
            return PlaylistColumnClass::Duration;
        }

        if column.custom {
            return PlaylistColumnClass::Custom;
        }

        PlaylistColumnClass::Generic
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
        match Self::playlist_column_class(column) {
            PlaylistColumnClass::AlbumArt => self.album_art_column_width_profile(),
            PlaylistColumnClass::Favorite => ColumnWidthProfile {
                min_px: 24,
                preferred_px: 24,
                max_px: 24,
            },
            PlaylistColumnClass::TrackNumber => ColumnWidthProfile {
                min_px: 52,
                preferred_px: 68,
                max_px: 90,
            },
            PlaylistColumnClass::DiscNumber => ColumnWidthProfile {
                min_px: 50,
                preferred_px: 64,
                max_px: 84,
            },
            PlaylistColumnClass::YearDate => ColumnWidthProfile {
                min_px: 64,
                preferred_px: 78,
                max_px: 104,
            },
            PlaylistColumnClass::Title => ColumnWidthProfile {
                min_px: 140,
                preferred_px: 230,
                max_px: 440,
            },
            PlaylistColumnClass::ArtistFamily => ColumnWidthProfile {
                min_px: 120,
                preferred_px: 190,
                max_px: 320,
            },
            PlaylistColumnClass::Album => ColumnWidthProfile {
                min_px: 140,
                preferred_px: 220,
                max_px: 360,
            },
            PlaylistColumnClass::Genre => ColumnWidthProfile {
                min_px: 100,
                preferred_px: 140,
                max_px: 210,
            },
            PlaylistColumnClass::Duration => ColumnWidthProfile {
                min_px: 78,
                preferred_px: 92,
                max_px: 120,
            },
            PlaylistColumnClass::Custom => ColumnWidthProfile {
                min_px: 110,
                preferred_px: 180,
                max_px: 340,
            },
            PlaylistColumnClass::Generic => ColumnWidthProfile {
                min_px: 100,
                preferred_px: 170,
                max_px: 300,
            },
        }
    }

    fn column_shrink_priority(class: PlaylistColumnClass) -> u8 {
        match class {
            PlaylistColumnClass::AlbumArt | PlaylistColumnClass::Favorite => 255,
            PlaylistColumnClass::TrackNumber
            | PlaylistColumnClass::DiscNumber
            | PlaylistColumnClass::YearDate
            | PlaylistColumnClass::Duration => 0,
            PlaylistColumnClass::Genre
            | PlaylistColumnClass::Custom
            | PlaylistColumnClass::Generic => 1,
            PlaylistColumnClass::ArtistFamily | PlaylistColumnClass::Album => 2,
            PlaylistColumnClass::Title => 3,
        }
    }

    fn auto_growth_headroom_px(class: PlaylistColumnClass) -> u32 {
        match class {
            PlaylistColumnClass::AlbumArt | PlaylistColumnClass::Favorite => 0,
            PlaylistColumnClass::TrackNumber
            | PlaylistColumnClass::DiscNumber
            | PlaylistColumnClass::YearDate
            | PlaylistColumnClass::Duration => 24,
            PlaylistColumnClass::Genre
            | PlaylistColumnClass::Custom
            | PlaylistColumnClass::Generic => 64,
            PlaylistColumnClass::ArtistFamily | PlaylistColumnClass::Album => 72,
            PlaylistColumnClass::Title => 96,
        }
    }

    fn auto_growth_max_px_for_column(
        class: PlaylistColumnClass,
        profile: ColumnWidthProfile,
        content_target_width_px: u32,
    ) -> u32 {
        if matches!(
            class,
            PlaylistColumnClass::AlbumArt | PlaylistColumnClass::Favorite
        ) {
            return profile.max_px;
        }
        content_target_width_px
            .max(profile.preferred_px)
            .saturating_add(Self::auto_growth_headroom_px(class))
            .clamp(profile.min_px, profile.max_px)
    }

    fn semantic_floor_for_column(class: PlaylistColumnClass, profile: ColumnWidthProfile) -> u32 {
        match class {
            PlaylistColumnClass::AlbumArt => profile.preferred_px,
            PlaylistColumnClass::Favorite => profile.preferred_px,
            PlaylistColumnClass::TrackNumber => profile.min_px.max(52),
            PlaylistColumnClass::DiscNumber => profile.min_px.max(50),
            PlaylistColumnClass::YearDate => profile.min_px.max(64),
            PlaylistColumnClass::Title => profile.min_px.max(140),
            PlaylistColumnClass::ArtistFamily => profile.min_px.max(120),
            PlaylistColumnClass::Album => profile.min_px.max(140),
            PlaylistColumnClass::Genre => profile.min_px.max(100),
            PlaylistColumnClass::Duration => profile.min_px.max(78),
            PlaylistColumnClass::Custom => profile.min_px.max(110),
            PlaylistColumnClass::Generic => profile.min_px.max(100),
        }
        .min(profile.max_px)
    }

    fn emergency_floor_for_column(class: PlaylistColumnClass, profile: ColumnWidthProfile) -> u32 {
        let fallback = profile.min_px.min(64);
        match class {
            PlaylistColumnClass::AlbumArt => profile.preferred_px,
            PlaylistColumnClass::Favorite => profile.preferred_px,
            PlaylistColumnClass::TrackNumber => 24,
            PlaylistColumnClass::DiscNumber => 24,
            PlaylistColumnClass::YearDate => 36,
            PlaylistColumnClass::Title => 72,
            PlaylistColumnClass::ArtistFamily => 64,
            PlaylistColumnClass::Album => 64,
            PlaylistColumnClass::Genre => 52,
            PlaylistColumnClass::Duration => 36,
            PlaylistColumnClass::Custom => 52,
            PlaylistColumnClass::Generic => fallback,
        }
        .min(profile.max_px)
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
            if Self::is_album_art_builtin_column(column) || Self::is_favorite_builtin_column(column)
            {
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
        stored_target_width_px: Option<u32>,
        content_target_width_px: u32,
        profile: ColumnWidthProfile,
        preserve_current_widths: bool,
    ) -> u32 {
        let base_width_px = if let Some(override_width_px) = override_width_px {
            override_width_px
        } else if preserve_current_widths {
            stored_target_width_px.unwrap_or(content_target_width_px)
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
        if Self::is_album_art_builtin_column(column) || Self::is_favorite_builtin_column(column) {
            return target_width_px;
        }
        profile.min_px
    }

    fn compute_playlist_column_widths(
        &self,
        available_width_px: u32,
        preserve_current_widths: bool,
    ) -> (Vec<u32>, HashMap<String, u32>) {
        let visible_columns = self.visible_playlist_columns();
        if visible_columns.is_empty() {
            return (Vec::new(), HashMap::new());
        }

        let mut layout_specs = Vec::with_capacity(visible_columns.len());
        let mut target_widths_by_key = HashMap::with_capacity(visible_columns.len());

        for (index, column) in visible_columns.iter().enumerate() {
            let profile = self.column_width_profile_for_column(column);
            let class = Self::playlist_column_class(column);
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
            let effective_max_px = if override_width.is_some() {
                profile.max_px
            } else {
                Self::auto_growth_max_px_for_column(class, profile, content_target)
            };
            let effective_profile = ColumnWidthProfile {
                min_px: profile.min_px,
                preferred_px: profile.preferred_px.min(effective_max_px),
                max_px: effective_max_px.max(profile.min_px),
            };
            let stored_target = self
                .playlist_column_target_widths_px
                .get(&column_key)
                .copied();
            let target = Self::resolve_playlist_column_target_width_px(
                override_width,
                stored_target,
                content_target,
                effective_profile,
                preserve_current_widths,
            );
            target_widths_by_key.insert(column_key, target);
            let min_width_px =
                Self::layout_min_width_px_for_column(column, effective_profile, target);
            let emergency_floor_px =
                Self::emergency_floor_for_column(class, effective_profile).min(min_width_px);
            layout_specs.push(DeterministicColumnLayoutSpec {
                min_px: min_width_px,
                max_px: effective_profile.max_px.max(min_width_px),
                target_px: target,
                semantic_floor_px: Self::semantic_floor_for_column(class, effective_profile)
                    .max(min_width_px),
                emergency_floor_px,
                shrink_priority: Self::column_shrink_priority(class),
                fixed_width: Self::is_album_art_builtin_column(column)
                    || Self::is_favorite_builtin_column(column),
            });
        }

        let spacing_total = PLAYLIST_COLUMN_SPACING_PX
            .saturating_mul((layout_specs.len().saturating_sub(1)) as u32);
        let preferred_total: u32 = layout_specs.iter().map(|spec| spec.target_px).sum();
        let available_for_columns = if available_width_px == 0 {
            preferred_total
        } else {
            available_width_px.saturating_sub(spacing_total)
        };

        let widths = fit_column_widths_deterministic(&layout_specs, available_for_columns);

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
        let content_width = widths.iter().copied().sum::<u32>().saturating_add(
            PLAYLIST_COLUMN_SPACING_PX.saturating_mul((widths.len().saturating_sub(1)) as u32),
        );
        if preserve_current_widths {
            for (key, value) in target_widths_by_key {
                self.playlist_column_target_widths_px
                    .entry(key)
                    .or_insert(value);
            }
        } else {
            self.playlist_column_target_widths_px = target_widths_by_key;
        }

        trace!(
            "Playlist geometry: visible_band={} widths={:?} columns_content={} row_height={}",
            self.playlist_columns_available_width_px,
            widths,
            content_width,
            row_height_px
        );

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
                cursor_px = cursor_px.saturating_add(PLAYLIST_COLUMN_SPACING_PX as i32);
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

    fn status_text_from_detailed_metadata(meta: &protocol::DetailedMetadata) -> String {
        let title = if meta.title.is_empty() {
            "Unknown".to_string()
        } else {
            meta.title.clone()
        };
        if !meta.artist.is_empty() {
            format!("{} - {}", meta.artist, title)
        } else {
            title
        }
    }

    fn status_selection_summary_text(selected_track_count: usize) -> String {
        match selected_track_count {
            0 => String::new(),
            1 => "1 track selected".to_string(),
            count => format!("{} tracks selected", count),
        }
    }

    fn resolve_metadata_for_track_path_from_sources(
        path: &Path,
        track_paths: &[PathBuf],
        track_metadata: &[TrackMetadata],
        library_entries: &[LibraryEntry],
    ) -> Option<protocol::DetailedMetadata> {
        if let Some(index) = track_paths.iter().position(|candidate| candidate == path) {
            if let Some(metadata) = track_metadata.get(index) {
                return Some(Self::to_detailed_metadata(metadata));
            }
        }

        if let Some(track) = library_entries.iter().find_map(|entry| match entry {
            LibraryEntry::Track(track) if track.path.as_path() == path => Some(track),
            _ => None,
        }) {
            return Some(Self::to_detailed_metadata_from_library_track(track));
        }

        None
    }

    fn resolve_metadata_for_track_path(&self, path: &Path) -> Option<protocol::DetailedMetadata> {
        Self::resolve_metadata_for_track_path_from_sources(
            path,
            &self.track_paths,
            &self.track_metadata,
            &self.library_entries,
        )
        .or_else(|| Some(Self::fallback_detailed_metadata_from_path(path)))
    }

    fn set_playing_track(
        &mut self,
        id: Option<String>,
        path: Option<PathBuf>,
        metadata_hint: Option<protocol::DetailedMetadata>,
    ) -> bool {
        let previous_path = self.playing_track.path.clone();
        let path_changed = previous_path != path;
        self.playing_track.id = id;
        self.playing_track.path = path.clone();
        self.playing_track.metadata = match (path.as_ref(), metadata_hint) {
            (None, _) => None,
            (Some(_), Some(metadata)) => Some(metadata),
            (Some(path), None) if !path_changed => self.playing_track.metadata.clone(),
            (Some(path), None) => self.resolve_metadata_for_track_path(path.as_path()),
        };
        path_changed
    }

    fn refresh_playing_track_metadata(&mut self) -> bool {
        let Some(path) = self.playing_track.path.as_ref() else {
            if self.playing_track.metadata.is_some() {
                self.playing_track.metadata = None;
                return true;
            }
            return false;
        };
        let resolved = self.resolve_metadata_for_track_path(path.as_path());
        if Self::detailed_metadata_equal(&self.playing_track.metadata, &resolved) {
            return false;
        }
        self.playing_track.metadata = resolved;
        true
    }

    fn has_display_target(
        path: &Option<PathBuf>,
        metadata: &Option<protocol::DetailedMetadata>,
    ) -> bool {
        path.is_some() || metadata.is_some()
    }

    fn detailed_metadata_equal(
        left: &Option<protocol::DetailedMetadata>,
        right: &Option<protocol::DetailedMetadata>,
    ) -> bool {
        match (left.as_ref(), right.as_ref()) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.title == right.title
                    && left.artist == right.artist
                    && left.album == right.album
                    && left.date == right.date
                    && left.genre == right.genre
            }
            _ => false,
        }
    }

    fn resolve_display_target(
        selected_indices: &[usize],
        track_paths: &[PathBuf],
        track_metadata: &[TrackMetadata],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
        prefer_playing: bool,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        let (selected_path, selected_metadata) =
            if let Some(&selected_index) = selected_indices.first() {
                (
                    track_paths.get(selected_index).cloned(),
                    track_metadata
                        .get(selected_index)
                        .map(Self::to_detailed_metadata),
                )
            } else {
                (None, None)
            };

        let playing_path = playing_track_path.cloned();
        let (playing_path, playing_metadata) = if let Some(path) = playing_track_path {
            if let Some(index) = track_paths.iter().position(|candidate| candidate == path) {
                if let Some(metadata) = track_metadata.get(index) {
                    (playing_path, Some(Self::to_detailed_metadata(metadata)))
                } else {
                    (playing_path, playing_track_metadata.cloned())
                }
            } else {
                (playing_path, playing_track_metadata.cloned())
            }
        } else {
            (playing_path, playing_track_metadata.cloned())
        };

        if prefer_playing {
            if Self::has_display_target(&playing_path, &playing_metadata) {
                return (playing_path, playing_metadata);
            }
            if Self::has_display_target(&selected_path, &selected_metadata) {
                return (selected_path, selected_metadata);
            }
        } else {
            if Self::has_display_target(&selected_path, &selected_metadata) {
                return (selected_path, selected_metadata);
            }
            if Self::has_display_target(&playing_path, &playing_metadata) {
                return (playing_path, playing_metadata);
            }
        }

        (None, None)
    }

    fn update_display_for_selection(
        &mut self,
        selected_indices: &[usize],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
        prefer_playing: bool,
    ) {
        let (display_path, display_metadata) = Self::resolve_display_target(
            selected_indices,
            &self.track_paths,
            &self.track_metadata,
            playing_track_path,
            playing_track_metadata,
            prefer_playing,
        );
        self.update_cover_art(display_path.as_ref());
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::MetadataDisplayChanged(display_metadata),
        ));
    }

    fn to_detailed_metadata_from_library_track(
        library_track: &protocol::LibraryTrack,
    ) -> protocol::DetailedMetadata {
        protocol::DetailedMetadata {
            title: library_track.title.clone(),
            artist: library_track.artist.clone(),
            album: library_track.album.clone(),
            date: library_track.year.clone(),
            genre: library_track.genre.clone(),
        }
    }

    fn fallback_detailed_metadata_from_path(path: &Path) -> protocol::DetailedMetadata {
        if let Some(parsed) = metadata_tags::read_common_track_metadata(path) {
            let has_any_metadata = !parsed.title.trim().is_empty()
                || !parsed.artist.trim().is_empty()
                || !parsed.album.trim().is_empty()
                || !parsed.date.trim().is_empty()
                || !parsed.year.trim().is_empty()
                || !parsed.genre.trim().is_empty();
            if has_any_metadata {
                let title = if parsed.title.trim().is_empty() {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .or_else(|| path.file_name().and_then(|name| name.to_str()))
                        .unwrap_or_default()
                        .to_string()
                } else {
                    parsed.title
                };
                let date = if parsed.date.trim().is_empty() {
                    parsed.year
                } else {
                    parsed.date
                };
                return protocol::DetailedMetadata {
                    title,
                    artist: parsed.artist,
                    album: parsed.album,
                    date,
                    genre: parsed.genre,
                };
            }
        }

        let title = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .or_else(|| path.file_name().and_then(|name| name.to_str()))
            .unwrap_or_default()
            .to_string();
        protocol::DetailedMetadata {
            title,
            artist: String::new(),
            album: String::new(),
            date: String::new(),
            genre: String::new(),
        }
    }

    fn resolve_library_display_target(
        selected_indices: &[usize],
        library_entries: &[LibraryEntry],
        playlist_track_paths: &[PathBuf],
        playlist_track_metadata: &[TrackMetadata],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
        prefer_playing: bool,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        let selected_track = selected_indices
            .iter()
            .filter_map(|index| library_entries.get(*index))
            .find_map(|entry| match entry {
                LibraryEntry::Track(track) => Some(track),
                _ => None,
            });
        let (selected_path, selected_metadata) = if let Some(track) = selected_track {
            (
                Some(track.path.clone()),
                Some(Self::to_detailed_metadata_from_library_track(track)),
            )
        } else {
            (None, None)
        };

        let playing_path = playing_track_path.cloned();
        let (playing_path, playing_metadata) = if let Some(path) = playing_track_path {
            let playing_track = library_entries.iter().find_map(|entry| match entry {
                LibraryEntry::Track(track) if &track.path == path => Some(track),
                _ => None,
            });
            if let Some(track) = playing_track {
                (
                    playing_path,
                    Some(Self::to_detailed_metadata_from_library_track(track)),
                )
            } else if let Some(index) = playlist_track_paths
                .iter()
                .position(|candidate| candidate == path)
            {
                if let Some(metadata) = playlist_track_metadata.get(index) {
                    (playing_path, Some(Self::to_detailed_metadata(metadata)))
                } else if let Some(metadata) = playing_track_metadata {
                    (playing_path, Some(metadata.clone()))
                } else {
                    (
                        playing_path,
                        Some(Self::fallback_detailed_metadata_from_path(path)),
                    )
                }
            } else if let Some(metadata) = playing_track_metadata {
                (playing_path, Some(metadata.clone()))
            } else {
                (
                    playing_path,
                    Some(Self::fallback_detailed_metadata_from_path(path)),
                )
            }
        } else {
            (playing_path, playing_track_metadata.cloned())
        };

        if prefer_playing {
            if Self::has_display_target(&playing_path, &playing_metadata) {
                return (playing_path, playing_metadata);
            }
            if Self::has_display_target(&selected_path, &selected_metadata) {
                return (selected_path, selected_metadata);
            }
        } else {
            if Self::has_display_target(&selected_path, &selected_metadata) {
                return (selected_path, selected_metadata);
            }
            if Self::has_display_target(&playing_path, &playing_metadata) {
                return (playing_path, playing_metadata);
            }
        }

        (None, None)
    }

    fn resolve_display_target_with_mode(
        selected_indices: &[usize],
        track_paths: &[PathBuf],
        track_metadata: &[TrackMetadata],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
        mode: DisplayTargetResolutionMode,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        match mode {
            DisplayTargetResolutionMode::PreferSelection => Self::resolve_display_target(
                selected_indices,
                track_paths,
                track_metadata,
                playing_track_path,
                playing_track_metadata,
                false,
            ),
            DisplayTargetResolutionMode::PreferPlaying => Self::resolve_display_target(
                selected_indices,
                track_paths,
                track_metadata,
                playing_track_path,
                playing_track_metadata,
                true,
            ),
            DisplayTargetResolutionMode::SelectionOnly => Self::resolve_display_target(
                selected_indices,
                track_paths,
                track_metadata,
                None,
                None,
                false,
            ),
            DisplayTargetResolutionMode::PlayingOnly => Self::resolve_display_target(
                &[],
                track_paths,
                track_metadata,
                playing_track_path,
                playing_track_metadata,
                true,
            ),
        }
    }

    fn resolve_library_display_target_with_mode(
        selected_indices: &[usize],
        library_entries: &[LibraryEntry],
        playlist_track_paths: &[PathBuf],
        playlist_track_metadata: &[TrackMetadata],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
        mode: DisplayTargetResolutionMode,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        match mode {
            DisplayTargetResolutionMode::PreferSelection => Self::resolve_library_display_target(
                selected_indices,
                library_entries,
                playlist_track_paths,
                playlist_track_metadata,
                playing_track_path,
                playing_track_metadata,
                false,
            ),
            DisplayTargetResolutionMode::PreferPlaying => Self::resolve_library_display_target(
                selected_indices,
                library_entries,
                playlist_track_paths,
                playlist_track_metadata,
                playing_track_path,
                playing_track_metadata,
                true,
            ),
            DisplayTargetResolutionMode::SelectionOnly => Self::resolve_library_display_target(
                selected_indices,
                library_entries,
                playlist_track_paths,
                playlist_track_metadata,
                None,
                None,
                false,
            ),
            DisplayTargetResolutionMode::PlayingOnly => Self::resolve_library_display_target(
                &[],
                library_entries,
                playlist_track_paths,
                playlist_track_metadata,
                playing_track_path,
                playing_track_metadata,
                true,
            ),
        }
    }

    fn display_resolution_mode_from_priority_code(
        priority_code: i32,
        last_event_priority: DisplayTargetPriority,
    ) -> DisplayTargetResolutionMode {
        match priority_code {
            VIEWER_DISPLAY_PRIORITY_PREFER_SELECTION => {
                DisplayTargetResolutionMode::PreferSelection
            }
            VIEWER_DISPLAY_PRIORITY_PREFER_NOW_PLAYING => {
                DisplayTargetResolutionMode::PreferPlaying
            }
            VIEWER_DISPLAY_PRIORITY_SELECTION_ONLY => DisplayTargetResolutionMode::SelectionOnly,
            VIEWER_DISPLAY_PRIORITY_NOW_PLAYING_ONLY => DisplayTargetResolutionMode::PlayingOnly,
            _ => {
                if last_event_priority == DisplayTargetPriority::Playing {
                    DisplayTargetResolutionMode::PreferPlaying
                } else {
                    DisplayTargetResolutionMode::PreferSelection
                }
            }
        }
    }

    fn viewer_panel_priority_index(priority_code: i32) -> usize {
        match priority_code {
            VIEWER_DISPLAY_PRIORITY_PREFER_SELECTION => 1,
            VIEWER_DISPLAY_PRIORITY_PREFER_NOW_PLAYING => 2,
            VIEWER_DISPLAY_PRIORITY_SELECTION_ONLY => 3,
            VIEWER_DISPLAY_PRIORITY_NOW_PLAYING_ONLY => 4,
            _ => 0,
        }
    }

    fn viewer_panel_metadata_source_index(source_code: i32) -> usize {
        match source_code {
            VIEWER_METADATA_SOURCE_ALBUM_DESCRIPTION => 1,
            VIEWER_METADATA_SOURCE_ARTIST_BIO => 2,
            VIEWER_METADATA_SOURCE_TRACK => 0,
            _ => 0,
        }
    }

    fn viewer_panel_image_source_index(source_code: i32) -> usize {
        match source_code {
            VIEWER_IMAGE_SOURCE_ARTIST_IMAGE => 1,
            VIEWER_IMAGE_SOURCE_ALBUM_ART => 0,
            _ => 0,
        }
    }

    fn viewer_track_context_for_display_target(
        &self,
        display_path: Option<&PathBuf>,
        display_metadata: Option<&protocol::DetailedMetadata>,
    ) -> ViewerTrackContext {
        if let Some(path) = display_path {
            if let Some(index) = self
                .track_paths
                .iter()
                .position(|candidate| candidate == path)
            {
                if let Some(metadata) = self.track_metadata.get(index) {
                    let artist = metadata.artist.trim().to_string();
                    let album = metadata.album.trim().to_string();
                    let album_artist = if metadata.album_artist.trim().is_empty() {
                        artist.clone()
                    } else {
                        metadata.album_artist.trim().to_string()
                    };
                    return ViewerTrackContext {
                        artist,
                        album,
                        album_artist,
                    };
                }
            }

            if let Some(library_track) = self.library_entries.iter().find_map(|entry| match entry {
                LibraryEntry::Track(track) if &track.path == path => Some(track),
                _ => None,
            }) {
                let artist = library_track.artist.trim().to_string();
                let album = library_track.album.trim().to_string();
                let album_artist = if library_track.album_artist.trim().is_empty() {
                    artist.clone()
                } else {
                    library_track.album_artist.trim().to_string()
                };
                return ViewerTrackContext {
                    artist,
                    album,
                    album_artist,
                };
            }
        }

        if let Some(metadata) = display_metadata {
            let artist = metadata.artist.trim().to_string();
            let album = metadata.album.trim().to_string();
            let album_artist = artist.clone();
            return ViewerTrackContext {
                artist,
                album,
                album_artist,
            };
        }

        ViewerTrackContext::default()
    }

    fn viewer_artist_entity_for_track_context(
        context: &ViewerTrackContext,
    ) -> Option<protocol::LibraryEnrichmentEntity> {
        (!context.artist.is_empty()).then(|| protocol::LibraryEnrichmentEntity::Artist {
            artist: context.artist.clone(),
        })
    }

    fn viewer_album_entity_for_track_context(
        context: &ViewerTrackContext,
    ) -> Option<protocol::LibraryEnrichmentEntity> {
        if context.album.is_empty() || context.album_artist.is_empty() {
            return None;
        }
        Some(protocol::LibraryEnrichmentEntity::Album {
            album: context.album.clone(),
            album_artist: context.album_artist.clone(),
        })
    }

    fn viewer_metadata_from_artist_bio_payload(
        context: &ViewerTrackContext,
        payload: &protocol::LibraryEnrichmentPayload,
    ) -> Option<protocol::DetailedMetadata> {
        if payload.status != protocol::LibraryEnrichmentStatus::Ready
            || payload.blurb.trim().is_empty()
        {
            return None;
        }
        let title = if context.artist.is_empty() {
            "Artist Bio".to_string()
        } else {
            context.artist.clone()
        };
        Some(protocol::DetailedMetadata {
            title,
            artist: payload.blurb.clone(),
            album: String::new(),
            date: String::new(),
            genre: payload.source_name.clone(),
        })
    }

    fn viewer_metadata_from_album_description_payload(
        context: &ViewerTrackContext,
        payload: &protocol::LibraryEnrichmentPayload,
    ) -> Option<protocol::DetailedMetadata> {
        if payload.status != protocol::LibraryEnrichmentStatus::Ready
            || payload.blurb.trim().is_empty()
        {
            return None;
        }
        let title = if context.album.is_empty() {
            "Album Description".to_string()
        } else {
            context.album.clone()
        };
        Some(protocol::DetailedMetadata {
            title,
            artist: payload.blurb.clone(),
            album: String::new(),
            date: String::new(),
            genre: payload.source_name.clone(),
        })
    }

    fn resolve_active_collection_display_target_with_mode(
        &self,
        mode: DisplayTargetResolutionMode,
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            Self::resolve_library_display_target_with_mode(
                &self.library_selected_indices,
                &self.library_entries,
                &self.track_paths,
                &self.track_metadata,
                playing_track_path,
                playing_track_metadata,
                mode,
            )
        } else {
            Self::resolve_display_target_with_mode(
                &self.selected_indices,
                &self.track_paths,
                &self.track_metadata,
                playing_track_path,
                playing_track_metadata,
                mode,
            )
        }
    }

    fn resolve_cover_art_path_for_viewer_path(&mut self, track_path: &PathBuf) -> Option<PathBuf> {
        let source_index = self
            .track_paths
            .iter()
            .position(|candidate| candidate == track_path);
        let mut cover_art_path =
            source_index.and_then(|index| self.resolve_track_cover_art_path(index));
        if cover_art_path.is_none() {
            cover_art_path = self.resolve_library_cover_art_path(track_path);
        }
        if cover_art_path.is_none() {
            cover_art_path = Self::find_local_cover_art(track_path.as_path());
        }
        if let Some(source_index) = source_index {
            if let Some(slot) = self.track_cover_art_paths.get_mut(source_index) {
                *slot = cover_art_path.clone();
            }
            if cover_art_path.is_some() {
                self.track_cover_art_missing_tracks.remove(track_path);
            } else {
                self.track_cover_art_missing_tracks
                    .insert(track_path.clone());
            }
        }
        self.library_cover_art_paths
            .insert(track_path.clone(), cover_art_path.clone());
        cover_art_path
    }

    fn refresh_viewer_panel_models(
        &self,
        track_metadata_by_priority: Vec<Option<protocol::DetailedMetadata>>,
        album_description_by_priority: Vec<Option<protocol::DetailedMetadata>>,
        artist_bio_by_priority: Vec<Option<protocol::DetailedMetadata>>,
        album_art_paths_by_priority: Vec<Option<PathBuf>>,
        artist_image_paths_by_priority: Vec<Option<PathBuf>>,
    ) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let metadata_model = ui.get_layout_metadata_viewer_panels();
            if let Some(vec_model) = metadata_model
                .as_any()
                .downcast_ref::<VecModel<LayoutMetadataViewerPanelModel>>()
            {
                let row_count = vec_model.row_count();
                for row_index in 0..row_count {
                    let Some(mut row_data) = vec_model.row_data(row_index) else {
                        continue;
                    };
                    let priority_index =
                        UiManager::viewer_panel_priority_index(row_data.display_priority);
                    row_data.display_priority = priority_index as i32;
                    let metadata_source_index =
                        UiManager::viewer_panel_metadata_source_index(row_data.metadata_source);
                    row_data.metadata_source = metadata_source_index as i32;
                    let metadata = match metadata_source_index {
                        1 => album_description_by_priority
                            .get(priority_index)
                            .and_then(|value| value.as_ref()),
                        2 => artist_bio_by_priority
                            .get(priority_index)
                            .and_then(|value| value.as_ref()),
                        _ => track_metadata_by_priority
                            .get(priority_index)
                            .and_then(|value| value.as_ref()),
                    };
                    if let Some(metadata) = metadata {
                        row_data.display_title = metadata.title.as_str().into();
                        row_data.display_artist = metadata.artist.as_str().into();
                        row_data.display_album = metadata.album.as_str().into();
                        row_data.display_date = metadata.date.as_str().into();
                        row_data.display_genre = metadata.genre.as_str().into();
                    } else {
                        row_data.display_title = "".into();
                        row_data.display_artist = "".into();
                        row_data.display_album = "".into();
                        row_data.display_date = "".into();
                        row_data.display_genre = "".into();
                    }
                    vec_model.set_row_data(row_index, row_data);
                }
            }

            let album_art_model = ui.get_layout_album_art_viewer_panels();
            if let Some(vec_model) = album_art_model
                .as_any()
                .downcast_ref::<VecModel<LayoutAlbumArtViewerPanelModel>>()
            {
                let row_count = vec_model.row_count();
                for row_index in 0..row_count {
                    let Some(mut row_data) = vec_model.row_data(row_index) else {
                        continue;
                    };
                    let priority_index =
                        UiManager::viewer_panel_priority_index(row_data.display_priority);
                    row_data.display_priority = priority_index as i32;
                    let image_source_index =
                        UiManager::viewer_panel_image_source_index(row_data.image_source);
                    row_data.image_source = image_source_index as i32;
                    let art_path = match image_source_index {
                        1 => artist_image_paths_by_priority.get(priority_index),
                        _ => album_art_paths_by_priority.get(priority_index),
                    }
                    .cloned()
                    .unwrap_or(None);
                    let image_kind = if image_source_index == 1 {
                        protocol::UiImageKind::ArtistImage
                    } else {
                        protocol::UiImageKind::CoverArt
                    };
                    let art_source = art_path
                        .as_ref()
                        .and_then(|path| {
                            UiManager::try_load_detail_cover_art_image_with_kind(
                                path,
                                image_kind,
                                DETAIL_VIEWER_RENDER_MAX_EDGE_PX,
                                DETAIL_VIEWER_CONVERT_THRESHOLD_PX,
                            )
                        })
                        .unwrap_or_default();
                    let has_art = art_path.is_some();
                    row_data.art_source = art_source;
                    row_data.has_art = has_art;
                    vec_model.set_row_data(row_index, row_data);
                }
            }
        });
    }

    fn update_viewer_panel_content_for_all_priorities(
        &mut self,
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) {
        let priority_codes = [
            VIEWER_DISPLAY_PRIORITY_DEFAULT,
            VIEWER_DISPLAY_PRIORITY_PREFER_SELECTION,
            VIEWER_DISPLAY_PRIORITY_PREFER_NOW_PLAYING,
            VIEWER_DISPLAY_PRIORITY_SELECTION_ONLY,
            VIEWER_DISPLAY_PRIORITY_NOW_PLAYING_ONLY,
        ];
        let mut track_metadata_by_priority = Vec::with_capacity(priority_codes.len());
        let mut display_paths_by_priority = Vec::with_capacity(priority_codes.len());
        let mut display_contexts_by_priority = Vec::with_capacity(priority_codes.len());
        let mut album_entities_by_priority = Vec::with_capacity(priority_codes.len());
        let mut artist_entities_by_priority = Vec::with_capacity(priority_codes.len());
        for priority_code in priority_codes {
            let mode = Self::display_resolution_mode_from_priority_code(
                priority_code,
                self.display_target_priority,
            );
            let (display_path, display_metadata) = self
                .resolve_active_collection_display_target_with_mode(
                    mode,
                    playing_track_path,
                    playing_track_metadata,
                );
            let display_context = self.viewer_track_context_for_display_target(
                display_path.as_ref(),
                display_metadata.as_ref(),
            );
            let album_entity = Self::viewer_album_entity_for_track_context(&display_context);
            let artist_entity = Self::viewer_artist_entity_for_track_context(&display_context);
            display_paths_by_priority.push(display_path);
            track_metadata_by_priority.push(display_metadata);
            display_contexts_by_priority.push(display_context);
            album_entities_by_priority.push(album_entity);
            artist_entities_by_priority.push(artist_entity);
        }

        let mut cover_art_cache: HashMap<Option<PathBuf>, Option<PathBuf>> = HashMap::new();
        let mut album_art_paths_by_priority = Vec::with_capacity(display_paths_by_priority.len());
        for display_path in &display_paths_by_priority {
            let cache_key = display_path.clone();
            let entry = cover_art_cache.entry(cache_key.clone()).or_insert_with(|| {
                if let Some(path) = cache_key.as_ref() {
                    self.resolve_cover_art_path_for_viewer_path(path)
                } else {
                    None
                }
            });
            album_art_paths_by_priority.push(entry.clone());
        }

        let mut album_description_by_priority = Vec::with_capacity(priority_codes.len());
        let mut artist_bio_by_priority = Vec::with_capacity(priority_codes.len());
        let mut artist_image_paths_by_priority = Vec::with_capacity(priority_codes.len());
        for index in 0..priority_codes.len() {
            let context = display_contexts_by_priority
                .get(index)
                .cloned()
                .unwrap_or_default();
            let album_entity = album_entities_by_priority.get(index).cloned().flatten();
            let artist_entity = artist_entities_by_priority.get(index).cloned().flatten();

            if let Some(entity) = album_entity.as_ref() {
                self.request_enrichment(
                    entity.clone(),
                    protocol::LibraryEnrichmentPriority::Interactive,
                );
            }
            if let Some(entity) = artist_entity.as_ref() {
                self.request_enrichment(
                    entity.clone(),
                    protocol::LibraryEnrichmentPriority::Interactive,
                );
            }

            let album_description = album_entity
                .as_ref()
                .and_then(|entity| self.library_enrichment.get(entity))
                .and_then(|payload| {
                    Self::viewer_metadata_from_album_description_payload(&context, payload)
                });
            album_description_by_priority.push(album_description);

            let artist_bio = artist_entity
                .as_ref()
                .and_then(|entity| self.library_enrichment.get(entity))
                .and_then(|payload| {
                    Self::viewer_metadata_from_artist_bio_payload(&context, payload)
                });
            artist_bio_by_priority.push(artist_bio);

            let artist_image_path = artist_entity.as_ref().and_then(|entity| match entity {
                protocol::LibraryEnrichmentEntity::Artist { artist } => {
                    self.artist_enrichment_image_path(artist)
                }
                _ => None,
            });
            artist_image_paths_by_priority.push(artist_image_path);
        }

        self.refresh_viewer_panel_models(
            track_metadata_by_priority,
            album_description_by_priority,
            artist_bio_by_priority,
            album_art_paths_by_priority,
            artist_image_paths_by_priority,
        );
    }

    /// Resolve `library_playing_index` to the source index in `library_entries`
    /// for the currently playing track.
    ///
    /// Uses the stable track **id** as the primary lookup key (consistent with
    /// the playlist-side `resolve_active_playing_source_index`), falling back
    /// to a path-based match when the id is unavailable.  This ensures the
    /// correct library row is highlighted even when a search filter reorders
    /// the view or when duplicate paths exist across local/remote sources.
    fn update_library_playing_index(&mut self) {
        // Primary: match by stable track id.
        if let Some(playing_id) = &self.playing_track.id {
            if let Some(index) = self.library_entries.iter().position(|entry| {
                if let LibraryEntry::Track(track) = entry {
                    &track.id == playing_id
                } else {
                    false
                }
            }) {
                self.library_playing_index = Some(index);
                return;
            }
        }
        // Fallback: match by file path when id is unavailable.
        if let Some(playing_path) = &self.playing_track.path {
            self.library_playing_index = self.library_entries.iter().position(|entry| {
                if let LibraryEntry::Track(track) = entry {
                    &track.path == playing_path
                } else {
                    false
                }
            });
        } else {
            self.library_playing_index = None;
        }
    }

    fn update_display_for_library_selection(
        &mut self,
        selected_indices: &[usize],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
        prefer_playing: bool,
    ) {
        let (display_path, display_metadata) = Self::resolve_library_display_target(
            selected_indices,
            &self.library_entries,
            &self.track_paths,
            &self.track_metadata,
            playing_track_path,
            playing_track_metadata,
            prefer_playing,
        );
        self.update_cover_art(display_path.as_ref());
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::MetadataDisplayChanged(display_metadata),
        ));
    }

    fn update_display_for_active_collection(&mut self) {
        let playing_track_path = self.playing_track.path.clone();
        let playing_track_metadata = self.playing_track.metadata.clone();
        let prefer_playing = self.display_target_priority == DisplayTargetPriority::Playing;
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            let selected_indices = self.library_selected_indices.clone();
            self.update_display_for_library_selection(
                &selected_indices,
                playing_track_path.as_ref(),
                playing_track_metadata.as_ref(),
                prefer_playing,
            );
        } else {
            let selected_indices = self.selected_indices.clone();
            self.update_display_for_selection(
                &selected_indices,
                playing_track_path.as_ref(),
                playing_track_metadata.as_ref(),
                prefer_playing,
            );
        }
        self.update_viewer_panel_content_for_all_priorities(
            playing_track_path.as_ref(),
            playing_track_metadata.as_ref(),
        );
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
        let track = match self.library_entries.get(source_index)? {
            LibraryEntry::Track(track) => track,
            _ => return None,
        };
        let title = track.title.trim().to_string();
        let display_title = if title.is_empty() {
            track
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_default()
        } else {
            title
        };
        Some((track.path.clone(), display_title))
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
        let Some((path, _target_title)) = self.active_properties_target() else {
            return;
        };

        self.properties_target_path = Some(path.clone());
        self.properties_target_title = _target_title;
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

    fn open_file_location(&self) {
        let Some((path, _)) = self.active_properties_target() else {
            return;
        };
        if Self::is_running_in_flatpak() {
            let reveal_target = path.parent().unwrap_or(path.as_path());
            match Command::new("xdg-open").arg(reveal_target).spawn() {
                Ok(_) => return,
                Err(err) => {
                    warn!(
                        "UiManager: flatpak xdg-open fallback failed for {}: {}",
                        reveal_target.display(),
                        err
                    );
                }
            }
        }
        showfile::show_path_in_file_manager(&path);
    }

    fn is_running_in_flatpak() -> bool {
        std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists()
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

        if self.playing_track.path.as_deref() == Some(path) {
            let detailed = protocol::DetailedMetadata {
                title: summary.title.clone(),
                artist: summary.artist.clone(),
                album: summary.album.clone(),
                date,
                genre: summary.genre.clone(),
            };
            if self.playing_track.metadata.as_ref().map(|meta| {
                meta.title == detailed.title
                    && meta.artist == detailed.artist
                    && meta.album == detailed.album
                    && meta.date == detailed.date
                    && meta.genre == detailed.genre
            }) != Some(true)
            {
                self.playing_track.metadata = Some(detailed);
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
            let LibraryEntry::Track(track) = entry else {
                continue;
            };
            if track.path.as_path() != path {
                continue;
            }

            if track.title != summary.title {
                track.title = summary.title.clone();
                changed = true;
            }
            if track.artist != summary.artist {
                track.artist = summary.artist.clone();
                changed = true;
            }
            if track.album != summary.album {
                track.album = summary.album.clone();
                changed = true;
            }
            if track.album_artist != summary.album_artist {
                track.album_artist = summary.album_artist.clone();
                changed = true;
            }
            if track.genre != summary.genre {
                track.genre = summary.genre.clone();
                changed = true;
            }
            if track.year != summary.year {
                track.year = summary.year.clone();
                changed = true;
            }
            if track.track_number != summary.track_number {
                track.track_number = summary.track_number.clone();
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

        self.refresh_playing_track_metadata();
        self.update_display_for_active_collection();

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

    fn reset_playlist_cover_art_state(
        track_cover_art_missing_tracks: &mut HashSet<PathBuf>,
        folder_cover_art_paths: &mut HashMap<PathBuf, Option<PathBuf>>,
    ) {
        track_cover_art_missing_tracks.clear();
        // Keep successful folder resolutions, but drop negative cache entries so
        // transient startup failures can be retried on the next resolve pass.
        folder_cover_art_paths.retain(|_, path| path.is_some());
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

    fn map_library_source_to_view_index(&self, source_index: usize) -> Option<usize> {
        if self.library_view_indices.is_empty() {
            return (source_index < self.library_entries.len()).then_some(source_index);
        }
        self.library_view_indices
            .iter()
            .position(|&candidate| candidate == source_index)
    }

    fn center_playlist_view_on_playing_track(&mut self) {
        let Some(source_index) = self.active_playing_index else {
            return;
        };
        let Some(view_index) = self.map_source_to_view_index(source_index) else {
            return;
        };
        self.playlist_scroll_center_token = self.playlist_scroll_center_token.wrapping_add(1);
        let token = self.playlist_scroll_center_token;
        let row = view_index as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_scroll_target_row(row);
            ui.set_playlist_scroll_center_token(token);
        });
    }

    fn center_library_view_on_playing_track(&mut self) {
        let Some(source_index) = self.library_playing_index else {
            return;
        };
        let Some(view_index) = self.map_library_source_to_view_index(source_index) else {
            return;
        };
        self.library_scroll_center_token = self.library_scroll_center_token.wrapping_add(1);
        let immediate_token = self.library_scroll_center_token;
        self.library_scroll_center_token = self.library_scroll_center_token.wrapping_add(1);
        let deferred_token = self.library_scroll_center_token;
        let row = view_index as i32;
        let ui_handle = self.ui.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_scroll_target_row(row);
            ui.set_library_scroll_center_token(immediate_token);
            let deferred_ui_handle = ui_handle.clone();
            slint::Timer::single_shot(Duration::from_millis(24), move || {
                if let Some(ui) = deferred_ui_handle.upgrade() {
                    ui.set_library_scroll_target_row(row);
                    ui.set_library_scroll_center_token(deferred_token);
                }
            });
        });
    }

    fn auto_scroll_active_collection_to_playing_track(&mut self) {
        if !self.auto_scroll_to_playing_track {
            return;
        }
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.center_library_view_on_playing_track();
        } else {
            self.center_playlist_view_on_playing_track();
        }
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
            LibraryEntry::Track(track) => {
                Self::library_text_matches_search(&track.title, normalized_query)
                    || Self::library_text_matches_search(&track.artist, normalized_query)
                    || Self::library_text_matches_search(&track.album, normalized_query)
                    || Self::library_text_matches_search(&track.album_artist, normalized_query)
                    || Self::library_text_matches_search(&track.genre, normalized_query)
                    || Self::library_text_matches_search(&track.year, normalized_query)
                    || Self::library_text_matches_search(&track.track_number, normalized_query)
                    || Self::library_text_matches_search(
                        &track.path.to_string_lossy(),
                        normalized_query,
                    )
            }
            LibraryEntry::Artist(artist) => {
                Self::library_text_matches_search(&artist.artist, normalized_query)
                    || artist.album_count.to_string().contains(normalized_query)
                    || artist.track_count.to_string().contains(normalized_query)
            }
            LibraryEntry::Album(album) => {
                Self::library_text_matches_search(&album.album, normalized_query)
                    || Self::library_text_matches_search(&album.album_artist, normalized_query)
                    || album.track_count.to_string().contains(normalized_query)
            }
            LibraryEntry::Genre(genre) => {
                Self::library_text_matches_search(&genre.genre, normalized_query)
                    || genre.track_count.to_string().contains(normalized_query)
            }
            LibraryEntry::Decade(decade) => {
                Self::library_text_matches_search(&decade.decade, normalized_query)
                    || decade.track_count.to_string().contains(normalized_query)
            }
            LibraryEntry::FavoriteCategory(category) => {
                Self::library_text_matches_search(&category.title, normalized_query)
                    || category.count.to_string().contains(normalized_query)
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

    fn prune_unavailable_track_ids(&mut self) {
        let active_track_ids: HashSet<String> = self.track_ids.iter().cloned().collect();
        self.unavailable_track_ids
            .retain(|track_id| active_track_ids.contains(track_id));
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

    fn resolve_shift_anchor_source_index(
        selected_indices: &[usize],
        current_anchor_source_index: Option<usize>,
    ) -> Option<usize> {
        if selected_indices.is_empty() {
            return None;
        }
        if let Some(anchor_source_index) = current_anchor_source_index {
            if selected_indices.contains(&anchor_source_index) {
                return Some(anchor_source_index);
            }
        }
        selected_indices.last().copied()
    }

    fn selection_lead_source_index(&self, anchor_source_index: Option<usize>) -> Option<usize> {
        if self.selected_indices.is_empty() {
            return None;
        }
        let Some(anchor) = anchor_source_index else {
            return self.selected_indices.last().copied();
        };
        let anchor_view = self.map_source_to_view_index(anchor);
        let first_view = self
            .map_source_to_view_index(*self.selected_indices.first()?)
            .unwrap_or(0);
        let last_view = self
            .map_source_to_view_index(*self.selected_indices.last()?)
            .unwrap_or(0);
        match anchor_view {
            Some(av) if av == first_view => self.selected_indices.last().copied(),
            Some(av) if av == last_view => self.selected_indices.first().copied(),
            _ => self.selected_indices.last().copied(),
        }
    }

    fn library_selection_lead_source_index(&self, anchor: Option<usize>) -> Option<usize> {
        if self.library_selected_indices.is_empty() {
            return None;
        }
        let Some(anchor) = anchor else {
            return self.library_selected_indices.last().copied();
        };
        let anchor_view = self.map_library_source_to_view_index(anchor);
        let first_view = self
            .map_library_source_to_view_index(*self.library_selected_indices.first()?)
            .unwrap_or(0);
        let last_view = self
            .map_library_source_to_view_index(*self.library_selected_indices.last()?)
            .unwrap_or(0);
        match anchor_view {
            Some(av) if av == first_view => self.library_selected_indices.last().copied(),
            Some(av) if av == last_view => self.library_selected_indices.first().copied(),
            _ => self.library_selected_indices.last().copied(),
        }
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
        let search_query = self.library_search_query.clone();
        let search_result_text = self.library_search_result_text();

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_search_visible(search_visible);
            ui.set_library_search_query(search_query.into());
            ui.set_library_search_result_text(search_result_text.into());
        });
    }

    fn sync_library_scan_status_to_ui(&self) {
        let scan_in_progress = self.library_scan_in_progress;
        let status_text = self.library_status_text.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_scan_in_progress(scan_in_progress);
            ui.set_library_status_text(status_text.into());
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
        self.prune_unavailable_track_ids();
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
            let mut values = Self::build_playlist_row_values(metadata, &self.playlist_columns);
            let track_unavailable = self
                .track_ids
                .get(source_index)
                .map(|track_id| self.unavailable_track_ids.contains(track_id))
                .unwrap_or(false);
            if track_unavailable {
                Self::apply_unavailable_title_override(&mut values, &self.playlist_columns);
            }
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
        let selected_track_count = selected_set.len();
        let selection_summary_text = Self::status_selection_summary_text(selected_track_count);
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
        let (cover_decode_start, cover_decode_end) = self.playlist_cover_decode_window(rows.len());
        type TrackRowPayload = (
            Vec<String>,
            Option<PathBuf>,
            String,
            bool,
            bool,
            String,
            bool,
        );
        let row_data: Vec<TrackRowPayload> = rows
            .into_iter()
            .enumerate()
            .map(|(row_index, row)| {
                let track_unavailable = self
                    .track_ids
                    .get(row.source_index)
                    .map(|track_id| self.unavailable_track_ids.contains(track_id))
                    .unwrap_or(false);
                let status = if track_unavailable {
                    ""
                } else if Some(row.source_index) == active_playing_index {
                    if playback_active {
                        "▶️"
                    } else {
                        "⏸️"
                    }
                } else {
                    ""
                };
                let resolve_album_art = album_art_column_visible
                    && row_index >= cover_decode_start
                    && row_index < cover_decode_end;
                let album_art_path = if resolve_album_art {
                    self.row_cover_art_path(row.source_index)
                        .and_then(|source_path| {
                            self.list_thumbnail_path_if_ready(
                                source_path.as_path(),
                                protocol::UiImageKind::CoverArt,
                            )
                        })
                } else {
                    None
                };
                let source_badge = self
                    .track_paths
                    .get(row.source_index)
                    .map(|path| Self::source_badge_for_track_path(path.as_path()))
                    .unwrap_or_default();
                let favorited = self
                    .track_paths
                    .get(row.source_index)
                    .map(|path| Self::favorite_key_for_track_path(path.as_path()))
                    .map(|key| self.favorites_by_key.contains_key(&key))
                    .unwrap_or(false);
                (
                    row.values,
                    album_art_path,
                    source_badge,
                    favorited,
                    selected_set.contains(&row.source_index),
                    status.to_string(),
                    track_unavailable,
                )
            })
            .collect();

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let mut rows = Vec::with_capacity(row_data.len());
            for (values, album_art_path, source_badge, favorited, selected, status, unavailable) in
                row_data
            {
                let values_shared: Vec<slint::SharedString> =
                    values.into_iter().map(Into::into).collect();
                rows.push(TrackRowData {
                    status: status.into(),
                    values: ModelRc::from(values_shared.as_slice()),
                    album_art: UiManager::load_track_row_cover_art_image(album_art_path.as_ref()),
                    source_badge: source_badge.into(),
                    favorited,
                    selected,
                    unavailable,
                });
            }
            UiManager::update_or_replace_track_model(&ui, rows);

            ui.set_selected_track_index(selected_view_index);
            ui.set_playing_track_index(playing_view_index);
            ui.set_status_selection_summary(selection_summary_text.into());
        });

        self.sync_filter_state_to_ui();
        self.sync_now_playing_favorite_state_to_ui();
        self.sync_properties_action_state();
    }

    fn sync_playlist_playback_state_to_ui(&self) {
        let view_indices = self.view_indices.clone();
        let track_count = self.track_paths.len();
        let track_ids = self.track_ids.clone();
        let unavailable_track_ids = self.unavailable_track_ids.clone();
        let active_playing_index = self.active_playing_index;
        let playback_active = self.playback_active;
        let playing_view_index = active_playing_index
            .and_then(|source_index| self.map_source_to_view_index(source_index))
            .map(|index| index as i32)
            .unwrap_or(-1);
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let current_model = ui.get_track_model();
            let Some(vec_model) = current_model
                .as_any()
                .downcast_ref::<VecModel<TrackRowData>>()
            else {
                return;
            };
            let source_indices: Vec<usize> = if view_indices.is_empty() {
                (0..track_count).collect()
            } else {
                view_indices.clone()
            };
            if vec_model.row_count() != source_indices.len() {
                return;
            }
            for (row_index, source_index) in source_indices.into_iter().enumerate() {
                let Some(mut row_data) = vec_model.row_data(row_index) else {
                    continue;
                };
                let track_unavailable = track_ids
                    .get(source_index)
                    .map(|track_id| unavailable_track_ids.contains(track_id))
                    .unwrap_or(false);
                let status = if track_unavailable {
                    ""
                } else if Some(source_index) == active_playing_index {
                    if playback_active {
                        "▶️"
                    } else {
                        "⏸️"
                    }
                } else {
                    ""
                };
                if row_data.status.as_str() == status {
                    continue;
                }
                row_data.status = status.into();
                vec_model.set_row_data(row_index, row_data);
            }
            ui.set_playing_track_index(playing_view_index);
        });
    }

    fn sync_now_playing_favorite_state_to_ui(&self) {
        let current_favorite_entity = self.current_track_favorite_entity();
        let has_current_track_context = current_favorite_entity.is_some();
        let now_playing_favorited = current_favorite_entity
            .as_ref()
            .map(|entity| self.favorites_by_key.contains_key(&entity.entity_key))
            .unwrap_or(false);
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_has_current_track_context(has_current_track_context);
            ui.set_now_playing_favorited(now_playing_favorited);
        });
    }

    fn prefetch_playlist_cover_art_window(&mut self, first_row: usize, row_count: usize) {
        if !self.is_album_art_column_visible() {
            return;
        }
        let total_rows = if self.view_indices.is_empty() {
            self.track_paths.len()
        } else {
            self.view_indices.len()
        };
        if total_rows == 0 {
            return;
        }
        self.playlist_prefetch_first_row = first_row.min(total_rows.saturating_sub(1));
        if row_count > 0 {
            self.playlist_prefetch_row_count = row_count;
        }
        let (start, end) = self.playlist_cover_decode_window(total_rows);
        for view_row in start..end {
            let Some(source_index) = self.map_view_to_source_index(view_row) else {
                continue;
            };
            let Some(cover_path) = self.row_cover_art_path(source_index) else {
                continue;
            };
            let _ = self.list_thumbnail_path_if_ready(
                cover_path.as_path(),
                protocol::UiImageKind::CoverArt,
            );
        }
    }

    fn refresh_visible_playlist_cover_art_rows(&mut self) -> bool {
        if !self.is_album_art_column_visible() {
            return false;
        }
        let total_rows = if self.view_indices.is_empty() {
            self.track_paths.len()
        } else {
            self.view_indices.len()
        };
        let (start, end) = self.playlist_cover_decode_window(total_rows);
        if start >= end {
            return false;
        }
        let mut updates: Vec<(usize, PathBuf)> = Vec::new();
        for view_row in start..end {
            let Some(source_index) = self.map_view_to_source_index(view_row) else {
                continue;
            };
            let Some(cover_path) = self.row_cover_art_path(source_index) else {
                continue;
            };
            let Some(thumbnail_path) = self.list_thumbnail_path_if_ready(
                cover_path.as_path(),
                protocol::UiImageKind::CoverArt,
            ) else {
                continue;
            };
            updates.push((view_row, thumbnail_path));
        }
        if updates.is_empty() {
            return false;
        }
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let current_model = ui.get_track_model();
            let Some(vec_model) = current_model
                .as_any()
                .downcast_ref::<VecModel<TrackRowData>>()
            else {
                return;
            };
            for (view_row, thumbnail_path) in updates {
                if view_row >= vec_model.row_count() {
                    continue;
                }
                let Some(mut row_data) = vec_model.row_data(view_row) else {
                    continue;
                };
                row_data.album_art = UiManager::try_load_cover_art_image_with_kind(
                    thumbnail_path.as_path(),
                    protocol::UiImageKind::CoverArt,
                )
                .unwrap_or_default();
                vec_model.set_row_data(view_row, row_data);
            }
        });
        true
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
        self.clear_search_bars_for_track_list_view_switch();
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

    fn clear_search_bars_for_track_list_view_switch(&mut self) {
        self.close_playlist_search();
        self.close_library_search();
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
            self.playing_track = PlayingTrackState::default();
        }

        self.reset_filter_state();

        self.refresh_playlist_column_content_targets();
        self.apply_playlist_column_layout();
        self.refresh_playing_track_metadata();
        self.update_display_for_active_collection();
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
                LibraryEntry::Track(track) => {
                    (format!("track:{}", track.path.to_string_lossy()), 1usize)
                }
                LibraryEntry::Artist(artist) => (
                    format!("artist:{}", artist.artist),
                    artist.track_count as usize,
                ),
                LibraryEntry::Album(album) => (
                    format!("album:{}\u{001f}{}", album.album, album.album_artist),
                    album.track_count as usize,
                ),
                LibraryEntry::Genre(genre) => {
                    (format!("genre:{}", genre.genre), genre.track_count as usize)
                }
                LibraryEntry::Decade(decade) => (
                    format!("decade:{}", decade.decade),
                    decade.track_count as usize,
                ),
                LibraryEntry::FavoriteCategory(category) => {
                    (format!("favorite_category:{}", category.title), 0usize)
                }
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

    fn request_library_remove_selection_confirmation(&mut self) {
        let selections = self.build_library_selection_specs();
        if selections.is_empty() {
            return;
        }
        self.pending_library_remove_selections = selections;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_show_library_remove_confirm(true);
        });
    }

    fn confirm_library_remove_selection(&mut self) {
        let selections = std::mem::take(&mut self.pending_library_remove_selections);
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_show_library_remove_confirm(false);
        });
        if selections.is_empty() {
            return;
        }
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::RemoveSelectionFromLibrary { selections },
        ));
    }

    fn cancel_library_remove_selection(&mut self) {
        self.pending_library_remove_selections.clear();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_show_library_remove_confirm(false);
        });
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
        // Route library-copy paste through playlist paste handling so insertion
        // stays anchored after current selection (or appends when no selection).
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::PasteSelectionToActivePlaylist {
                selections: self.copied_library_selections.clone(),
            },
        ));
    }

    fn cut_selected_library_items(&mut self) {
        self.copy_selected_library_items();
        self.request_library_remove_selection_confirmation();
    }

    fn cut_selected_tracks(&mut self) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.cut_selected_library_items();
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
            .unwrap_or(LibraryViewState::TracksRoot)
    }

    fn scroll_view_key_for_library_view(view: &LibraryViewState) -> Option<LibraryScrollViewKey> {
        match view {
            LibraryViewState::TracksRoot => Some(LibraryScrollViewKey::TracksRoot),
            LibraryViewState::ArtistsRoot => Some(LibraryScrollViewKey::ArtistsRoot),
            LibraryViewState::AlbumsRoot => Some(LibraryScrollViewKey::AlbumsRoot),
            LibraryViewState::GenresRoot => Some(LibraryScrollViewKey::GenresRoot),
            LibraryViewState::DecadesRoot => Some(LibraryScrollViewKey::DecadesRoot),
            LibraryViewState::FavoritesRoot => Some(LibraryScrollViewKey::FavoritesRoot),
            LibraryViewState::FavoriteTracks => Some(LibraryScrollViewKey::FavoriteTracks),
            LibraryViewState::FavoriteArtists => Some(LibraryScrollViewKey::FavoriteArtists),
            LibraryViewState::FavoriteAlbums => Some(LibraryScrollViewKey::FavoriteAlbums),
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
            Some(LibraryViewState::TracksRoot) => 0,
            Some(LibraryViewState::ArtistsRoot) => 1,
            Some(LibraryViewState::AlbumsRoot) => 2,
            Some(LibraryViewState::GenresRoot) => 3,
            Some(LibraryViewState::DecadesRoot) => 4,
            Some(LibraryViewState::FavoritesRoot) => 5,
            Some(LibraryViewState::FavoriteTracks) => 5,
            Some(LibraryViewState::FavoriteArtists) => 5,
            Some(LibraryViewState::FavoriteAlbums) => 5,
            Some(LibraryViewState::GlobalSearch) => match self.library_view_stack.first() {
                Some(LibraryViewState::TracksRoot) => 0,
                Some(LibraryViewState::ArtistsRoot) => 1,
                Some(LibraryViewState::AlbumsRoot) => 2,
                Some(LibraryViewState::GenresRoot) => 3,
                Some(LibraryViewState::DecadesRoot) => 4,
                Some(LibraryViewState::FavoritesRoot) => 5,
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
            LibraryViewState::TracksRoot => ("Tracks".to_string(), String::new()),
            LibraryViewState::ArtistsRoot => ("Artists".to_string(), String::new()),
            LibraryViewState::AlbumsRoot => ("Albums".to_string(), String::new()),
            LibraryViewState::GenresRoot => ("Genres".to_string(), String::new()),
            LibraryViewState::DecadesRoot => ("Decades".to_string(), String::new()),
            LibraryViewState::FavoritesRoot => ("Favorites".to_string(), String::new()),
            LibraryViewState::FavoriteTracks => ("Favorite Tracks".to_string(), String::new()),
            LibraryViewState::FavoriteArtists => ("Favorite Artists".to_string(), String::new()),
            LibraryViewState::FavoriteAlbums => ("Favorite Albums".to_string(), String::new()),
            LibraryViewState::GlobalSearch => ("Global Search".to_string(), String::new()),
            LibraryViewState::ArtistDetail { artist } => (artist.clone(), String::new()),
            LibraryViewState::AlbumDetail {
                album,
                album_artist,
            } => (album.clone(), format!("by {}", album_artist)),
            LibraryViewState::GenreDetail { genre } => (genre.clone(), String::new()),
            LibraryViewState::DecadeDetail { decade } => (decade.clone(), String::new()),
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

    fn is_counted_failed_enrichment_result(payload: &protocol::LibraryEnrichmentPayload) -> bool {
        match payload.status {
            protocol::LibraryEnrichmentStatus::NotFound => true,
            protocol::LibraryEnrichmentStatus::Error => {
                payload.error_kind != Some(protocol::LibraryEnrichmentErrorKind::RateLimited)
            }
            _ => false,
        }
    }

    fn failed_enrichment_attempt_cap_reached(attempts: u32) -> bool {
        attempts >= ENRICHMENT_FAILED_ATTEMPT_CAP
    }

    fn is_enrichment_attempt_capped(&self, entity: &protocol::LibraryEnrichmentEntity) -> bool {
        let attempts = self
            .library_enrichment_failed_attempt_counts
            .get(entity)
            .copied()
            .unwrap_or(0);
        Self::failed_enrichment_attempt_cap_reached(attempts)
    }

    fn update_failed_enrichment_attempt_state(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
        payload: &protocol::LibraryEnrichmentPayload,
    ) {
        if payload.status == protocol::LibraryEnrichmentStatus::Ready
            || payload.status == protocol::LibraryEnrichmentStatus::Disabled
        {
            self.library_enrichment_failed_attempt_counts.remove(entity);
            return;
        }
        if !Self::is_counted_failed_enrichment_result(payload) {
            return;
        }
        let attempts = self
            .library_enrichment_failed_attempt_counts
            .entry(entity.clone())
            .and_modify(|value| *value = value.saturating_add(1))
            .or_insert(1);
        if *attempts == ENRICHMENT_FAILED_ATTEMPT_CAP {
            debug!(
                "Enrichment attempts capped for {:?} after {} failed attempts",
                entity, attempts
            );
        }
    }

    fn clear_enrichment_retry_state(&mut self, entity: &protocol::LibraryEnrichmentEntity) {
        self.library_enrichment_retry_counts.remove(entity);
        self.library_enrichment_retry_not_before.remove(entity);
    }

    fn clear_enrichment_not_found_backoff_state(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
    ) {
        self.library_enrichment_not_found_not_before.remove(entity);
    }

    fn schedule_enrichment_not_found_backoff(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
    ) {
        self.library_enrichment_not_found_not_before.insert(
            entity.clone(),
            Instant::now() + NOT_FOUND_ENRICHMENT_RETRY_INTERVAL,
        );
    }

    fn enrichment_not_found_backoff_elapsed(
        &self,
        entity: &protocol::LibraryEnrichmentEntity,
    ) -> bool {
        Self::enrichment_backoff_elapsed(self.library_enrichment_not_found_not_before.get(entity))
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
        Self::enrichment_backoff_elapsed(self.library_enrichment_retry_not_before.get(entity))
    }

    fn enrichment_backoff_elapsed(not_before: Option<&Instant>) -> bool {
        not_before.is_none_or(|deadline| Instant::now() >= *deadline)
    }

    fn should_request_prefetch_for_entity(
        &mut self,
        entity: &protocol::LibraryEnrichmentEntity,
    ) -> bool {
        if self.is_enrichment_attempt_capped(entity) {
            return false;
        }
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
        if self.is_enrichment_attempt_capped(&entity) {
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
            if existing.status == protocol::LibraryEnrichmentStatus::NotFound {
                if self.enrichment_not_found_backoff_elapsed(&entity) {
                    self.library_enrichment.remove(&entity);
                    self.clear_enrichment_not_found_backoff_state(&entity);
                } else {
                    return;
                }
            } else if Self::is_retryable_enrichment_result(&existing)
                && self.enrichment_retry_backoff_elapsed(&entity)
            {
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
            let should_refresh =
                self.library_enrichment.get(&entity).is_some_and(|payload| {
                    payload.status == protocol::LibraryEnrichmentStatus::Error
                }) && !self.is_enrichment_attempt_capped(&entity);
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
        if self.is_enrichment_attempt_capped(entity) {
            return false;
        }
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
        if !Self::library_view_supports_artist_enrichment_prefetch(&self.current_library_view()) {
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

        if Self::library_view_supports_artist_enrichment_prefetch(&self.current_library_view()) {
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
            LibraryViewState::ArtistsRoot
                | LibraryViewState::FavoriteArtists
                | LibraryViewState::GlobalSearch
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
            let mut presentation = self.library_row_presentation_from_entry(
                &LibraryEntry::Artist(artist.clone()),
                &view,
                selected_set.contains(&source_index),
                true,
            );
            presentation.cover_art_path =
                self.list_thumbnail_for_library_presentation(&presentation);
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

    fn refresh_visible_library_cover_art_rows(&mut self) -> bool {
        if self.collection_mode != COLLECTION_MODE_LIBRARY || self.library_view_indices.is_empty() {
            return false;
        }

        let view = self.current_library_view();
        let detail_header_visible = matches!(
            view,
            LibraryViewState::AlbumDetail { .. } | LibraryViewState::ArtistDetail { .. }
        );
        let (cover_decode_start, cover_decode_end) =
            self.library_cover_decode_window(self.library_view_indices.len());
        let (row_start, row_end) = if detail_header_visible {
            (0usize, self.library_view_indices.len())
        } else {
            (cover_decode_start, cover_decode_end)
        };
        if row_start >= row_end {
            return false;
        }

        let selected_set: HashSet<usize> = self.library_selected_indices.iter().copied().collect();
        let mut updates: Vec<(usize, LibraryRowPresentation)> = Vec::new();

        for row_index in row_start..row_end {
            let Some(source_index) = self.library_view_indices.get(row_index).copied() else {
                continue;
            };
            let Some(entry) = self.library_entries.get(source_index).cloned() else {
                continue;
            };
            let resolve_cover_art = matches!(entry, LibraryEntry::Artist(_))
                || detail_header_visible
                || (row_index >= cover_decode_start && row_index < cover_decode_end);
            if !resolve_cover_art {
                continue;
            }

            let mut presentation = self.library_row_presentation_from_entry(
                &entry,
                &view,
                selected_set.contains(&source_index),
                true,
            );
            presentation.cover_art_path =
                self.list_thumbnail_for_library_presentation(&presentation);
            if presentation.cover_art_path.is_none() {
                continue;
            }
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
        if let Some(cached) = self.library_cover_art_paths.get(track_path).cloned() {
            if let Some(path) = cached.clone() {
                if Self::failed_cover_path(&path) {
                    self.library_cover_art_paths
                        .insert(track_path.clone(), None);
                    return None;
                }
            } else if let Some(cached_embedded) =
                Self::embedded_art_cache_path_if_present(track_path)
            {
                self.library_cover_art_paths
                    .insert(track_path.clone(), Some(cached_embedded.clone()));
                self.track_cover_art_missing_tracks.remove(track_path);
                return Some(cached_embedded);
            } else if is_remote_track_path(track_path.as_path())
                && !self.track_cover_art_missing_tracks.contains(track_path)
            {
                self.update_cover_art(Some(track_path));
                self.track_cover_art_missing_tracks
                    .insert(track_path.clone());
            }
            return cached;
        }
        let resolved = if is_remote_track_path(track_path.as_path()) {
            self.update_cover_art(Some(track_path));
            None
        } else {
            self.find_external_cover_art_cached(track_path)
                .or_else(|| Self::embedded_art_cache_path_if_present(track_path))
        };
        if resolved.is_none() && !is_remote_track_path(track_path.as_path()) {
            // Cover-file lookup already failed; proactively process embedded art in background.
            self.queue_embedded_cover_art_prepare(track_path.as_path());
        }
        self.library_cover_art_paths
            .insert(track_path.clone(), resolved.clone());
        if resolved.is_none() {
            self.track_cover_art_missing_tracks
                .insert(track_path.clone());
        } else {
            self.track_cover_art_missing_tracks.remove(track_path);
        }
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
        let image_kind = if entry.item_kind == LIBRARY_ITEM_KIND_ARTIST {
            protocol::UiImageKind::ArtistImage
        } else {
            protocol::UiImageKind::CoverArt
        };
        let album_art = entry
            .cover_art_path
            .as_deref()
            .and_then(|path| UiManager::try_load_cover_art_image_with_kind(path, image_kind));
        let has_album_art = album_art.is_some();
        LibraryRowData {
            leading: entry.leading.into(),
            primary: entry.primary.into(),
            secondary: entry.secondary.into(),
            item_kind: entry.item_kind,
            album_art: album_art.unwrap_or_default(),
            has_album_art,
            source_badge: entry.source_badge.into(),
            is_playing: entry.is_playing,
            favoritable: entry.favoritable,
            favorited: entry.favorited,
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

    fn sync_library_playing_state_to_ui(&self) {
        let view_indices = self.library_view_indices.clone();
        let library_playing_index = self.library_playing_index;
        let library_playing_view_index = library_playing_index
            .and_then(|playing_source_index| {
                view_indices
                    .iter()
                    .position(|source_index| *source_index == playing_source_index)
            })
            .map(|index| index as i32)
            .unwrap_or(-1);
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_playing_track_index(library_playing_view_index);
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
                let is_playing = library_playing_index == Some(*source_index);
                if row_data.is_playing == is_playing {
                    continue;
                }
                row_data.is_playing = is_playing;
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

    fn sync_library_root_counts_to_ui(&self) {
        let counts: Vec<i32> = self
            .library_root_counts
            .iter()
            .map(|count| (*count).min(i32::MAX as usize) as i32)
            .collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_root_counts(ModelRc::from(Rc::new(VecModel::from(counts))));
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
        self.library_selection_anchor = Self::resolve_shift_anchor_source_index(
            &self.library_selected_indices,
            self.library_selection_anchor,
        );

        self.display_target_priority = DisplayTargetPriority::Selection;
        self.update_display_for_active_collection();

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
            let Some((key, spec)) = (match entry {
                LibraryEntry::Track(track) => Some((
                    format!("track:{}", track.path.to_string_lossy()),
                    protocol::LibrarySelectionSpec::Track {
                        path: track.path.clone(),
                    },
                )),
                LibraryEntry::Artist(artist) => Some((
                    format!("artist:{}", artist.artist),
                    protocol::LibrarySelectionSpec::Artist {
                        artist: artist.artist.clone(),
                    },
                )),
                LibraryEntry::Album(album) => Some((
                    format!("album:{}\u{001f}{}", album.album, album.album_artist),
                    protocol::LibrarySelectionSpec::Album {
                        album: album.album.clone(),
                        album_artist: album.album_artist.clone(),
                    },
                )),
                LibraryEntry::Genre(genre) => Some((
                    format!("genre:{}", genre.genre),
                    protocol::LibrarySelectionSpec::Genre {
                        genre: genre.genre.clone(),
                    },
                )),
                LibraryEntry::Decade(decade) => Some((
                    format!("decade:{}", decade.decade),
                    protocol::LibrarySelectionSpec::Decade {
                        decade: decade.decade.clone(),
                    },
                )),
                LibraryEntry::FavoriteCategory(_) => None,
            }) else {
                continue;
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
        let compact_track_row_view = matches!(
            view,
            LibraryViewState::AlbumDetail { .. } | LibraryViewState::ArtistDetail { .. }
        );
        let global_search_view = matches!(view, LibraryViewState::GlobalSearch);
        match entry {
            LibraryEntry::Track(track) => {
                let favorite_key = Self::favorite_key_for_track_path(track.path.as_path());
                LibraryRowPresentation {
                    leading: if compact_track_row_view {
                        Self::library_track_number_leading(&track.track_number)
                    } else {
                        String::new()
                    },
                    primary: track.title.clone(),
                    secondary: if compact_track_row_view {
                        track.artist.clone()
                    } else if global_search_view {
                        format!("Track • {} • {}", track.artist, track.album)
                    } else {
                        format!("{} • {}", track.artist, track.album)
                    },
                    item_kind: LIBRARY_ITEM_KIND_SONG,
                    cover_art_path: if compact_track_row_view || !resolve_cover_art {
                        None
                    } else {
                        self.resolve_library_cover_art_path(&track.path)
                    },
                    source_badge: Self::source_badge_for_track_path(track.path.as_path()),
                    is_playing: self.playing_track.path.as_ref() == Some(&track.path),
                    favoritable: true,
                    favorited: self.favorites_by_key.contains_key(&favorite_key),
                    selected,
                }
            }
            LibraryEntry::Artist(artist) => {
                let favorite_key = Self::favorite_key_for_artist(&artist.artist);
                LibraryRowPresentation {
                    leading: String::new(),
                    primary: artist.artist.clone(),
                    secondary: if global_search_view {
                        format!(
                            "Artist • {} albums • {} tracks",
                            artist.album_count, artist.track_count
                        )
                    } else {
                        format!(
                            "{} albums • {} tracks",
                            artist.album_count, artist.track_count
                        )
                    },
                    item_kind: LIBRARY_ITEM_KIND_ARTIST,
                    cover_art_path: if resolve_cover_art {
                        self.artist_enrichment_image_path(&artist.artist)
                    } else {
                        None
                    },
                    source_badge: String::new(),
                    is_playing: false,
                    favoritable: true,
                    favorited: self.favorites_by_key.contains_key(&favorite_key),
                    selected,
                }
            }
            LibraryEntry::Album(album) => {
                let favorite_key = Self::favorite_key_for_album(&album.album, &album.album_artist);
                LibraryRowPresentation {
                    leading: String::new(),
                    primary: album.album.clone(),
                    secondary: if global_search_view {
                        format!(
                            "Album • {} • {} tracks",
                            album.album_artist, album.track_count
                        )
                    } else {
                        format!("{} • {} tracks", album.album_artist, album.track_count)
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
                    source_badge: String::new(),
                    is_playing: false,
                    favoritable: true,
                    favorited: self.favorites_by_key.contains_key(&favorite_key),
                    selected,
                }
            }
            LibraryEntry::Genre(genre) => LibraryRowPresentation {
                leading: String::new(),
                primary: genre.genre.clone(),
                secondary: format!("{} tracks", genre.track_count),
                item_kind: LIBRARY_ITEM_KIND_GENRE,
                cover_art_path: None,
                source_badge: String::new(),
                is_playing: false,
                favoritable: false,
                favorited: false,
                selected,
            },
            LibraryEntry::Decade(decade) => LibraryRowPresentation {
                leading: String::new(),
                primary: decade.decade.clone(),
                secondary: format!("{} tracks", decade.track_count),
                item_kind: LIBRARY_ITEM_KIND_DECADE,
                cover_art_path: None,
                source_badge: String::new(),
                is_playing: false,
                favoritable: false,
                favorited: false,
                selected,
            },
            LibraryEntry::FavoriteCategory(category) => LibraryRowPresentation {
                leading: String::new(),
                primary: category.title.clone(),
                secondary: format!("{} items", category.count),
                item_kind: Self::item_kind_for_favorite_category(category.kind),
                cover_art_path: None,
                source_badge: String::new(),
                is_playing: false,
                favoritable: false,
                favorited: false,
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
        let detail_track_count = if detail_header_visible {
            entries
                .iter()
                .filter(|entry| matches!(entry, LibraryEntry::Track(_)))
                .count()
        } else {
            0
        };
        let album_year = if matches!(view, LibraryViewState::AlbumDetail { .. }) {
            entries.iter().find_map(|entry| {
                if let LibraryEntry::Track(track) = entry {
                    let year = track.year.trim();
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
                let tracks_label = if detail_track_count == 1 {
                    "track"
                } else {
                    "tracks"
                };
                if let Some(year) = album_year {
                    format!("{} • {} {}", year, detail_track_count, tracks_label)
                } else {
                    format!("{} {}", detail_track_count, tracks_label)
                }
            }
            LibraryViewState::ArtistDetail { .. } => {
                let albums_label = if detail_album_count == 1 {
                    "album"
                } else {
                    "albums"
                };
                let tracks_label = if detail_track_count == 1 {
                    "track"
                } else {
                    "tracks"
                };
                format!(
                    "{} {} • {} {}",
                    detail_album_count, albums_label, detail_track_count, tracks_label
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
                if let LibraryEntry::Track(track) = entry {
                    self.resolve_library_cover_art_path(&track.path)
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(entity) = detail_entity.clone() {
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
                    let mut presentation = self.library_row_presentation_from_entry(
                        entry,
                        &view,
                        selected_set.contains(source_index),
                        resolve_cover_art,
                    );
                    if resolve_cover_art {
                        presentation.cover_art_path =
                            self.list_thumbnail_for_library_presentation(&presentation);
                    } else {
                        presentation.cover_art_path = None;
                    }
                    if let Some(playing_idx) = self.library_playing_index {
                        if *source_index == playing_idx {
                            presentation.is_playing = true;
                        }
                    }
                    presentation
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
        let library_playing_view_index = self
            .library_playing_index
            .and_then(|playing_index| {
                library_view_indices
                    .iter()
                    .position(|source_index| *source_index == playing_index)
            })
            .map(|index| index as i32)
            .unwrap_or(-1);
        let fetch_capable_view = detail_entity.is_some()
            || Self::library_view_supports_artist_enrichment_prefetch(&view);
        let online_prompt_visible = self.collection_mode == COLLECTION_MODE_LIBRARY
            && fetch_capable_view
            && self.library_online_metadata_prompt_pending;
        let detail_header_image_kind = if matches!(view, LibraryViewState::ArtistDetail { .. }) {
            protocol::UiImageKind::ArtistImage
        } else {
            protocol::UiImageKind::CoverArt
        };

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
            let album_header_art = detail_header_art_path.as_deref().and_then(|path| {
                UiManager::try_load_detail_cover_art_image_with_kind(
                    path,
                    detail_header_image_kind,
                    DETAIL_COMPACT_RENDER_MAX_EDGE_PX,
                    DETAIL_COMPACT_RENDER_MAX_EDGE_PX,
                )
            });
            ui.set_library_album_header_has_art(album_header_art.is_some());
            ui.set_library_album_header_art(album_header_art.unwrap_or_default());
            ui.set_library_album_header_meta(detail_header_meta.into());
            ui.set_library_detail_header_blurb(detail_header_blurb.into());
            ui.set_library_detail_header_source_name(detail_header_source_name.into());
            ui.set_library_detail_header_source_url(detail_header_source_url.into());
            ui.set_library_detail_header_source_visible(detail_header_source_visible);
            ui.set_library_detail_header_loading(detail_header_loading);
            ui.set_library_online_prompt_visible(online_prompt_visible);
            ui.set_library_playing_track_index(library_playing_view_index);
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
        self.clear_search_bars_for_track_list_view_switch();
        self.collection_mode = normalized_mode;
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.update_library_playing_index();
            self.request_library_view_data();
        } else {
            self.library_add_to_dialog_visible = false;
            self.sync_library_add_to_playlist_ui();
        }
        self.update_display_for_active_collection();
        self.sync_library_ui();
        self.auto_scroll_active_collection_to_playing_track();
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
        self.clear_search_bars_for_track_list_view_switch();
        self.remember_current_library_scroll_position();
        let root = match section {
            1 => LibraryViewState::ArtistsRoot,
            2 => LibraryViewState::AlbumsRoot,
            3 => LibraryViewState::GenresRoot,
            4 => LibraryViewState::DecadesRoot,
            5 => LibraryViewState::FavoritesRoot,
            _ => LibraryViewState::TracksRoot,
        };
        self.library_view_stack.clear();
        self.library_view_stack.push(root);
        self.library_artist_prefetch_first_row = 0;
        self.library_artist_prefetch_row_count = 0;
        self.pending_library_scroll_restore_row = None;
        self.prepare_library_view_transition();
        self.update_display_for_active_collection();
        self.request_library_view_data();
        self.sync_library_ui();
    }

    fn navigate_library_back(&mut self) {
        if self.library_view_stack.len() <= 1 {
            return;
        }
        self.clear_search_bars_for_track_list_view_switch();
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
        self.update_display_for_active_collection();
        self.request_library_view_data();
        self.sync_library_ui();
    }

    fn library_page_query_for_view(view: &LibraryViewState) -> protocol::LibraryViewQuery {
        match view {
            LibraryViewState::TracksRoot => protocol::LibraryViewQuery::Tracks,
            LibraryViewState::ArtistsRoot => protocol::LibraryViewQuery::Artists,
            LibraryViewState::AlbumsRoot => protocol::LibraryViewQuery::Albums,
            LibraryViewState::GenresRoot => protocol::LibraryViewQuery::Genres,
            LibraryViewState::DecadesRoot => protocol::LibraryViewQuery::Decades,
            LibraryViewState::FavoritesRoot => protocol::LibraryViewQuery::FavoritesRoot,
            LibraryViewState::FavoriteTracks => protocol::LibraryViewQuery::FavoriteTracks,
            LibraryViewState::FavoriteArtists => protocol::LibraryViewQuery::FavoriteArtists,
            LibraryViewState::FavoriteAlbums => protocol::LibraryViewQuery::FavoriteAlbums,
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
            protocol::LibraryEntryPayload::Track(track) => LibraryEntry::Track(track),
            protocol::LibraryEntryPayload::Artist(artist) => LibraryEntry::Artist(artist),
            protocol::LibraryEntryPayload::Album(album) => LibraryEntry::Album(album),
            protocol::LibraryEntryPayload::Genre(genre) => LibraryEntry::Genre(genre),
            protocol::LibraryEntryPayload::Decade(decade) => LibraryEntry::Decade(decade),
            protocol::LibraryEntryPayload::FavoriteCategory(category) => {
                LibraryEntry::FavoriteCategory(category)
            }
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

        let page_view = self.library_page_view.clone();
        let mut final_entries: Vec<LibraryEntry> = std::mem::take(&mut self.library_page_entries)
            .into_iter()
            .map(Self::library_entry_from_payload)
            .collect();
        if matches!(
            page_view,
            Some(protocol::LibraryViewQuery::ArtistDetail { .. })
        ) {
            let mut albums = Vec::new();
            let mut tracks = Vec::new();
            for entry in final_entries {
                match entry {
                    LibraryEntry::Album(album) => albums.push(album),
                    LibraryEntry::Track(track) => tracks.push(track),
                    LibraryEntry::Artist(_)
                    | LibraryEntry::Genre(_)
                    | LibraryEntry::Decade(_)
                    | LibraryEntry::FavoriteCategory(_) => {}
                }
            }
            final_entries = Self::build_artist_detail_entries(albums, tracks);
        }
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

    fn request_library_root_counts(&self) {
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::RequestRootCounts,
        ));
    }

    fn set_library_entries(&mut self, entries: Vec<LibraryEntry>) {
        self.library_entries = entries;
        self.update_library_playing_index();
        self.reset_library_selection();
        self.library_add_to_dialog_visible = false;
        self.sync_library_add_to_playlist_ui();
        self.update_display_for_active_collection();
        self.sync_library_ui();
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.auto_scroll_active_collection_to_playing_track();
        }
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
                self.library_enrichment_not_found_not_before.clear();
                self.library_enrichment_failed_attempt_counts.clear();
                self.replace_prefetch_queue_if_changed(Vec::new());
                self.replace_background_queue_if_changed(Vec::new());
                self.library_last_detail_enrichment_entity = None;
                self.library_add_to_dialog_visible = false;
                self.sync_library_add_to_playlist_ui();
                self.sync_library_scan_status_to_ui();
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
                self.sync_library_scan_status_to_ui();
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
                self.library_enrichment_not_found_not_before.clear();
                self.library_enrichment_failed_attempt_counts.clear();
                self.replace_prefetch_queue_if_changed(Vec::new());
                self.replace_background_queue_if_changed(Vec::new());
                self.library_last_detail_enrichment_entity = None;
                self.sync_library_scan_status_to_ui();
                self.request_library_view_data();
                self.request_library_root_counts();
            }
            protocol::LibraryMessage::MetadataBackfillProgress { updated, remaining } => {
                if remaining == 0 {
                    self.library_status_text =
                        format!("Metadata backfill complete ({} updated)", updated);
                    self.sync_library_scan_status_to_ui();
                    self.request_library_view_data();
                    self.request_library_root_counts();
                } else {
                    self.library_status_text = format!(
                        "Metadata backfill: {} updated, {} remaining",
                        updated, remaining
                    );
                    self.sync_library_scan_status_to_ui();
                }
            }
            protocol::LibraryMessage::ScanFailed(error_text) => {
                self.library_scan_in_progress = false;
                self.library_status_text = error_text;
                self.sync_library_scan_status_to_ui();
            }
            _ => {}
        }
    }

    fn drain_scan_progress_queue(&mut self) {
        while let Ok(message) = self.library_scan_progress_rx.try_recv() {
            self.handle_scan_status_message(message);
        }
    }

    fn has_paused_playback_queue(&self) -> bool {
        !self.playback_active && self.playing_track.path.is_some()
    }

    fn build_playlist_queue_request(
        &self,
        preferred_view_index: Option<usize>,
    ) -> Option<protocol::PlaybackQueueRequest> {
        let preferred_source_index =
            preferred_view_index.and_then(|view_index| self.map_view_to_source_index(view_index));
        let (tracks, start_index) = Self::build_playlist_queue_snapshot(
            &self.track_ids,
            &self.track_paths,
            &self.view_indices,
            &self.selected_indices,
            preferred_source_index,
        )?;
        Some(protocol::PlaybackQueueRequest {
            source: protocol::PlaybackQueueSource::Playlist {
                playlist_id: self.active_playlist_id.clone(),
            },
            tracks,
            start_index,
        })
    }

    /// Build the playback queue snapshot in **view order** (filtered/sorted).
    ///
    /// The returned `tracks` vector preserves the order the user sees on screen,
    /// so next/previous navigation in the `playback_playlist` matches the
    /// visual ordering.  The `start_index` is an offset *within this queue*,
    /// not a source index.
    ///
    /// Because the queue is emitted in view order, the `playing_index` that
    /// `PlaylistManager` later broadcasts in `PlaylistIndicesChanged` is a
    /// queue-relative index and must **not** be treated as a source index by
    /// the UI.  See `resolve_active_playing_source_index` for the correct
    /// back-mapping strategy.
    fn build_playlist_queue_snapshot(
        track_ids: &[String],
        track_paths: &[PathBuf],
        view_indices: &[usize],
        selected_indices: &[usize],
        preferred_source_index: Option<usize>,
    ) -> Option<(Vec<protocol::RestoredTrack>, usize)> {
        let selected_set: HashSet<usize> = selected_indices.iter().copied().collect();
        let mut selected_start_index = None;
        let mut explicit_start_index = None;
        let mut tracks = Vec::with_capacity(view_indices.len());

        for source_index in view_indices.iter().copied() {
            let Some(track_id) = track_ids.get(source_index) else {
                continue;
            };
            let Some(track_path) = track_paths.get(source_index) else {
                continue;
            };
            let queue_index = tracks.len();
            if preferred_source_index == Some(source_index) {
                explicit_start_index = Some(queue_index);
            }
            if selected_start_index.is_none() && selected_set.contains(&source_index) {
                selected_start_index = Some(queue_index);
            }
            tracks.push(protocol::RestoredTrack {
                id: track_id.clone(),
                path: track_path.clone(),
            });
        }

        if tracks.is_empty() {
            return None;
        }

        let start_index = explicit_start_index.or(selected_start_index).unwrap_or(0);
        Some((tracks, start_index))
    }

    /// Resolve the currently-playing track to a **source index** in the
    /// editing playlist's parallel arrays (`track_ids`, `track_paths`, etc.).
    ///
    /// # Why track-ID lookup is required (not `playing_index`)
    ///
    /// `playing_index` in `PlaylistIndicesChanged` originates from the
    /// *playback playlist*, which is a queue snapshot built by
    /// `build_playlist_queue_snapshot` in **view order** (filtered/sorted).
    /// It is NOT a source index into the editing playlist.  Using it directly
    /// would highlight the wrong track whenever a sort or search filter is
    /// active, and would break next/prev visual consistency.
    ///
    /// The stable track **id** is the only reliable key that uniquely
    /// identifies the exact playlist entry, even when:
    /// - A filter or sort view reorders the playback queue relative to the
    ///   editing playlist's source order.
    /// - The same file path appears more than once in the playlist (duplicates).
    /// - The playlist contains a mix of local and remote tracks.
    ///
    /// `playing_track_path` is kept as a fallback for robustness in the rare
    /// case that `playing_track_id` is `None` (e.g. stale messages from older
    /// protocol versions).
    fn resolve_active_playing_source_index(
        track_ids: &[String],
        track_paths: &[PathBuf],
        is_playing_active_playlist: bool,
        playing_track_id: Option<&str>,
        playing_track_path: Option<&PathBuf>,
    ) -> Option<usize> {
        if !is_playing_active_playlist {
            return None;
        }
        // Primary: resolve via the stable track id (unique per playlist entry).
        if let Some(id) = playing_track_id {
            if let Some(index) = track_ids.iter().position(|candidate| candidate == id) {
                return Some(index);
            }
        }
        // Fallback: resolve via file path when track id is unavailable.
        // This finds the first match, which is acceptable as a best-effort
        // fallback but may be ambiguous for duplicate-path entries.
        playing_track_path.and_then(|playing_path| {
            track_paths
                .iter()
                .position(|candidate| candidate == playing_path)
        })
    }

    /// Build a library playback queue in **view order** (filtered/searched).
    ///
    /// Analogous to `build_playlist_queue_snapshot` — the returned queue
    /// preserves the on-screen row order so that next/previous navigation
    /// matches what the user sees.  The `start_index` is an offset within this
    /// queue, not a source index into `library_entries`.
    fn build_library_queue_request(
        &self,
        preferred_source_index: Option<usize>,
    ) -> Option<protocol::PlaybackQueueRequest> {
        let selected_set: HashSet<usize> = self.library_selected_indices.iter().copied().collect();
        let mut selected_start_index = None;
        let mut explicit_start_index = None;
        let mut tracks = Vec::new();

        for source_index in self.library_view_indices.iter().copied() {
            let Some(LibraryEntry::Track(track)) = self.library_entries.get(source_index) else {
                continue;
            };
            let queue_index = tracks.len();
            if preferred_source_index == Some(source_index) {
                explicit_start_index = Some(queue_index);
            }
            if selected_start_index.is_none() && selected_set.contains(&source_index) {
                selected_start_index = Some(queue_index);
            }
            tracks.push(protocol::RestoredTrack {
                id: track.id.clone(),
                path: track.path.clone(),
            });
        }

        if tracks.is_empty() {
            return None;
        }

        let start_index = explicit_start_index.or(selected_start_index).unwrap_or(0);
        Some(protocol::PlaybackQueueRequest {
            source: protocol::PlaybackQueueSource::Library,
            tracks,
            start_index,
        })
    }

    fn start_queue_if_possible(&self, request: Option<protocol::PlaybackQueueRequest>) {
        let Some(request) = request else {
            return;
        };
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::StartQueue(request),
        ));
    }

    fn play_active_collection(&self) {
        if self.has_paused_playback_queue() {
            let _ = self
                .bus_sender
                .send(protocol::Message::Playback(protocol::PlaybackMessage::Play));
            return;
        }
        let request = if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.build_library_queue_request(None)
        } else {
            self.build_playlist_queue_request(None)
        };
        self.start_queue_if_possible(request);
    }

    fn activate_library_item(&mut self, index: usize) {
        let Some(entry) = self.library_entries.get(index).cloned() else {
            return;
        };

        match entry {
            LibraryEntry::Track(_) => {
                self.start_queue_if_possible(self.build_library_queue_request(Some(index)));
            }
            LibraryEntry::Artist(artist) => {
                self.clear_search_bars_for_track_list_view_switch();
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
                self.clear_search_bars_for_track_list_view_switch();
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
                self.clear_search_bars_for_track_list_view_switch();
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
                self.clear_search_bars_for_track_list_view_switch();
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
            LibraryEntry::FavoriteCategory(category) => {
                self.clear_search_bars_for_track_list_view_switch();
                self.remember_current_library_scroll_position();
                let next_view = match category.kind {
                    protocol::FavoriteEntityKind::Track => LibraryViewState::FavoriteTracks,
                    protocol::FavoriteEntityKind::Artist => LibraryViewState::FavoriteArtists,
                    protocol::FavoriteEntityKind::Album => LibraryViewState::FavoriteAlbums,
                };
                self.library_view_stack.push(next_view);
                self.prepare_library_view_transition();
                self.request_library_view_data();
                self.sync_library_ui();
            }
        }
    }

    fn toggle_favorite_for_library_row(&self, view_row: usize) {
        let Some(source_index) = self.map_library_view_to_source_index(view_row) else {
            return;
        };
        let Some(entry) = self.library_entries.get(source_index) else {
            return;
        };
        let Some(entity) = self.favorite_entity_for_library_entry(entry) else {
            return;
        };
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::ToggleFavorite {
                entity,
                desired: None,
            },
        ));
    }

    fn toggle_favorite_for_playlist_row(&self, view_row: usize) {
        let Some(source_index) = self.map_view_to_source_index(view_row) else {
            return;
        };
        let Some(entity) = self.favorite_entity_for_playlist_source_index(source_index) else {
            return;
        };
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::ToggleFavorite {
                entity,
                desired: None,
            },
        ));
    }

    fn toggle_favorite_now_playing(&self) {
        let Some(entity) = self.current_track_favorite_entity() else {
            return;
        };
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::ToggleFavorite {
                entity,
                desired: None,
            },
        ));
    }

    fn apply_favorites_snapshot(&mut self, items: Vec<protocol::FavoriteEntityRef>) {
        self.favorites_by_key = items
            .into_iter()
            .map(|item| (item.entity_key.clone(), item))
            .collect();
        self.rebuild_track_model();
        self.sync_library_ui();
        self.sync_now_playing_favorite_state_to_ui();
    }

    fn fallback_track_metadata(path: &Path) -> TrackMetadata {
        if is_remote_track_path(path) {
            return TrackMetadata {
                title: "Remote track".to_string(),
                artist: "".to_string(),
                album: "".to_string(),
                album_artist: "".to_string(),
                date: "".to_string(),
                year: "".to_string(),
                genre: "".to_string(),
                track_number: "".to_string(),
            };
        }
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
        if is_remote_track_path(track_path.as_path()) {
            return;
        }
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
            if update.summary.title == REMOTE_TRACK_UNAVAILABLE_TITLE {
                self.unavailable_track_ids.insert(update.track_id.clone());
            } else {
                self.unavailable_track_ids.remove(&update.track_id);
            }
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

    /// Selects all visible tracks in the active collection.
    ///
    /// In playlist mode, selects all tracks visible in the current view
    /// (which may be a filtered/sorted subset).  In library mode, selects
    /// all visible library entries.
    fn handle_select_all(&mut self) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            // Library: select all visible entries by source index.
            self.library_selected_indices = if self.library_view_indices.is_empty() {
                (0..self.library_entries.len()).collect()
            } else {
                self.library_view_indices.clone()
            };
            self.library_selected_indices.sort_unstable();
            self.library_selected_indices.dedup();
            self.display_target_priority = DisplayTargetPriority::Selection;
            self.update_display_for_active_collection();
            self.sync_library_selection_to_ui();
            self.sync_library_add_to_playlist_ui();
            self.sync_properties_action_state();
        } else {
            // Playlist: select all visible tracks (view-order source indices).
            let all_source_indices: Vec<usize> = if self.view_indices.is_empty() {
                (0..self.track_metadata.len()).collect()
            } else {
                self.view_indices.clone()
            };
            if all_source_indices.is_empty() {
                return;
            }
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectionChanged(all_source_indices),
            ));
        }
    }

    /// Handles Up/Down arrow key navigation in the active collection.
    ///
    /// `direction` is -1 (up) or +1 (down).  When `shift` is true the
    /// selection extends from the current anchor; otherwise the selection
    /// collapses to the single navigated row.  Works correctly in both
    /// filtered/sorted views and unfiltered views.
    fn handle_arrow_key_navigate(&mut self, direction: i32, shift: bool) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.arrow_navigate_library(direction, shift);
        } else {
            self.arrow_navigate_playlist(direction, shift);
        }
    }

    fn arrow_navigate_playlist(&mut self, direction: i32, shift: bool) {
        let view_len = if self.view_indices.is_empty() {
            self.track_metadata.len()
        } else {
            self.view_indices.len()
        };
        if view_len == 0 {
            return;
        }

        let anchor_source_index = self.selection_anchor_source_index();
        let current_view_row = if shift {
            self.selection_lead_source_index(anchor_source_index)
                .and_then(|lead| self.map_source_to_view_index(lead))
        } else {
            self.selected_indices
                .last()
                .and_then(|&source_index| self.map_source_to_view_index(source_index))
        };

        let next_view_row = match current_view_row {
            Some(row) => {
                let next = row as i32 + direction;
                next.clamp(0, view_len as i32 - 1) as usize
            }
            None => {
                if direction > 0 {
                    0
                } else {
                    view_len - 1
                }
            }
        };

        let Some(target_source_index) = self.map_view_to_source_index(next_view_row) else {
            return;
        };

        if shift {
            let selected = Self::build_shift_selection_from_view_order(
                &self.view_indices,
                anchor_source_index,
                target_source_index,
            );
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectionChanged(selected),
            ));
        } else {
            self.set_selection_anchor_from_source_index(target_source_index);
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectionChanged(vec![target_source_index]),
            ));
        }

        // Scroll to ensure the navigated row is visible.
        self.playlist_scroll_center_token = self.playlist_scroll_center_token.wrapping_add(1);
        let token = self.playlist_scroll_center_token;
        let row = next_view_row as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_scroll_target_row(row);
            ui.set_playlist_scroll_center_token(token);
        });
    }

    fn arrow_navigate_library(&mut self, direction: i32, shift: bool) {
        let view_len = if self.library_view_indices.is_empty() {
            self.library_entries.len()
        } else {
            self.library_view_indices.len()
        };
        if view_len == 0 {
            return;
        }

        let current_view_row = if shift {
            self.library_selection_lead_source_index(self.library_selection_anchor)
                .and_then(|lead| self.map_library_source_to_view_index(lead))
        } else {
            self.library_selected_indices
                .last()
                .and_then(|&source_index| self.map_library_source_to_view_index(source_index))
        };

        let next_view_row = match current_view_row {
            Some(row) => {
                let next = row as i32 + direction;
                next.clamp(0, view_len as i32 - 1) as usize
            }
            None => {
                if direction > 0 {
                    0
                } else {
                    view_len - 1
                }
            }
        };

        let Some(target_source_index) = self.map_library_view_to_source_index(next_view_row) else {
            return;
        };

        if shift {
            let selected = Self::build_shift_selection_from_view_order(
                &self.library_view_indices,
                self.library_selection_anchor,
                target_source_index,
            );
            self.library_selected_indices = selected;
        } else {
            self.library_selected_indices = vec![target_source_index];
            self.library_selection_anchor = Some(target_source_index);
        }
        self.library_selected_indices.sort_unstable();
        self.library_selected_indices.dedup();

        self.display_target_priority = DisplayTargetPriority::Selection;
        self.update_display_for_active_collection();
        self.sync_library_selection_to_ui();
        self.sync_library_add_to_playlist_ui();
        self.sync_properties_action_state();

        // Scroll to ensure the navigated row is visible.
        self.library_scroll_center_token = self.library_scroll_center_token.wrapping_add(1);
        let token = self.library_scroll_center_token;
        let row = next_view_row as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_scroll_target_row(row);
            ui.set_library_scroll_center_token(token);
        });
    }

    fn handle_page_navigate(
        &mut self,
        action: protocol::PageNavigationAction,
        shift: bool,
        visible_row_count: usize,
    ) {
        if self.collection_mode == COLLECTION_MODE_LIBRARY {
            self.page_navigate_library(action, shift, visible_row_count);
        } else {
            self.page_navigate_playlist(action, shift, visible_row_count);
        }
    }

    fn page_navigate_playlist(
        &mut self,
        action: protocol::PageNavigationAction,
        shift: bool,
        visible_row_count: usize,
    ) {
        let view_len = if self.view_indices.is_empty() {
            self.track_metadata.len()
        } else {
            self.view_indices.len()
        };
        if view_len == 0 {
            return;
        }

        let anchor_source_index = self.selection_anchor_source_index();
        let current_view_row = if shift {
            self.selection_lead_source_index(anchor_source_index)
                .and_then(|lead| self.map_source_to_view_index(lead))
        } else {
            self.selected_indices
                .last()
                .and_then(|&source_index| self.map_source_to_view_index(source_index))
        };

        let target_view_row = match action {
            protocol::PageNavigationAction::Home => 0,
            protocol::PageNavigationAction::End => view_len.saturating_sub(1),
            protocol::PageNavigationAction::PageUp => {
                let page_size = visible_row_count.max(1);
                current_view_row
                    .map(|row| row.saturating_sub(page_size))
                    .unwrap_or(0)
            }
            protocol::PageNavigationAction::PageDown => {
                let page_size = visible_row_count.max(1);
                current_view_row
                    .map(|row| (row + page_size).min(view_len.saturating_sub(1)))
                    .unwrap_or(0)
            }
        };

        let Some(target_source_index) = self.map_view_to_source_index(target_view_row) else {
            return;
        };

        if shift {
            let selected = Self::build_shift_selection_from_view_order(
                &self.view_indices,
                anchor_source_index,
                target_source_index,
            );
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectionChanged(selected),
            ));
        } else {
            self.set_selection_anchor_from_source_index(target_source_index);
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectionChanged(vec![target_source_index]),
            ));
        }

        self.playlist_scroll_center_token = self.playlist_scroll_center_token.wrapping_add(1);
        let token = self.playlist_scroll_center_token;
        let row = target_view_row as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_scroll_target_row(row);
            ui.set_playlist_scroll_center_token(token);
        });
    }

    fn page_navigate_library(
        &mut self,
        action: protocol::PageNavigationAction,
        shift: bool,
        visible_row_count: usize,
    ) {
        let view_len = if self.library_view_indices.is_empty() {
            self.library_entries.len()
        } else {
            self.library_view_indices.len()
        };
        if view_len == 0 {
            return;
        }

        let current_view_row = if shift {
            self.library_selection_lead_source_index(self.library_selection_anchor)
                .and_then(|lead| self.map_library_source_to_view_index(lead))
        } else {
            self.library_selected_indices
                .last()
                .and_then(|&source_index| self.map_library_source_to_view_index(source_index))
        };

        let target_view_row = match action {
            protocol::PageNavigationAction::Home => 0,
            protocol::PageNavigationAction::End => view_len.saturating_sub(1),
            protocol::PageNavigationAction::PageUp => {
                let page_size = visible_row_count.max(1);
                current_view_row
                    .map(|row| row.saturating_sub(page_size))
                    .unwrap_or(0)
            }
            protocol::PageNavigationAction::PageDown => {
                let page_size = visible_row_count.max(1);
                current_view_row
                    .map(|row| (row + page_size).min(view_len.saturating_sub(1)))
                    .unwrap_or(0)
            }
        };

        let Some(target_source_index) = self.map_library_view_to_source_index(target_view_row)
        else {
            return;
        };

        if shift {
            let selected = Self::build_shift_selection_from_view_order(
                &self.library_view_indices,
                self.library_selection_anchor,
                target_source_index,
            );
            self.library_selected_indices = selected;
        } else {
            self.library_selected_indices = vec![target_source_index];
            self.library_selection_anchor = Some(target_source_index);
        }
        self.library_selected_indices.sort_unstable();
        self.library_selected_indices.dedup();

        self.display_target_priority = DisplayTargetPriority::Selection;
        self.update_display_for_active_collection();
        self.sync_library_selection_to_ui();
        self.sync_library_add_to_playlist_ui();
        self.sync_properties_action_state();

        self.library_scroll_center_token = self.library_scroll_center_token.wrapping_add(1);
        let token = self.library_scroll_center_token;
        let row = target_view_row as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_library_scroll_target_row(row);
            ui.set_library_scroll_center_token(token);
        });
    }

    /// Handles selection gesture start from the UI overlay.
    pub fn on_pointer_down(&mut self, pressed_index: usize, ctrl: bool, shift: bool) {
        trace!(
            "on_pointer_down: index={}, ctrl={}, shift={}",
            pressed_index,
            ctrl,
            shift
        );
        let Some(source_index) = self.map_view_to_source_index(pressed_index) else {
            self.pressed_index = None;
            self.pending_single_select_on_click = None;
            return;
        };
        self.pressed_index = Some(source_index);
        self.pending_single_select_on_click = None;

        let is_already_selected = self.selected_indices.contains(&source_index);
        if is_already_selected && !ctrl && !shift && self.selected_indices.len() > 1 {
            // Defer collapse to pointer-up so dragging still moves the full selection.
            self.pending_single_select_on_click = Some(source_index);
            self.set_selection_anchor_from_source_index(source_index);
            return;
        }
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
            // Drag-reorder is blocked in filter/sort views.  Do NOT clear
            // pending_single_select_on_click here — it must survive until
            // on_drag_end so that click-to-collapse-multiselect still works.
            self.drag_indices.clear();
            self.is_dragging = false;
            return;
        }
        self.pending_single_select_on_click = None;

        let Some(source_index) = self.map_view_to_source_index(pressed_index) else {
            return;
        };
        trace!(
            ">>> on_drag_start START: pressed_index={}, self.selected_indices={:?}",
            source_index,
            self.selected_indices
        );
        if self.selected_indices.contains(&source_index) {
            self.drag_indices = self.selected_indices.clone();
            trace!(
                ">>> MULTI-SELECT DRAG: using drag_indices {:?}",
                self.drag_indices
            );
        } else {
            self.drag_indices = vec![source_index];
            trace!(">>> SINGLE DRAG: using index {:?}", self.drag_indices);
        }
        self.drag_indices.sort();
        trace!(
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
    pub fn on_drag_end(&mut self, drop_gap: usize, drag_blocked: bool) {
        if self.is_filter_view_active() {
            // Drag-reorder is blocked in filter/sort views. Only collapse
            // selection if this was a simple click, not a blocked drag attempt.
            if !drag_blocked {
                if let Some(source_index) = self.pending_single_select_on_click.take() {
                    let _ = self.bus_sender.send(protocol::Message::Playlist(
                        protocol::PlaylistMessage::SelectionChanged(vec![source_index]),
                    ));
                }
            }
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

        trace!(
            ">>> on_drag_end START: is_dragging={}, drop_gap={}, drag_indices={:?}",
            self.is_dragging,
            drop_gap,
            self.drag_indices
        );
        if self.is_dragging && !self.drag_indices.is_empty() {
            let indices = self.drag_indices.clone();
            let to = drop_gap;
            trace!(
                ">>> on_drag_end SENDING ReorderTracks: indices={:?}, to={}",
                indices,
                to
            );

            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::ReorderTracks { indices, to },
            ));
        } else if let Some(source_index) = self.pending_single_select_on_click.take() {
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectionChanged(vec![source_index]),
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

    fn apply_ui_library_config_updates(
        &mut self,
        ui_update: Option<protocol::UiConfigDelta>,
        library_update: Option<protocol::LibraryConfigDelta>,
    ) {
        if ui_update.is_none() && library_update.is_none() {
            return;
        }

        let previous_list_image_max_edge_px = self.list_image_max_edge_px;
        let previous_cover_art_cache_max_size_mb = self.cover_art_cache_max_size_mb;
        let previous_artist_image_cache_max_size_mb = self.artist_image_cache_max_size_mb;
        let previous_cover_art_memory_cache_max_size_mb = self.cover_art_memory_cache_max_size_mb;
        let previous_artist_image_memory_cache_max_size_mb =
            self.artist_image_memory_cache_max_size_mb;
        let previous_library_online_metadata_enabled = self.library_online_metadata_enabled;
        let previous_library_online_metadata_prompt_pending =
            self.library_online_metadata_prompt_pending;
        let previous_playlist_columns = self.playlist_columns.clone();
        let previous_album_art_column_min_width_px = self.album_art_column_min_width_px;
        let previous_album_art_column_max_width_px = self.album_art_column_max_width_px;

        let mut list_image_max_edge_changed = false;
        let mut online_metadata_enabled_changed = false;
        let mut online_metadata_prompt_changed = false;

        if let Some(library) = library_update {
            if let Some(value) = library.online_metadata_enabled {
                self.library_online_metadata_enabled = value;
            }
            if let Some(value) = library.online_metadata_prompt_pending {
                self.library_online_metadata_prompt_pending = value;
            }
            if let Some(value) = library.list_image_max_edge_px {
                self.list_image_max_edge_px = value;
            }
            if let Some(value) = library.cover_art_cache_max_size_mb {
                self.cover_art_cache_max_size_mb = value;
            }
            if let Some(value) = library.artist_image_cache_max_size_mb {
                self.artist_image_cache_max_size_mb = value;
            }
            if let Some(value) = library.cover_art_memory_cache_max_size_mb {
                self.cover_art_memory_cache_max_size_mb = value;
            }
            if let Some(value) = library.artist_image_memory_cache_max_size_mb {
                self.artist_image_memory_cache_max_size_mb = value;
            }

            list_image_max_edge_changed =
                previous_list_image_max_edge_px != self.list_image_max_edge_px;
            let cover_art_cache_budget_changed =
                previous_cover_art_cache_max_size_mb != self.cover_art_cache_max_size_mb;
            let artist_image_cache_budget_changed =
                previous_artist_image_cache_max_size_mb != self.artist_image_cache_max_size_mb;
            let cover_art_memory_budget_changed = previous_cover_art_memory_cache_max_size_mb
                != self.cover_art_memory_cache_max_size_mb;
            let artist_image_memory_budget_changed = previous_artist_image_memory_cache_max_size_mb
                != self.artist_image_memory_cache_max_size_mb;
            online_metadata_enabled_changed =
                previous_library_online_metadata_enabled != self.library_online_metadata_enabled;
            online_metadata_prompt_changed = previous_library_online_metadata_prompt_pending
                != self.library_online_metadata_prompt_pending;

            if list_image_max_edge_changed
                || cover_art_cache_budget_changed
                || artist_image_cache_budget_changed
            {
                image_pipeline::configure_runtime_limits(
                    self.list_image_max_edge_px,
                    self.cover_art_cache_max_size_mb,
                    self.artist_image_cache_max_size_mb,
                );
            }
            if cover_art_memory_budget_changed {
                COVER_ART_MEMORY_CACHE_BUDGET_BYTES.store(
                    image_pipeline::mb_to_bytes(self.cover_art_memory_cache_max_size_mb),
                    Ordering::Relaxed,
                );
            }
            if artist_image_memory_budget_changed {
                ARTIST_IMAGE_MEMORY_CACHE_BUDGET_BYTES.store(
                    image_pipeline::mb_to_bytes(self.artist_image_memory_cache_max_size_mb),
                    Ordering::Relaxed,
                );
            }
            if list_image_max_edge_changed {
                TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| {
                    cache.borrow_mut().clear();
                });
                TRACK_ROW_ARTIST_IMAGE_CACHE.with(|cache| {
                    cache.borrow_mut().clear();
                });
                self.pending_list_image_requests.clear();
            }
            if list_image_max_edge_changed
                || cover_art_cache_budget_changed
                || artist_image_cache_budget_changed
            {
                let _ = image_pipeline::prune_kind_disk_cache(
                    ManagedImageKind::CoverArt,
                    image_pipeline::mb_to_bytes(self.cover_art_cache_max_size_mb),
                );
                let _ = image_pipeline::prune_kind_disk_cache(
                    ManagedImageKind::ArtistImage,
                    image_pipeline::mb_to_bytes(self.artist_image_cache_max_size_mb),
                );
            }
            if online_metadata_enabled_changed && !self.library_online_metadata_enabled {
                self.library_enrichment_pending.clear();
                self.library_enrichment_last_request_at.clear();
                self.library_enrichment_retry_counts.clear();
                self.library_enrichment_retry_not_before.clear();
                self.library_enrichment_not_found_not_before.clear();
                self.library_enrichment_failed_attempt_counts.clear();
                self.replace_prefetch_queue_if_changed(Vec::new());
                self.replace_background_queue_if_changed(Vec::new());
                self.library_last_detail_enrichment_entity = None;
            }
        }

        let mut playlist_columns_changed = false;
        let mut album_art_column_width_limits_changed = false;
        if let Some(ui_config) = ui_update {
            let has_playlist_columns_patch = ui_config.playlist_columns.is_some();
            let has_layout_patch = ui_config.layout.is_some();
            let has_album_art_min_patch =
                ui_config.playlist_album_art_column_min_width_px.is_some();
            let has_album_art_max_patch =
                ui_config.playlist_album_art_column_max_width_px.is_some();
            if let Some(auto_scroll_to_playing_track) = ui_config.auto_scroll_to_playing_track {
                self.auto_scroll_to_playing_track = auto_scroll_to_playing_track;
            }
            if let Some(playlist_columns) = ui_config.playlist_columns {
                self.playlist_columns = playlist_columns;
            }
            if let Some(album_art_column_min_width_px) =
                ui_config.playlist_album_art_column_min_width_px
            {
                self.album_art_column_min_width_px = album_art_column_min_width_px;
            }
            if let Some(album_art_column_max_width_px) =
                ui_config.playlist_album_art_column_max_width_px
            {
                self.album_art_column_max_width_px = album_art_column_max_width_px;
            }
            let valid_column_keys: HashSet<String> = self
                .playlist_columns
                .iter()
                .map(Self::playlist_column_key)
                .collect();
            if let Some(layout) = ui_config.layout {
                self.apply_layout_column_width_overrides(&layout.playlist_column_width_overrides);
            }
            self.playlist_column_target_widths_px
                .retain(|key, _| valid_column_keys.contains(key));
            let playlist_layout_affected = has_playlist_columns_patch
                || has_layout_patch
                || has_album_art_min_patch
                || has_album_art_max_patch;
            if playlist_layout_affected {
                self.refresh_playlist_column_content_targets();
                self.apply_playlist_column_layout();
                let playlist_columns = self.playlist_columns.clone();

                let visible_headers: Vec<slint::SharedString> = playlist_columns
                    .iter()
                    .filter(|column| column.enabled)
                    .map(|column| column.name.as_str().into())
                    .collect();
                let visible_kinds = Self::visible_playlist_column_kinds(&playlist_columns);
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
                    ui.set_playlist_visible_column_headers(ModelRc::from(Rc::new(VecModel::from(
                        visible_headers,
                    ))));
                    ui.set_playlist_visible_column_kinds(ModelRc::from(Rc::new(VecModel::from(
                        visible_kinds,
                    ))));
                    ui.set_playlist_column_menu_labels(ModelRc::from(Rc::new(VecModel::from(
                        menu_labels,
                    ))));
                    ui.set_playlist_column_menu_checked(ModelRc::from(Rc::new(VecModel::from(
                        menu_checked,
                    ))));
                    ui.set_playlist_column_menu_is_custom(ModelRc::from(Rc::new(VecModel::from(
                        menu_is_custom,
                    ))));
                });
            }
            if has_playlist_columns_patch {
                playlist_columns_changed = previous_playlist_columns != self.playlist_columns;
            }
            if has_album_art_min_patch || has_album_art_max_patch {
                album_art_column_width_limits_changed = previous_album_art_column_min_width_px
                    != self.album_art_column_min_width_px
                    || previous_album_art_column_max_width_px != self.album_art_column_max_width_px;
            }
        }

        let refresh_library_ui = list_image_max_edge_changed
            || online_metadata_enabled_changed
            || online_metadata_prompt_changed;
        let rebuild_playlist_rows = list_image_max_edge_changed || playlist_columns_changed;
        let refresh_display_target =
            refresh_library_ui || playlist_columns_changed || album_art_column_width_limits_changed;
        if refresh_library_ui {
            self.sync_library_ui();
        }
        if rebuild_playlist_rows {
            self.rebuild_track_model();
        }
        if refresh_display_target {
            self.update_display_for_active_collection();
        }
    }

    /// Starts the blocking UI event loop that listens for bus messages.
    pub fn run(&mut self) {
        self.sync_library_ui();
        self.sync_cast_state_to_ui();
        self.refresh_technical_info_ui();
        self.sync_library_add_to_playlist_ui();
        self.sync_library_root_counts_to_ui();
        self.sync_now_playing_favorite_state_to_ui();
        self.request_library_root_counts();
        let _ = self.bus_sender.send(protocol::Message::Library(
            protocol::LibraryMessage::RequestFavoritesSnapshot,
        ));
        let _ = self.bus_sender.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::RequestPlaylistState,
        ));
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
                            protocol::LibraryMessage::ToggleFavoriteForLibraryRow { row_index } => {
                                self.toggle_favorite_for_library_row(row_index);
                            }
                            protocol::LibraryMessage::ToggleFavoriteForPlaylistRow { view_row } => {
                                self.toggle_favorite_for_playlist_row(view_row);
                            }
                            protocol::LibraryMessage::ToggleFavoriteNowPlaying => {
                                self.toggle_favorite_now_playing();
                            }
                            protocol::LibraryMessage::RequestFavoritesSnapshot => {}
                            protocol::LibraryMessage::ToggleFavorite { .. } => {}
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
                            protocol::LibraryMessage::CopySelected => {
                                self.copy_selected_library_items();
                            }
                            protocol::LibraryMessage::CutSelected => {
                                self.cut_selected_library_items();
                            }
                            protocol::LibraryMessage::DeleteSelected => {
                                self.request_library_remove_selection_confirmation();
                            }
                            protocol::LibraryMessage::OpenFileLocation => {
                                self.open_file_location();
                            }
                            protocol::LibraryMessage::ConfirmRemoveSelection => {
                                self.confirm_library_remove_selection();
                            }
                            protocol::LibraryMessage::CancelRemoveSelection => {
                                self.cancel_library_remove_selection();
                            }
                            protocol::LibraryMessage::LibraryViewportChanged {
                                first_row,
                                row_count,
                            } => {
                                trace!(
                                    "Library viewport changed: first_row={}, row_count={}",
                                    first_row,
                                    row_count
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
                                self.refresh_visible_library_cover_art_rows();
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
                            protocol::LibraryMessage::TracksResult(tracks) => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::TracksRoot
                                ) {
                                    self.set_library_entries(
                                        tracks.into_iter().map(LibraryEntry::Track).collect(),
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
                            protocol::LibraryMessage::RootCountsResult {
                                tracks,
                                artists,
                                albums,
                                genres,
                                decades,
                                favorites,
                            } => {
                                self.library_root_counts =
                                    [tracks, artists, albums, genres, decades, favorites];
                                self.sync_library_root_counts_to_ui();
                            }
                            protocol::LibraryMessage::GlobalSearchDataResult {
                                tracks,
                                artists,
                                albums,
                            } => {
                                if matches!(
                                    self.current_library_view(),
                                    LibraryViewState::GlobalSearch
                                ) {
                                    self.set_library_entries(Self::build_global_search_entries(
                                        tracks, artists, albums,
                                    ));
                                }
                            }
                            protocol::LibraryMessage::ArtistDetailResult {
                                artist,
                                albums,
                                tracks,
                            } => {
                                if let LibraryViewState::ArtistDetail {
                                    artist: requested_artist,
                                } = self.current_library_view()
                                {
                                    if requested_artist == artist {
                                        self.set_library_entries(
                                            Self::build_artist_detail_entries(albums, tracks),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::AlbumTracksResult {
                                album,
                                album_artist,
                                tracks,
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
                                            tracks.into_iter().map(LibraryEntry::Track).collect(),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::GenreTracksResult { genre, tracks } => {
                                if let LibraryViewState::GenreDetail {
                                    genre: requested_genre,
                                } = self.current_library_view()
                                {
                                    if requested_genre == genre {
                                        self.set_library_entries(
                                            tracks.into_iter().map(LibraryEntry::Track).collect(),
                                        );
                                    }
                                }
                            }
                            protocol::LibraryMessage::DecadeTracksResult { decade, tracks } => {
                                if let LibraryViewState::DecadeDetail {
                                    decade: requested_decade,
                                } = self.current_library_view()
                                {
                                    if requested_decade == decade {
                                        self.set_library_entries(
                                            tracks.into_iter().map(LibraryEntry::Track).collect(),
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
                                self.update_failed_enrichment_attempt_state(&entity, &payload);
                                if Self::is_retryable_enrichment_result(&payload)
                                    && (is_current_detail_entity || is_visible_prefetch_artist)
                                {
                                    self.schedule_enrichment_retry_backoff(&entity, &payload);
                                } else {
                                    self.clear_enrichment_retry_state(&entity);
                                }
                                if payload.status == protocol::LibraryEnrichmentStatus::NotFound {
                                    self.schedule_enrichment_not_found_backoff(&entity);
                                } else {
                                    self.clear_enrichment_not_found_backoff_state(&entity);
                                }
                                self.library_enrichment.insert(entity.clone(), payload);
                                self.update_display_for_active_collection();

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
                                self.library_enrichment_not_found_not_before.clear();
                                self.library_enrichment_failed_attempt_counts.clear();
                                self.replace_prefetch_queue_if_changed(Vec::new());
                                self.replace_background_queue_if_changed(Vec::new());
                                self.library_last_detail_enrichment_entity = None;
                                TRACK_ROW_COVER_ART_IMAGE_CACHE.with(|cache| {
                                    cache.borrow_mut().clear();
                                });
                                TRACK_ROW_ARTIST_IMAGE_CACHE.with(|cache| {
                                    cache.borrow_mut().clear();
                                });
                                TRACK_ROW_COVER_ART_FAILED_PATHS.with(|failed| {
                                    failed.borrow_mut().clear();
                                });
                                self.pending_list_image_requests.clear();
                                let toast_text = format!(
                                    "Cleared internet metadata cache ({} entries, {} images)",
                                    cleared_rows, deleted_images
                                );
                                self.library_status_text = toast_text.clone();
                                self.show_library_toast(toast_text);
                                self.sync_library_ui();
                            }
                            protocol::LibraryMessage::FavoritesSnapshot { items } => {
                                self.apply_favorites_snapshot(items);
                            }
                            protocol::LibraryMessage::FavoriteStateChanged {
                                entity,
                                favorited,
                            } => {
                                if favorited {
                                    self.favorites_by_key
                                        .insert(entity.entity_key.clone(), entity);
                                } else {
                                    self.favorites_by_key.remove(&entity.entity_key);
                                }
                                self.rebuild_track_model();
                                self.sync_library_ui();
                                self.sync_now_playing_favorite_state_to_ui();
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
                            protocol::LibraryMessage::RemoveSelectionCompleted {
                                removed_tracks,
                            } => {
                                self.pending_library_remove_selections.clear();
                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_show_library_remove_confirm(false);
                                });
                                let toast_text =
                                    format!("Removed {} track(s) from library", removed_tracks);
                                self.library_status_text = toast_text.clone();
                                self.show_library_toast(toast_text);
                                self.library_cover_art_paths.clear();
                                self.folder_cover_art_paths.clear();
                                self.request_library_view_data();
                                self.request_library_root_counts();
                            }
                            protocol::LibraryMessage::RemoveSelectionFailed(error_text) => {
                                self.pending_library_remove_selections.clear();
                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_show_library_remove_confirm(false);
                                });
                                let toast_text =
                                    format!("Failed to remove from library: {}", error_text);
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
                            | protocol::LibraryMessage::RequestRootCounts
                            | protocol::LibraryMessage::RequestTracks
                            | protocol::LibraryMessage::RequestArtists
                            | protocol::LibraryMessage::RequestAlbums
                            | protocol::LibraryMessage::RequestGenres
                            | protocol::LibraryMessage::RequestDecades
                            | protocol::LibraryMessage::RequestGlobalSearchData
                            | protocol::LibraryMessage::RequestArtistDetail { .. }
                            | protocol::LibraryMessage::RequestAlbumTracks { .. }
                            | protocol::LibraryMessage::RequestGenreTracks { .. }
                            | protocol::LibraryMessage::RequestDecadeTracks { .. }
                            | protocol::LibraryMessage::RequestLibraryPage { .. }
                            | protocol::LibraryMessage::RequestEnrichment { .. }
                            | protocol::LibraryMessage::ReplaceEnrichmentPrefetchQueue { .. }
                            | protocol::LibraryMessage::ReplaceEnrichmentBackgroundQueue {
                                ..
                            }
                            | protocol::LibraryMessage::ClearEnrichmentCache
                            | protocol::LibraryMessage::AddSelectionToPlaylists { .. }
                            | protocol::LibraryMessage::PasteSelectionToActivePlaylist { .. }
                            | protocol::LibraryMessage::RemoveSelectionFromLibrary { .. } => {}
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
                            protocol::PlaylistMessage::OpenSubsonicSyncEligiblePlaylists(
                                playlist_ids,
                            ),
                        ) => {
                            self.opensubsonic_sync_eligible_playlist_ids =
                                playlist_ids.into_iter().collect();
                            let sync_flags = self
                                .playlist_ids
                                .iter()
                                .map(|playlist_id| {
                                    self.opensubsonic_sync_eligible_playlist_ids
                                        .contains(playlist_id)
                                })
                                .collect::<Vec<_>>();
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_playlist_can_sync_opensubsonic(ModelRc::from(Rc::new(
                                    VecModel::from(sync_flags),
                                )));
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistsRestored(playlists),
                        ) => {
                            let old_playlist_ids = self.playlist_ids.clone();
                            let old_len = self.playlist_ids.len();
                            self.playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
                            self.playlist_names =
                                playlists.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
                            let remote_playlist_flags = self
                                .playlist_ids
                                .iter()
                                .map(|playlist_id| Self::is_pure_remote_playlist_id(playlist_id))
                                .collect::<Vec<_>>();
                            let sync_flags = self
                                .playlist_ids
                                .iter()
                                .map(|playlist_id| {
                                    self.opensubsonic_sync_eligible_playlist_ids
                                        .contains(playlist_id)
                                })
                                .collect::<Vec<_>>();
                            self.library_add_to_playlist_checked =
                                vec![false; self.playlist_ids.len()];
                            self.sync_library_add_to_playlist_ui();
                            let new_len = self.playlist_ids.len();
                            let mut slint_playlists = Vec::new();
                            for p in playlists {
                                slint_playlists.push(StandardListViewItem::from(p.name.as_str()));
                            }
                            let new_playlist_edit_index = if new_len == old_len + 1 && old_len > 0 {
                                self.playlist_ids
                                    .iter()
                                    .position(|playlist_id| !old_playlist_ids.contains(playlist_id))
                                    .filter(|index| {
                                        !remote_playlist_flags.get(*index).copied().unwrap_or(false)
                                    })
                                    .map(|index| index as i32)
                                    .unwrap_or(-1)
                            } else {
                                -1
                            };

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_playlists(ModelRc::from(Rc::new(VecModel::from(
                                    slint_playlists,
                                ))));
                                ui.set_playlist_is_remote(ModelRc::from(Rc::new(VecModel::from(
                                    remote_playlist_flags,
                                ))));
                                ui.set_playlist_can_sync_opensubsonic(ModelRc::from(Rc::new(
                                    VecModel::from(sync_flags),
                                )));
                                if new_playlist_edit_index >= 0 {
                                    ui.set_editing_playlist_index(new_playlist_edit_index);
                                    ui.set_new_playlist_edit_index(new_playlist_edit_index);
                                } else {
                                    ui.set_new_playlist_edit_index(-1);
                                }
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistChanged(id),
                        ) => {
                            self.active_playlist_id = id.clone();
                            self.selection_anchor_track_id = None;
                            self.playlist_column_target_widths_px.clear();
                            self.apply_playlist_column_layout();
                            if let Some(index) =
                                self.playlist_ids.iter().position(|p_id| p_id == &id)
                            {
                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_active_playlist_index(index as i32);
                                    ui.set_new_playlist_edit_index(-1);
                                });
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistRestored(tracks),
                        ) => {
                            // Switching playlists should always start in the playlist's natural order
                            // with no active read-only filter/search view state.
                            self.reset_filter_state();
                            self.close_library_search();
                            Self::reset_playlist_cover_art_state(
                                &mut self.track_cover_art_missing_tracks,
                                &mut self.folder_cover_art_paths,
                            );
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
                            if self.collection_mode == COLLECTION_MODE_LIBRARY {
                                self.request_library_remove_selection_confirmation();
                                continue;
                            }
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
                                self.refresh_playing_track_metadata();
                                self.update_display_for_active_collection();
                                self.rebuild_track_model();
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::TrackUnavailable { id, reason },
                        ) => {
                            let newly_unavailable = self.unavailable_track_ids.insert(id.clone());
                            if newly_unavailable {
                                info!(
                                    "UiManager: track marked unavailable id={} reason={}",
                                    id, reason
                                );
                            }
                            if let Some(index) =
                                self.track_ids.iter().position(|track_id| track_id == &id)
                            {
                                if let Some(metadata) = self.track_metadata.get_mut(index) {
                                    metadata.title = REMOTE_TRACK_UNAVAILABLE_TITLE.to_string();
                                    metadata.artist.clear();
                                    metadata.album.clear();
                                    metadata.album_artist.clear();
                                    metadata.date.clear();
                                    metadata.genre.clear();
                                    metadata.year.clear();
                                    metadata.track_number.clear();
                                }
                            }
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            self.refresh_playing_track_metadata();
                            self.update_display_for_active_collection();
                            self.rebuild_track_model();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlayTrackByViewIndex(view_index),
                        ) => {
                            let unavailable = self
                                .map_view_to_source_index(view_index)
                                .and_then(|source_index| self.track_ids.get(source_index))
                                .map(|track_id| self.unavailable_track_ids.contains(track_id))
                                .unwrap_or(false);
                            if unavailable {
                                continue;
                            }
                            self.start_queue_if_possible(
                                self.build_playlist_queue_request(Some(view_index)),
                            );
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::SelectAll) => {
                            self.handle_select_all();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ArrowKeyNavigate { direction, shift },
                        ) => {
                            self.handle_arrow_key_navigate(direction, shift);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::PageNavigate {
                            action,
                            shift,
                            visible_row_count,
                        }) => {
                            self.handle_page_navigate(action, shift, visible_row_count);
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
                            protocol::PlaybackMessage::PlayActiveCollection,
                        ) => {
                            self.play_active_collection();
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
                        protocol::Message::Cast(protocol::CastMessage::DevicesUpdated(devices)) => {
                            self.cast_device_ids =
                                devices.iter().map(|device| device.id.clone()).collect();
                            self.cast_device_names = devices
                                .iter()
                                .map(|device| {
                                    if device.model.trim().is_empty() {
                                        device.name.clone()
                                    } else {
                                        format!("{} ({})", device.name, device.model)
                                    }
                                })
                                .collect();
                            self.sync_cast_state_to_ui();
                        }
                        protocol::Message::Cast(
                            protocol::CastMessage::ConnectionStateChanged {
                                state,
                                device,
                                reason,
                            },
                        ) => {
                            self.cast_connected = state == protocol::CastConnectionState::Connected;
                            self.cast_connecting =
                                state == protocol::CastConnectionState::Connecting;
                            self.cast_discovering =
                                state == protocol::CastConnectionState::Discovering;
                            self.cast_device_name =
                                device.map(|device| device.name).unwrap_or_default();
                            if !self.cast_connected {
                                self.cast_playback_path_kind = None;
                                self.cast_transcode_output_metadata = None;
                            }
                            if let Some(reason) = reason {
                                let trimmed = reason.trim();
                                if !trimmed.is_empty() {
                                    self.library_status_text = trimmed.to_string();
                                    self.show_library_toast(trimmed);
                                }
                            }
                            self.sync_cast_state_to_ui();
                            self.refresh_technical_info_ui();
                        }
                        protocol::Message::Cast(protocol::CastMessage::PlaybackPathChanged {
                            kind,
                            description: _description,
                            transcode_output_metadata,
                        }) => {
                            self.cast_playback_path_kind = Some(kind);
                            self.cast_transcode_output_metadata = transcode_output_metadata;
                            self.refresh_technical_info_ui();
                        }
                        protocol::Message::Cast(protocol::CastMessage::PlaybackError {
                            track_id,
                            message,
                            can_retry_with_transcode,
                        }) => {
                            let mut trimmed = message.trim().to_string();
                            if let Some(track_id) = track_id {
                                if !track_id.trim().is_empty() {
                                    trimmed = format!("{} [track: {}]", trimmed, track_id);
                                }
                            }
                            if can_retry_with_transcode {
                                trimmed = format!(
                                    "{} (direct cast failed; transcode fallback available)",
                                    trimmed
                                );
                            }
                            if !trimmed.is_empty() {
                                self.library_status_text = trimmed.clone();
                                self.show_library_toast(trimmed);
                            }
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                            self.playback_active = false;
                            self.active_playing_index = None;
                            self.last_progress_at = None;
                            let had_playing_track = self.playing_track.path.is_some();
                            self.playing_track = PlayingTrackState::default();
                            self.current_technical_metadata = None;
                            self.current_output_path_info = None;
                            self.library_playing_index = None;
                            if had_playing_track {
                                self.display_target_priority = DisplayTargetPriority::Playing;
                            }
                            self.update_display_for_active_collection();

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
                            self.sync_playlist_playback_state_to_ui();
                            if had_playing_track {
                                self.sync_library_playing_state_to_ui();
                            }
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted {
                            index: _index,
                            playlist_id,
                            ..
                        }) => {
                            self.playback_active = true;
                            if playlist_id != self.active_playlist_id {
                                self.active_playing_index = None;
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistIndicesChanged {
                                playing_playlist_id,
                                playing_track_id,
                                playing_track_path,
                                playing_track_metadata,
                                selected_indices,
                                is_playing,
                                playback_order,
                                repeat_mode,
                                ..
                            },
                        ) => {
                            self.playback_active = is_playing;
                            if !is_playing {
                                self.last_progress_at = None;
                            }
                            let previous_active_playing_index = self.active_playing_index;
                            let selected_indices_clone = selected_indices.clone();
                            self.selected_indices = selected_indices_clone.clone();
                            let is_playing_active_playlist =
                                playing_playlist_id.as_ref() == Some(&self.active_playlist_id);
                            self.active_playing_index = Self::resolve_active_playing_source_index(
                                &self.track_ids,
                                &self.track_paths,
                                is_playing_active_playlist,
                                playing_track_id.as_deref(),
                                playing_track_path.as_ref(),
                            );

                            let playing_track_changed = self.set_playing_track(
                                playing_track_id.clone(),
                                playing_track_path.clone(),
                                playing_track_metadata.clone(),
                            );
                            if playing_track_changed {
                                self.display_target_priority = DisplayTargetPriority::Playing;
                            }
                            self.update_library_playing_index();
                            self.update_display_for_active_collection();
                            if playing_track_changed {
                                self.sync_library_ui();
                            }

                            let status_text = self
                                .playing_track
                                .metadata
                                .as_ref()
                                .map(Self::status_text_from_detailed_metadata);

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if let Some(text) = status_text {
                                    ui.set_status_text(text.into());
                                }
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
                            if playing_track_changed
                                || previous_active_playing_index != self.active_playing_index
                            {
                                self.auto_scroll_active_collection_to_playing_track();
                            }
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
                            let next_anchor_source_index = Self::resolve_shift_anchor_source_index(
                                &self.selected_indices,
                                self.selection_anchor_source_index(),
                            );
                            if let Some(anchor_source_index) = next_anchor_source_index {
                                self.set_selection_anchor_from_source_index(anchor_source_index);
                            } else {
                                self.selection_anchor_track_id = None;
                            }
                            self.display_target_priority = DisplayTargetPriority::Selection;
                            debug!(
                                "After SelectionChanged: self.selected_indices = {:?}",
                                self.selected_indices
                            );

                            // Selection interaction is the most recent user change, so it
                            // temporarily owns metadata/cover-art display until playback changes.
                            self.update_display_for_active_collection();
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
                            drag_blocked,
                        }) => {
                            self.on_drag_end(drop_gap, drag_blocked);
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
                            if path.is_some() {
                                self.refresh_visible_library_cover_art_rows();
                                if self.is_album_art_column_visible() {
                                    self.refresh_visible_playlist_cover_art_rows();
                                }
                            }
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if let Some(path) = path {
                                    if let Some(img) =
                                        UiManager::try_load_detail_cover_art_image_with_kind(
                                            &path,
                                            protocol::UiImageKind::CoverArt,
                                            DETAIL_COMPACT_RENDER_MAX_EDGE_PX,
                                            DETAIL_COMPACT_RENDER_MAX_EDGE_PX,
                                        )
                                    {
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
                            protocol::PlaybackMessage::ListImageReady {
                                source_path,
                                kind,
                                variant,
                            },
                        ) => {
                            self.pending_list_image_requests
                                .remove(&(source_path, kind, variant));
                            if variant == protocol::UiImageVariant::ListThumb {
                                match kind {
                                    protocol::UiImageKind::CoverArt => {
                                        self.refresh_visible_playlist_cover_art_rows();
                                        self.refresh_visible_library_cover_art_rows();
                                    }
                                    protocol::UiImageKind::ArtistImage => {
                                        self.refresh_visible_library_cover_art_rows();
                                    }
                                }
                            }
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
                            changes,
                        )) => {
                            let mut ui_update = protocol::UiConfigDelta::default();
                            let mut library_update = protocol::LibraryConfigDelta::default();
                            for change in changes {
                                match change {
                                    protocol::ConfigDeltaEntry::Ui(ui) => {
                                        ui_update.merge_from(ui);
                                    }
                                    protocol::ConfigDeltaEntry::Library(library) => {
                                        library_update.merge_from(library);
                                    }
                                    protocol::ConfigDeltaEntry::Output(_)
                                    | protocol::ConfigDeltaEntry::Cast(_)
                                    | protocol::ConfigDeltaEntry::Buffering(_)
                                    | protocol::ConfigDeltaEntry::Integrations(_) => {}
                                }
                            }
                            self.apply_ui_library_config_updates(
                                (!ui_update.is_empty()).then_some(ui_update),
                                (!library_update.is_empty()).then_some(library_update),
                            );
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistViewportChanged {
                                first_row,
                                row_count,
                            },
                        ) => {
                            self.prefetch_playlist_cover_art_window(first_row, row_count);
                            self.refresh_visible_playlist_cover_art_rows();
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
                            protocol::PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                                column_key,
                                width_px,
                            },
                        ) => {
                            if column_key.trim().is_empty() {
                                continue;
                            }
                            if !self
                                .playlist_columns
                                .iter()
                                .any(|column| Self::playlist_column_key(column) == column_key)
                            {
                                continue;
                            }
                            let clamped_width_px =
                                self.clamp_column_override_width_px(&column_key, width_px);
                            if self
                                .playlist_column_width_overrides_px
                                .get(&column_key)
                                .copied()
                                == Some(clamped_width_px)
                            {
                                continue;
                            }
                            self.playlist_column_width_overrides_px
                                .insert(column_key, clamped_width_px);
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
        fit_column_widths_deterministic, ColumnWidthProfile, CoverArtLookupRequest,
        DeterministicColumnLayoutSpec, LibraryEntry, LibraryViewState, PlaylistColumnClass,
        PlaylistSortDirection, TrackMetadata, UiManager, ENRICHMENT_FAILED_ATTEMPT_CAP,
    };
    use crate::{config::PlaylistColumnConfig, protocol};
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

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

    fn make_library_track(id: &str, title: &str, path: &str) -> protocol::LibraryTrack {
        protocol::LibraryTrack {
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

    fn make_library_track_in_album(
        id: &str,
        title: &str,
        path: &str,
        album: &str,
        album_artist: &str,
        year: &str,
        track_number: &str,
    ) -> protocol::LibraryTrack {
        protocol::LibraryTrack {
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
            track_count: 3,
            representative_track_path: Some(PathBuf::from(format!("{album}.mp3"))),
        }
    }

    fn make_library_artist(
        name: &str,
        album_count: u32,
        track_count: u32,
    ) -> protocol::LibraryArtist {
        protocol::LibraryArtist {
            artist: name.to_string(),
            album_count,
            track_count,
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
    fn test_reset_playlist_cover_art_state_clears_missing_and_negative_folder_cache() {
        let mut missing_tracks = HashSet::from([
            PathBuf::from("/music/a/track1.mp3"),
            PathBuf::from("/music/b/track2.mp3"),
        ]);
        let mut folder_cover_cache = HashMap::from([
            (PathBuf::from("/music/a"), None),
            (
                PathBuf::from("/music/b"),
                Some(PathBuf::from("/music/b/cover.jpg")),
            ),
        ]);

        UiManager::reset_playlist_cover_art_state(&mut missing_tracks, &mut folder_cover_cache);

        assert!(missing_tracks.is_empty());
        assert_eq!(folder_cover_cache.len(), 1);
        assert_eq!(
            folder_cover_cache.get(&PathBuf::from("/music/b")),
            Some(&Some(PathBuf::from("/music/b/cover.jpg")))
        );
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
    fn test_build_playlist_queue_snapshot_uses_rendered_view_order() {
        let track_ids = vec![
            "t0".to_string(),
            "t1".to_string(),
            "t2".to_string(),
            "t3".to_string(),
        ];
        let track_paths = vec![
            PathBuf::from("a.mp3"),
            PathBuf::from("b.mp3"),
            PathBuf::from("c.mp3"),
            PathBuf::from("d.mp3"),
        ];
        let view_indices = vec![2usize, 0, 3];
        let selected_indices = vec![0usize];

        let (tracks, start_index) = UiManager::build_playlist_queue_snapshot(
            &track_ids,
            &track_paths,
            &view_indices,
            &selected_indices,
            None,
        )
        .expect("queue snapshot should exist");

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, "t2");
        assert_eq!(tracks[1].id, "t0");
        assert_eq!(tracks[2].id, "t3");
        assert_eq!(start_index, 1);
    }

    #[test]
    fn test_build_playlist_queue_snapshot_preferred_source_index_overrides_selection() {
        let track_ids = vec!["t0".to_string(), "t1".to_string(), "t2".to_string()];
        let track_paths = vec![
            PathBuf::from("a.mp3"),
            PathBuf::from("b.mp3"),
            PathBuf::from("c.mp3"),
        ];
        let view_indices = vec![2usize, 0, 1];
        let selected_indices = vec![0usize, 1];

        let (_, start_index) = UiManager::build_playlist_queue_snapshot(
            &track_ids,
            &track_paths,
            &view_indices,
            &selected_indices,
            Some(2),
        )
        .expect("queue snapshot should exist");

        assert_eq!(start_index, 0);
    }

    /// `playing_index` is a playback-playlist index (view-ordered queue), NOT a
    /// source index into the editing playlist.  The resolver must always use
    /// The resolver uses the stable track id to find the correct source index,
    /// even when a sorted/filtered view has reordered the playback queue.
    #[test]
    fn test_resolve_active_playing_source_index_uses_track_id() {
        let track_ids = vec![
            "id-a1".to_string(),
            "id-b".to_string(),
            "id-a2".to_string(),
            "id-c".to_string(),
        ];
        let track_paths = vec![
            PathBuf::from("a.mp3"),
            PathBuf::from("b.mp3"),
            PathBuf::from("a.mp3"),
            PathBuf::from("c.mp3"),
        ];
        // Two entries share the path "a.mp3" but have distinct track ids.
        // The resolver must find source index 2 (id-a2), not 0 (id-a1).
        let playing_path = PathBuf::from("a.mp3");
        let resolved = UiManager::resolve_active_playing_source_index(
            &track_ids,
            &track_paths,
            true,
            Some("id-a2"),
            Some(&playing_path),
        );
        assert_eq!(resolved, Some(2));
    }

    /// When track id is unavailable (None), the resolver falls back to the
    /// first path match.  This is a best-effort fallback for robustness.
    #[test]
    fn test_resolve_active_playing_source_index_falls_back_to_path_when_id_missing() {
        let track_ids = vec!["id-a".to_string(), "id-b".to_string()];
        let track_paths = vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")];
        let playing_path = PathBuf::from("b.mp3");
        let resolved = UiManager::resolve_active_playing_source_index(
            &track_ids,
            &track_paths,
            true,
            None,
            Some(&playing_path),
        );
        assert_eq!(resolved, Some(1));
    }

    /// When not playing the active playlist, the resolver returns None.
    #[test]
    fn test_resolve_active_playing_source_index_returns_none_for_different_playlist() {
        let track_ids = vec!["id-a".to_string()];
        let track_paths = vec![PathBuf::from("a.mp3")];
        let resolved = UiManager::resolve_active_playing_source_index(
            &track_ids,
            &track_paths,
            false,
            Some("id-a"),
            Some(&PathBuf::from("a.mp3")),
        );
        assert_eq!(resolved, None);
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
    fn test_resolve_shift_anchor_source_index_keeps_current_anchor_when_selected() {
        let selected = vec![4usize, 7, 9];
        let anchor = UiManager::resolve_shift_anchor_source_index(&selected, Some(7));
        assert_eq!(anchor, Some(7));
    }

    #[test]
    fn test_resolve_shift_anchor_source_index_falls_back_when_anchor_not_selected() {
        let selected = vec![2usize, 5, 6];
        let anchor = UiManager::resolve_shift_anchor_source_index(&selected, Some(1));
        assert_eq!(anchor, Some(6));
    }

    #[test]
    fn test_resolve_shift_anchor_source_index_none_when_no_selection() {
        let selected: Vec<usize> = Vec::new();
        let anchor = UiManager::resolve_shift_anchor_source_index(&selected, Some(4));
        assert_eq!(anchor, None);
    }

    #[test]
    fn test_is_counted_failed_enrichment_result_counts_not_found() {
        let payload = protocol::LibraryEnrichmentPayload {
            entity: protocol::LibraryEnrichmentEntity::Artist {
                artist: "Example Artist".to_string(),
            },
            status: protocol::LibraryEnrichmentStatus::NotFound,
            blurb: String::new(),
            image_path: None,
            source_name: "Wikipedia".to_string(),
            source_url: String::new(),
            error_kind: None,
            attempt_kind: protocol::LibraryEnrichmentAttemptKind::Detail,
        };
        assert!(UiManager::is_counted_failed_enrichment_result(&payload));
    }

    #[test]
    fn test_is_counted_failed_enrichment_result_ignores_rate_limited_error() {
        let payload = protocol::LibraryEnrichmentPayload {
            entity: protocol::LibraryEnrichmentEntity::Artist {
                artist: "Example Artist".to_string(),
            },
            status: protocol::LibraryEnrichmentStatus::Error,
            blurb: String::new(),
            image_path: None,
            source_name: "TheAudioDB".to_string(),
            source_url: String::new(),
            error_kind: Some(protocol::LibraryEnrichmentErrorKind::RateLimited),
            attempt_kind: protocol::LibraryEnrichmentAttemptKind::VisiblePrefetch,
        };
        assert!(!UiManager::is_counted_failed_enrichment_result(&payload));
    }

    #[test]
    fn test_is_counted_failed_enrichment_result_counts_non_rate_limited_error() {
        let payload = protocol::LibraryEnrichmentPayload {
            entity: protocol::LibraryEnrichmentEntity::Artist {
                artist: "Example Artist".to_string(),
            },
            status: protocol::LibraryEnrichmentStatus::Error,
            blurb: String::new(),
            image_path: None,
            source_name: "Wikipedia".to_string(),
            source_url: String::new(),
            error_kind: Some(protocol::LibraryEnrichmentErrorKind::Timeout),
            attempt_kind: protocol::LibraryEnrichmentAttemptKind::VisiblePrefetch,
        };
        assert!(UiManager::is_counted_failed_enrichment_result(&payload));
    }

    #[test]
    fn test_failed_enrichment_attempt_cap_reached_uses_threshold() {
        assert!(!UiManager::failed_enrichment_attempt_cap_reached(
            ENRICHMENT_FAILED_ATTEMPT_CAP.saturating_sub(1),
        ));
        assert!(UiManager::failed_enrichment_attempt_cap_reached(
            ENRICHMENT_FAILED_ATTEMPT_CAP,
        ));
    }

    #[test]
    fn test_enrichment_backoff_elapsed_true_for_missing_or_past_deadline() {
        assert!(UiManager::enrichment_backoff_elapsed(None));
        let past = Instant::now() - Duration::from_secs(1);
        assert!(UiManager::enrichment_backoff_elapsed(Some(&past)));
    }

    #[test]
    fn test_enrichment_backoff_elapsed_false_for_future_deadline() {
        let future = Instant::now() + Duration::from_secs(10);
        assert!(!UiManager::enrichment_backoff_elapsed(Some(&future)));
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
            false,
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("selected metadata should exist");
        assert_eq!(meta.title, "B");
    }

    #[test]
    fn test_resolve_display_target_prefers_playing_when_requested() {
        let selected = vec![1usize];
        let paths = vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")];
        let metadata = vec![make_meta("A"), make_meta("B")];
        let playing_path = Some(PathBuf::from("a.mp3"));

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            None,
            true,
        );

        assert_eq!(path, Some(PathBuf::from("a.mp3")));
        let meta = meta.expect("playing metadata should exist");
        assert_eq!(meta.title, "A");
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
            false,
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
            false,
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
            UiManager::resolve_display_target(&selected, &paths, &metadata, None, None, false);

        assert!(path.is_none());
        assert!(meta.is_none());
    }

    #[test]
    fn test_resolve_library_display_target_prefers_selected_track() {
        let selected = vec![1usize];
        let entries = vec![
            LibraryEntry::Track(make_library_track("track-a", "A", "a.mp3")),
            LibraryEntry::Track(make_library_track("track-b", "B", "b.mp3")),
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
            &[],
            &[],
            playing_path.as_ref(),
            Some(&playing_meta),
            false,
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("selected library track metadata should exist");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist, "B-artist");
    }

    #[test]
    fn test_resolve_library_display_target_prefers_playing_when_requested() {
        let selected = vec![1usize];
        let entries = vec![
            LibraryEntry::Track(make_library_track("track-a", "A", "a.mp3")),
            LibraryEntry::Track(make_library_track("track-b", "B", "b.mp3")),
        ];
        let playing_path = Some(PathBuf::from("a.mp3"));

        let (path, meta) = UiManager::resolve_library_display_target(
            &selected,
            &entries,
            &[],
            &[],
            playing_path.as_ref(),
            None,
            true,
        );

        assert_eq!(path, Some(PathBuf::from("a.mp3")));
        let meta = meta.expect("playing library metadata should exist");
        assert_eq!(meta.title, "A");
        assert_eq!(meta.artist, "A-artist");
    }

    #[test]
    fn test_resolve_library_display_target_falls_back_when_selection_has_no_track() {
        let selected = vec![0usize];
        let entries = vec![LibraryEntry::Artist(protocol::LibraryArtist {
            artist: "Artist".to_string(),
            album_count: 2,
            track_count: 10,
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
            &[],
            &[],
            playing_path.as_ref(),
            Some(&playing_meta),
            false,
        );

        assert_eq!(path, Some(PathBuf::from("playing.mp3")));
        let meta = meta.expect("playing metadata should be preserved");
        assert_eq!(meta.title, "Playing");
    }

    #[test]
    fn test_resolve_library_display_target_uses_playlist_metadata_when_detail_view_has_no_track_rows(
    ) {
        let selected = vec![];
        let entries = vec![LibraryEntry::Artist(protocol::LibraryArtist {
            artist: "Artist".to_string(),
            album_count: 1,
            track_count: 8,
        })];
        let playlist_paths = vec![PathBuf::from("playing.mp3")];
        let playlist_metadata = vec![make_meta("Now Playing")];
        let playing_path = Some(PathBuf::from("playing.mp3"));

        let (path, meta) = UiManager::resolve_library_display_target(
            &selected,
            &entries,
            &playlist_paths,
            &playlist_metadata,
            playing_path.as_ref(),
            None,
            false,
        );

        assert_eq!(path, Some(PathBuf::from("playing.mp3")));
        let meta = meta.expect("playlist metadata should be used for playing fallback");
        assert_eq!(meta.title, "Now Playing");
        assert_eq!(meta.artist, "Now Playing-artist");
    }

    #[test]
    fn test_resolve_library_display_target_falls_back_to_path_metadata_when_missing_everywhere() {
        let selected = vec![];
        let entries = vec![LibraryEntry::Artist(protocol::LibraryArtist {
            artist: "Artist".to_string(),
            album_count: 1,
            track_count: 8,
        })];
        let playing_path = Some(PathBuf::from("/tmp/Example Track.flac"));

        let (path, meta) = UiManager::resolve_library_display_target(
            &selected,
            &entries,
            &[],
            &[],
            playing_path.as_ref(),
            None,
            false,
        );

        assert_eq!(path, Some(PathBuf::from("/tmp/Example Track.flac")));
        let meta = meta.expect("fallback metadata should be synthesized from path");
        assert_eq!(meta.title, "Example Track");
        assert!(meta.artist.is_empty());
    }

    #[test]
    fn test_resolve_metadata_for_track_path_from_sources_uses_library_when_playlist_missing() {
        let path = PathBuf::from("library-only.mp3");
        let metadata = UiManager::resolve_metadata_for_track_path_from_sources(
            &path,
            &[],
            &[],
            &[LibraryEntry::Track(make_library_track(
                "track-library",
                "Library Track",
                "library-only.mp3",
            ))],
        )
        .expect("library metadata should resolve");
        assert_eq!(metadata.title, "Library Track");
        assert_eq!(metadata.artist, "Library Track-artist");
    }

    #[test]
    fn test_resolve_metadata_for_track_path_from_sources_prefers_playlist_cache_when_available() {
        let path = PathBuf::from("shared.mp3");
        let metadata = UiManager::resolve_metadata_for_track_path_from_sources(
            &path,
            &[PathBuf::from("shared.mp3")],
            &[make_meta("Playlist Track")],
            &[LibraryEntry::Track(make_library_track(
                "track-library",
                "Library Track",
                "shared.mp3",
            ))],
        )
        .expect("playlist metadata should resolve");
        assert_eq!(metadata.title, "Playlist Track");
        assert_eq!(metadata.artist, "Playlist Track-artist");
    }

    #[test]
    fn test_build_artist_detail_entries_groups_tracks_by_album_and_sorts_year_desc() {
        let albums = vec![
            make_library_album("Alpha", "Artist"),
            make_library_album("No Year", "Artist"),
            make_library_album("Zeta", "Artist"),
        ];
        let tracks = vec![
            make_library_track_in_album(
                "alpha-1",
                "Alpha Track 1",
                "alpha-1.mp3",
                "Alpha",
                "Artist",
                "1998",
                "1",
            ),
            make_library_track_in_album(
                "alpha-2",
                "Alpha Track 2",
                "alpha-2.mp3",
                "Alpha",
                "Artist",
                "1998",
                "2",
            ),
            make_library_track_in_album(
                "noyear-1",
                "No Year Track 1",
                "noyear-1.mp3",
                "No Year",
                "Artist",
                "",
                "1",
            ),
            make_library_track_in_album(
                "zeta-1",
                "Zeta Track 1",
                "zeta-1.mp3",
                "Zeta",
                "Artist",
                "2024",
                "1",
            ),
            make_library_track_in_album(
                "zeta-2",
                "Zeta Track 2",
                "zeta-2.mp3",
                "Zeta",
                "Artist",
                "2024",
                "2",
            ),
        ];

        let entries = UiManager::build_artist_detail_entries(albums, tracks);
        let summary: Vec<String> = entries
            .iter()
            .map(|entry| match entry {
                LibraryEntry::Album(album) => {
                    format!("album:{}\u{001f}{}", album.album, album.album_artist)
                }
                LibraryEntry::Track(track) => {
                    format!("track:{}\u{001f}{}", track.album, track.title)
                }
                LibraryEntry::Artist(_)
                | LibraryEntry::Genre(_)
                | LibraryEntry::Decade(_)
                | LibraryEntry::FavoriteCategory(_) => "unexpected".to_string(),
            })
            .collect();

        assert_eq!(
            summary,
            vec![
                "album:Zeta\u{001f}Artist",
                "track:Zeta\u{001f}Zeta Track 1",
                "track:Zeta\u{001f}Zeta Track 2",
                "album:Alpha\u{001f}Artist",
                "track:Alpha\u{001f}Alpha Track 1",
                "track:Alpha\u{001f}Alpha Track 2",
                "album:No Year\u{001f}Artist",
                "track:No Year\u{001f}No Year Track 1",
            ]
        );
    }

    #[test]
    fn test_build_artist_detail_entries_creates_album_header_for_track_only_album() {
        let albums = vec![];
        let tracks = vec![make_library_track_in_album(
            "beta-1",
            "Beta Track 1",
            "beta-1.mp3",
            "Beta",
            "Artist",
            "2019",
            "1",
        )];

        let entries = UiManager::build_artist_detail_entries(albums, tracks);
        assert_eq!(entries.len(), 2);

        match &entries[0] {
            LibraryEntry::Album(album) => {
                assert_eq!(album.album, "Beta");
                assert_eq!(album.album_artist, "Artist");
            }
            other => panic!("expected synthetic album header, got {:?}", other),
        }
        match &entries[1] {
            LibraryEntry::Track(track) => {
                assert_eq!(track.album, "Beta");
                assert_eq!(track.title, "Beta Track 1");
            }
            other => panic!(
                "expected track row under synthetic album header, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_build_global_search_entries_orders_by_name_then_kind() {
        let tracks = vec![
            make_library_track("track-alpha", "Alpha", "alpha.mp3"),
            make_library_track("track-beta", "Beta", "beta.mp3"),
        ];
        let artists = vec![make_library_artist("Alpha", 3, 25)];
        let albums = vec![make_library_album("Alpha", "Artist One")];

        let entries = UiManager::build_global_search_entries(tracks, artists, albums);
        let summary: Vec<String> = entries
            .into_iter()
            .map(|entry| match entry {
                LibraryEntry::Track(track) => format!("track:{}", track.title),
                LibraryEntry::Artist(artist) => format!("artist:{}", artist.artist),
                LibraryEntry::Album(album) => format!("album:{}", album.album),
                LibraryEntry::Genre(_)
                | LibraryEntry::Decade(_)
                | LibraryEntry::FavoriteCategory(_) => "unexpected".to_string(),
            })
            .collect();

        assert_eq!(
            summary,
            vec!["artist:Alpha", "album:Alpha", "track:Alpha", "track:Beta"]
        );
    }

    #[test]
    fn test_library_view_labels_for_global_search() {
        let (title, subtitle) = UiManager::library_view_labels(&LibraryViewState::GlobalSearch);
        assert_eq!(title, "Global Search");
        assert_eq!(subtitle, "");
    }

    #[test]
    fn test_build_library_view_indices_for_query_matches_current_view_entries() {
        let entries = vec![
            LibraryEntry::Track(make_library_track("track-a", "Midnight Train", "train.mp3")),
            LibraryEntry::Artist(protocol::LibraryArtist {
                artist: "Daft Punk".to_string(),
                album_count: 4,
                track_count: 30,
            }),
            LibraryEntry::Album(make_library_album("Discovery", "Daft Punk")),
            LibraryEntry::Genre(protocol::LibraryGenre {
                genre: "Electronic".to_string(),
                track_count: 12,
            }),
            LibraryEntry::Decade(protocol::LibraryDecade {
                decade: "2000s".to_string(),
                track_count: 99,
            }),
        ];

        let track_match = UiManager::build_library_view_indices_for_query(&entries, "train");
        assert_eq!(track_match, vec![0]);

        let artist_match = UiManager::build_library_view_indices_for_query(&entries, "daft");
        assert_eq!(artist_match, vec![1, 2]);

        let decade_match = UiManager::build_library_view_indices_for_query(&entries, "2000");
        assert_eq!(decade_match, vec![4]);
    }

    #[test]
    fn test_build_library_view_indices_for_query_returns_all_when_query_empty() {
        let entries = vec![
            LibraryEntry::Track(make_library_track("track-a", "A", "a.mp3")),
            LibraryEntry::Artist(protocol::LibraryArtist {
                artist: "Artist".to_string(),
                album_count: 1,
                track_count: 1,
            }),
            LibraryEntry::Album(make_library_album("Album", "Artist")),
        ];

        let indices = UiManager::build_library_view_indices_for_query(&entries, "   ");
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn test_build_library_selection_specs_for_entries_expands_supported_item_types() {
        let entries = vec![
            LibraryEntry::Track(make_library_track("track-a", "A", "a.mp3")),
            LibraryEntry::Artist(protocol::LibraryArtist {
                artist: "Artist A".to_string(),
                album_count: 2,
                track_count: 20,
            }),
            LibraryEntry::Album(make_library_album("Album A", "Artist A")),
            LibraryEntry::Genre(protocol::LibraryGenre {
                genre: "Rock".to_string(),
                track_count: 15,
            }),
            LibraryEntry::Decade(protocol::LibraryDecade {
                decade: "1990s".to_string(),
                track_count: 40,
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
            protocol::LibrarySelectionSpec::Track { path } => {
                assert_eq!(path, &PathBuf::from("a.mp3"));
            }
            other => panic!("expected track spec, got {:?}", other),
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
            LibraryEntry::Track(make_library_track("track-a", "A", "a.mp3")),
            LibraryEntry::Track(make_library_track("track-b", "B", "b.mp3")),
            LibraryEntry::Album(make_library_album("Album A", "Artist A")),
        ];
        let selected_indices = vec![0usize, 2, 0, 1, 2, 99];

        let specs =
            UiManager::build_library_selection_specs_for_entries(&entries, &selected_indices);
        assert_eq!(specs.len(), 3);

        match &specs[0] {
            protocol::LibrarySelectionSpec::Track { path } => {
                assert_eq!(path, &PathBuf::from("a.mp3"));
            }
            other => panic!("expected first track spec, got {:?}", other),
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
            protocol::LibrarySelectionSpec::Track { path } => {
                assert_eq!(path, &PathBuf::from("b.mp3"));
            }
            other => panic!("expected second track spec, got {:?}", other),
        }
    }

    #[test]
    fn test_render_column_value_replaces_placeholders() {
        let metadata = make_meta("Track");
        let rendered = UiManager::render_column_value(&metadata, "{album} ({year})");
        assert_eq!(rendered, "Track-album (2026)");
    }

    #[test]
    fn test_render_column_value_handles_escaping_and_unknown_fields() {
        let metadata = make_meta("Track");
        let rendered = UiManager::render_column_value(&metadata, "{{{unknown}}} - {artist}");
        assert_eq!(rendered, "{} - Track-artist");
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
        let metadata = make_meta("Track");
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
        assert_eq!(values[0], "Track");
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
        let favorite = PlaylistColumnConfig {
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
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
        assert!(!UiManager::is_sortable_playlist_column(&favorite));
        assert!(UiManager::is_sortable_playlist_column(&title));
    }

    #[test]
    fn test_item_kind_for_favorite_category_uses_entity_kind_specific_icons() {
        assert_eq!(
            UiManager::item_kind_for_favorite_category(protocol::FavoriteEntityKind::Track),
            0
        );
        assert_eq!(
            UiManager::item_kind_for_favorite_category(protocol::FavoriteEntityKind::Artist),
            1
        );
        assert_eq!(
            UiManager::item_kind_for_favorite_category(protocol::FavoriteEntityKind::Album),
            2
        );
    }

    #[test]
    fn test_library_view_supports_artist_enrichment_prefetch_for_favorites_artists() {
        assert!(UiManager::library_view_supports_artist_enrichment_prefetch(
            &LibraryViewState::ArtistsRoot
        ));
        assert!(UiManager::library_view_supports_artist_enrichment_prefetch(
            &LibraryViewState::FavoriteArtists
        ));
        assert!(
            !UiManager::library_view_supports_artist_enrichment_prefetch(
                &LibraryViewState::AlbumsRoot
            )
        );
    }

    #[test]
    fn test_playlist_column_class_identifies_favorite_builtin() {
        let favorite = PlaylistColumnConfig {
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_favorite = PlaylistColumnConfig {
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
            enabled: true,
            custom: true,
        };

        assert_eq!(
            UiManager::playlist_column_class(&favorite),
            PlaylistColumnClass::Favorite
        );
        assert_eq!(
            UiManager::playlist_column_class(&custom_favorite),
            PlaylistColumnClass::Custom
        );
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
    fn test_fit_column_widths_deterministic_respects_available_space() {
        let specs = vec![
            DeterministicColumnLayoutSpec {
                min_px: 100,
                max_px: 400,
                target_px: 220,
                semantic_floor_px: 100,
                emergency_floor_px: 64,
                shrink_priority: 1,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 120,
                max_px: 360,
                target_px: 180,
                semantic_floor_px: 120,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 130,
                max_px: 380,
                target_px: 200,
                semantic_floor_px: 130,
                emergency_floor_px: 72,
                shrink_priority: 3,
                fixed_width: false,
            },
        ];

        let widths = fit_column_widths_deterministic(&specs, 420);
        assert_eq!(widths.iter().copied().sum::<u32>(), 420);
        assert_eq!(widths, vec![100, 120, 200]);
    }

    #[test]
    fn test_auto_growth_max_for_title_tracks_content_target_with_headroom() {
        let profile = ColumnWidthProfile {
            min_px: 140,
            preferred_px: 230,
            max_px: 440,
        };

        let max_px =
            UiManager::auto_growth_max_px_for_column(PlaylistColumnClass::Title, profile, 143);
        assert_eq!(max_px, 326);
    }

    #[test]
    fn test_auto_growth_max_for_title_respects_profile_max() {
        let profile = ColumnWidthProfile {
            min_px: 140,
            preferred_px: 230,
            max_px: 440,
        };

        let max_px =
            UiManager::auto_growth_max_px_for_column(PlaylistColumnClass::Title, profile, 400);
        assert_eq!(max_px, 440);
    }

    #[test]
    fn test_fit_column_widths_deterministic_grows_to_use_available_space() {
        let specs = vec![
            DeterministicColumnLayoutSpec {
                min_px: 100,
                max_px: 400,
                target_px: 220,
                semantic_floor_px: 100,
                emergency_floor_px: 64,
                shrink_priority: 1,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 120,
                max_px: 360,
                target_px: 180,
                semantic_floor_px: 120,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 130,
                max_px: 380,
                target_px: 200,
                semantic_floor_px: 130,
                emergency_floor_px: 72,
                shrink_priority: 3,
                fixed_width: false,
            },
        ];

        let widths = fit_column_widths_deterministic(&specs, 760);
        assert_eq!(widths.iter().copied().sum::<u32>(), 760);
        assert!(widths[0] >= 220);
        assert!(widths[1] >= 180);
        assert!(widths[2] >= 200);
    }

    #[test]
    fn test_fit_column_widths_deterministic_monotonic_for_adjacent_widths() {
        let specs = vec![
            DeterministicColumnLayoutSpec {
                min_px: 140,
                max_px: 440,
                target_px: 230,
                semantic_floor_px: 140,
                emergency_floor_px: 72,
                shrink_priority: 3,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 120,
                max_px: 320,
                target_px: 190,
                semantic_floor_px: 120,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 140,
                max_px: 360,
                target_px: 220,
                semantic_floor_px: 140,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
        ];

        let widths_narrow = fit_column_widths_deterministic(&specs, 449);
        let widths_wide = fit_column_widths_deterministic(&specs, 450);
        for (narrow, wide) in widths_narrow.iter().zip(widths_wide.iter()) {
            assert!(
                *wide >= *narrow,
                "column width decreased for wider viewport: {} -> {}",
                narrow,
                wide
            );
        }
        assert!(widths_wide
            .iter()
            .zip(widths_narrow.iter())
            .all(|(wide, narrow)| (*wide as i32 - *narrow as i32).abs() <= 1));
    }

    #[test]
    fn test_fit_column_widths_deterministic_keeps_fixed_album_art_width() {
        let specs = vec![
            DeterministicColumnLayoutSpec {
                min_px: 96,
                max_px: 96,
                target_px: 96,
                semantic_floor_px: 96,
                emergency_floor_px: 96,
                shrink_priority: 255,
                fixed_width: true,
            },
            DeterministicColumnLayoutSpec {
                min_px: 140,
                max_px: 440,
                target_px: 230,
                semantic_floor_px: 140,
                emergency_floor_px: 72,
                shrink_priority: 3,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 120,
                max_px: 320,
                target_px: 190,
                semantic_floor_px: 120,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
        ];

        let widths = fit_column_widths_deterministic(&specs, 260);
        assert_eq!(widths[0], 96);
        assert!(widths[1] >= 72);
        assert!(widths[2] >= 64);
    }

    #[test]
    fn test_fit_column_widths_deterministic_monotonic_across_full_resize_range() {
        let specs = vec![
            DeterministicColumnLayoutSpec {
                min_px: 16,
                max_px: 220,
                target_px: 72,
                semantic_floor_px: 72,
                emergency_floor_px: 72,
                shrink_priority: 255,
                fixed_width: true,
            },
            DeterministicColumnLayoutSpec {
                min_px: 140,
                max_px: 440,
                target_px: 230,
                semantic_floor_px: 140,
                emergency_floor_px: 72,
                shrink_priority: 3,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 120,
                max_px: 320,
                target_px: 190,
                semantic_floor_px: 120,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
            DeterministicColumnLayoutSpec {
                min_px: 140,
                max_px: 360,
                target_px: 220,
                semantic_floor_px: 140,
                emergency_floor_px: 64,
                shrink_priority: 2,
                fixed_width: false,
            },
        ];

        let mut previous = fit_column_widths_deterministic(&specs, 320);
        for available in 321..760 {
            let current = fit_column_widths_deterministic(&specs, available);
            for (index, (prev_width, current_width)) in
                previous.iter().zip(current.iter()).enumerate()
            {
                assert!(
                    *current_width >= *prev_width,
                    "column {index} decreased while growing width: {} -> {} (available={})",
                    prev_width,
                    current_width,
                    available
                );
            }
            previous = current;
        }

        let mut previous = fit_column_widths_deterministic(&specs, 760);
        for available in (320..760).rev() {
            let current = fit_column_widths_deterministic(&specs, available);
            for (index, (prev_width, current_width)) in
                previous.iter().zip(current.iter()).enumerate()
            {
                assert!(
                    *current_width <= *prev_width,
                    "column {index} increased while shrinking width: {} -> {} (available={})",
                    prev_width,
                    current_width,
                    available
                );
            }
            previous = current;
        }
    }

    #[test]
    fn test_resolve_playlist_column_target_width_falls_back_to_content_when_preserving_without_stored_target(
    ) {
        let profile = ColumnWidthProfile {
            min_px: 60,
            preferred_px: 160,
            max_px: 320,
        };

        let target =
            UiManager::resolve_playlist_column_target_width_px(None, None, 220, profile, true);

        assert_eq!(target, 220);
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
            Some(240),
            72,
            profile,
            true,
        );
        let from_content =
            UiManager::resolve_playlist_column_target_width_px(None, Some(240), 72, profile, false);

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

        let target =
            UiManager::resolve_playlist_column_target_width_px(None, Some(96), 72, profile, true);

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
