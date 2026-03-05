//! Event-bus protocol shared by all runtime components.
//!
//! This module defines all message payloads exchanged between playlist logic,
//! decoding, playback, UI, and runtime configuration handlers.

use std::path::PathBuf;

use crate::config::{
    BackendProfileConfig, LoudnessStandard, PlaylistColumnConfig, ResamplerQuality,
    UiPlaybackOrder, UiRepeatMode,
};
use crate::layout::LayoutConfig;

/// Repeat behavior applied when navigating beyond the current track.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RepeatMode {
    Off,      // Stop after reaching the end of playlist
    Playlist, // Repeat playlist from the beginning
    Track,    // Repeat current track
}

/// Top-level envelope for all bus traffic.
#[derive(Debug, Clone)]
pub enum Message {
    Playlist(PlaylistMessage),
    Library(LibraryMessage),
    Audio(AudioMessage),
    Playback(PlaybackMessage),
    Metadata(MetadataMessage),
    Config(ConfigMessage),
    Cast(CastMessage),
    Integration(IntegrationMessage),
}

/// Track traversal strategy for next/previous operations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackOrder {
    Default,
    Shuffle,
    Random,
}

/// Image category used for async list-thumbnail preparation updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub enum UiImageKind {
    CoverArt,
    ArtistImage,
}

/// Image variant used by the UI image pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub enum UiImageVariant {
    ListThumb,
    DetailOriginal,
}

/// Page navigation action for Home, End, PageUp, PageDown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageNavigationAction {
    Home,
    End,
    PageUp,
    PageDown,
}

/// Supported metadata link targets that can navigate Library views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum MetadataLinkKind {
    Artist,
    Album,
    Genre,
    Decade,
    Title,
}

/// UI-emitted metadata link activation payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct MetadataLinkPayload {
    pub kind: MetadataLinkKind,
    /// Primary link value rendered in the clicked metadata run.
    pub value: String,
    /// Album context used for title -> album-detail navigation.
    pub album: String,
    /// Album-artist context used for album-detail navigation.
    pub album_artist: String,
    /// Optional track path context for selecting one row after navigation.
    pub track_path: Option<PathBuf>,
}

/// Playback start notification payload.
#[derive(Debug, Clone)]
pub struct TrackStarted {
    /// Stable track id in the active playlist.
    pub id: String,
    /// Offset applied when playback started, in milliseconds.
    pub start_offset_ms: u64,
}

/// Playlist-domain commands and notifications.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum PlaylistMessage {
    #[allow(dead_code)]
    LoadTrack(PathBuf),
    DrainBulkImportQueue,
    #[allow(dead_code)]
    LoadTracksBatch {
        paths: Vec<PathBuf>,
        source: ImportSource,
    },
    DeleteTracks(Vec<usize>),
    DeleteSelected,
    PruneActivePlaylistPaths {
        paths: Vec<PathBuf>,
    },
    /// UI requested playback for a currently rendered track row.
    /// The index is in filtered/sorted view coordinates and must be mapped
    /// to playlist source coordinates by the UI manager.
    PlayTrackByViewIndex(usize),
    SelectTrackMulti {
        index: usize,
        ctrl: bool,
        shift: bool,
    },
    SelectionChanged(Vec<usize>),
    OnPointerDown {
        index: usize,
        ctrl: bool,
        shift: bool,
    },
    OnDragStart {
        pressed_index: usize,
    },
    OnDragMove {
        drop_gap: usize,
    },
    OnDragEnd {
        drop_gap: usize,
        drag_blocked: bool,
    },
    CopySelectedTracks,
    CutSelectedTracks,
    PasteCopiedTracks,
    UndoTrackListEdit,
    RedoTrackListEdit,
    PasteTracks(Vec<PathBuf>),
    AddTracksToPlaylists {
        playlist_ids: Vec<String>,
        paths: Vec<PathBuf>,
    },
    TracksInserted {
        tracks: Vec<RestoredTrack>,
        insert_at: usize,
    },
    TracksInsertedBatch {
        tracks: Vec<RestoredTrack>,
        insert_at: usize,
    },
    TrackMetadataBatchUpdated {
        updates: Vec<TrackMetadataPatch>,
    },
    TrackUnavailable {
        id: String,
        reason: String,
    },
    OpenPlaylistSearch,
    ClosePlaylistSearch,
    SetPlaylistSearchQuery(String),
    ClearPlaylistFilterView,
    CyclePlaylistSortByColumn(usize),
    RequestApplyFilterView,
    ApplyFilterViewSnapshot(Vec<usize>),
    PlaylistViewportChanged {
        first_row: usize,
        row_count: usize,
    },
    PlaylistViewportWidthChanged(u32),
    DeselectAll,
    SelectAll,
    /// Arrow key navigation.  `direction` is -1 (up) or +1 (down).
    /// When `shift` is true the selection extends from the current anchor;
    /// otherwise the selection collapses to the single navigated row.
    ArrowKeyNavigate {
        direction: i32,
        shift: bool,
    },
    /// Page navigation: Home, End, PageUp, PageDown.
    /// When `shift` is true the selection extends from the current anchor.
    /// `visible_row_count` is used for PageUp/PageDown to determine how far to move.
    PageNavigate {
        action: PageNavigationAction,
        shift: bool,
        visible_row_count: usize,
    },
    ReorderTracks {
        indices: Vec<usize>,
        to: usize,
    },
    PlaylistRestored(Vec<RestoredTrack>),
    TrackAdded {
        id: String,
        path: PathBuf,
    },
    CreatePlaylist {
        name: String,
    },
    RenamePlaylist {
        id: String,
        name: String,
    },
    RenamePlaylistByIndex(usize, String),
    DeletePlaylist {
        id: String,
    },
    DeletePlaylistByIndex(usize),
    SyncPlaylistToOpenSubsonicByIndex(usize),
    SyncPlaylistToOpenSubsonic {
        id: String,
    },
    SwitchPlaylist {
        id: String,
    },
    SwitchPlaylistByIndex(usize),
    RequestPlaylistState,
    PlaylistsRestored(Vec<PlaylistInfo>),
    OpenSubsonicSyncEligiblePlaylists(Vec<String>),
    ActivePlaylistChanged(String),
    SetActivePlaylistColumnWidthOverride {
        column_key: String,
        width_px: u32,
    },
    TrackFinished,
    TrackStarted {
        index: usize,
        playlist_id: String,
    },
    PlaylistIndicesChanged {
        playing_playlist_id: Option<String>,
        /// Index within the *playback queue* — **not** a source index into the
        /// editing playlist.  The playback queue is built in view order
        /// (filtered/sorted), so this value only coincides with the source
        /// index when no filter or sort is active.  Consumers must resolve
        /// the actual source position via `playing_track_id`.
        playing_index: Option<usize>,
        /// Stable unique track id of the currently playing track.  This is the
        /// authoritative key for mapping back to the editing playlist's source
        /// arrays, since it correctly identifies the entry even when duplicate
        /// file paths exist or a filter/sort view reorders the queue.
        playing_track_id: Option<String>,
        playing_track_path: Option<PathBuf>,
        playing_track_metadata: Option<DetailedMetadata>,
        selected_indices: Vec<usize>,
        is_playing: bool,
        playback_order: PlaybackOrder,
        repeat_mode: RepeatMode,
    },
    ChangePlaybackOrder(PlaybackOrder),
    ToggleRepeat,
    RepeatModeChanged(RepeatMode),
    RemoteDetachConfirmationRequested {
        playlist_id: String,
        playlist_name: String,
    },
    ConfirmDetachRemotePlaylist {
        playlist_id: String,
    },
    CancelDetachRemotePlaylist {
        playlist_id: String,
    },
    RemotePlaylistWritebackState {
        playlist_id: String,
        success: bool,
        error: Option<String>,
    },
}

/// Library-domain commands and notifications.
#[derive(Debug, Clone)]
pub enum LibraryMessage {
    SetCollectionMode(i32),
    SelectRootSection(i32),
    OpenGlobalSearch,
    SelectListItem {
        index: usize,
        ctrl: bool,
        shift: bool,
        context_click: bool,
    },
    ActivateMetadataLink {
        link: MetadataLinkPayload,
        reset_stack_to_root: bool,
    },
    NavigateBack,
    ActivateListItem(usize),
    PrepareAddToPlaylists,
    ToggleAddToPlaylist(usize),
    ConfirmAddToPlaylists,
    CancelAddToPlaylists,
    OpenSearch,
    CloseSearch,
    SetSearchQuery(String),
    AddSelectionToPlaylists {
        selections: Vec<LibrarySelectionSpec>,
        playlist_ids: Vec<String>,
    },
    /// Paste copied library selections into the current active playlist.
    /// This follows playlist paste insertion semantics (after the current
    /// selection anchor, or append to end when no selection exists).
    PasteSelectionToActivePlaylist {
        selections: Vec<LibrarySelectionSpec>,
    },
    CopySelected,
    CutSelected,
    DeleteSelected,
    OpenFileLocation,
    ConfirmRemoveSelection,
    CancelRemoveSelection,
    EvaluateRemoveSelection {
        request_id: u64,
        selections: Vec<LibrarySelectionSpec>,
    },
    RemoveSelectionFromLibrary {
        selections: Vec<LibrarySelectionSpec>,
        remove_from_playlists: bool,
    },
    RequestReplayGainScanSelection,
    ConfirmReplayGainScanOverwrite,
    SkipReplayGainScanExisting,
    CancelReplayGainScanOverwrite,
    AbortReplayGainScan,
    EvaluateReplayGainScanSelection {
        request_id: u64,
        selections: Vec<LibrarySelectionSpec>,
    },
    StartReplayGainScanSelection {
        request_id: u64,
        selections: Vec<LibrarySelectionSpec>,
        overwrite_existing: bool,
        loudness_standard: LoudnessStandard,
    },
    RequestScan,
    RequestRootCounts,
    RequestFavoritesSnapshot,
    #[allow(dead_code)]
    RequestTracks,
    #[allow(dead_code)]
    RequestArtists,
    #[allow(dead_code)]
    RequestAlbums,
    #[allow(dead_code)]
    RequestGenres,
    #[allow(dead_code)]
    RequestDecades,
    #[allow(dead_code)]
    RequestGlobalSearchData,
    #[allow(dead_code)]
    RequestArtistDetail {
        artist: String,
    },
    #[allow(dead_code)]
    RequestAlbumTracks {
        album: String,
        album_artist: String,
    },
    #[allow(dead_code)]
    RequestGenreTracks {
        genre: String,
    },
    #[allow(dead_code)]
    RequestDecadeTracks {
        decade: String,
    },
    DrainScanProgressQueue,
    RequestLibraryPage {
        request_id: u64,
        view: LibraryViewQuery,
        offset: usize,
        limit: usize,
        query: String,
    },
    ToggleFavorite {
        entity: FavoriteEntityRef,
        desired: Option<bool>,
    },
    ToggleFavoriteForLibraryRow {
        row_index: usize,
    },
    ToggleFavoriteForPlaylistRow {
        view_row: usize,
    },
    ToggleFavoriteNowPlaying,
    RequestEnrichment {
        entity: LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
    },
    ReplaceEnrichmentPrefetchQueue {
        entities: Vec<LibraryEnrichmentEntity>,
    },
    ReplaceEnrichmentBackgroundQueue {
        entities: Vec<LibraryEnrichmentEntity>,
    },
    SetEnrichmentTextOverrideForDisplay {
        field: LibraryEnrichmentOverrideField,
        display_priority: i32,
        text: String,
    },
    SetArtistImageOverrideForDisplay {
        display_priority: i32,
        source_path: PathBuf,
    },
    SetEnrichmentTextOverride {
        entity: LibraryEnrichmentEntity,
        text: String,
    },
    SetArtistImageOverride {
        artist: String,
        source_path: PathBuf,
    },
    EnrichmentPrefetchTick,
    ClearEnrichmentCache,
    LibraryViewportChanged {
        first_row: usize,
        row_count: usize,
    },
    ScanStarted,
    ScanProgress {
        discovered: usize,
        indexed: usize,
        metadata_pending: usize,
    },
    ScanCompleted {
        indexed_tracks: usize,
    },
    MetadataBackfillProgress {
        updated: usize,
        remaining: usize,
    },
    ScanFailed(String),
    RootCountsResult {
        tracks: usize,
        artists: usize,
        albums: usize,
        genres: usize,
        decades: usize,
        favorites: usize,
    },
    TracksResult(Vec<LibraryTrack>),
    ArtistsResult(Vec<LibraryArtist>),
    AlbumsResult(Vec<LibraryAlbum>),
    GenresResult(Vec<LibraryGenre>),
    DecadesResult(Vec<LibraryDecade>),
    GlobalSearchDataResult {
        tracks: Vec<LibraryTrack>,
        artists: Vec<LibraryArtist>,
        albums: Vec<LibraryAlbum>,
    },
    ArtistDetailResult {
        artist: String,
        albums: Vec<LibraryAlbum>,
        tracks: Vec<LibraryTrack>,
    },
    AlbumTracksResult {
        album: String,
        album_artist: String,
        tracks: Vec<LibraryTrack>,
    },
    GenreTracksResult {
        genre: String,
        tracks: Vec<LibraryTrack>,
    },
    DecadeTracksResult {
        decade: String,
        tracks: Vec<LibraryTrack>,
    },
    LibraryPageResult {
        request_id: u64,
        total: usize,
        entries: Vec<LibraryEntryPayload>,
    },
    FavoritesSnapshot {
        items: Vec<FavoriteEntityRef>,
    },
    FavoriteStateChanged {
        entity: FavoriteEntityRef,
        favorited: bool,
    },
    EnrichmentResult(LibraryEnrichmentPayload),
    EnrichmentOverrideFailed(String),
    EnrichmentCacheCleared {
        cleared_rows: usize,
        deleted_images: usize,
    },
    AddToPlaylistsCompleted {
        playlist_count: usize,
        track_count: usize,
    },
    AddToPlaylistsFailed(String),
    RemoveSelectionCompleted {
        removed_tracks: usize,
    },
    RemoveSelectionEvaluationResult {
        request_id: u64,
        requires_playlist_removal: bool,
    },
    RemoveSelectionFailed(String),
    ReplayGainScanEvaluationResult {
        request_id: u64,
        track_count: usize,
        existing_tag_count: usize,
    },
    ReplayGainScanEvaluationFailed {
        request_id: u64,
        error: String,
    },
    ToastTimeout {
        generation: u64,
    },
}

/// Stable identity for one enrichable library entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub enum LibraryEnrichmentEntity {
    Artist { artist: String },
    Album { album: String, album_artist: String },
}

/// Scheduling intent for enrichment requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LibraryEnrichmentPriority {
    Interactive,
    Prefetch,
}

/// Field identity for one editable library enrichment text payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LibraryEnrichmentOverrideField {
    AlbumDescription,
    ArtistBio,
}

/// Classification of enrichment failures for retry/backoff behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LibraryEnrichmentErrorKind {
    Timeout,
    RateLimited,
    BudgetExhausted,
    Hard,
}

/// Scheduler lane used for one enrichment attempt/result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
pub enum LibraryEnrichmentAttemptKind {
    Detail,
    #[default]
    VisiblePrefetch,
    BackgroundWarm,
}

/// Result state for one enrichment lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LibraryEnrichmentStatus {
    Ready,
    NotFound,
    Disabled,
    Error,
}

/// Display-only metadata fetched for library artist/album views.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct LibraryEnrichmentPayload {
    pub entity: LibraryEnrichmentEntity,
    pub status: LibraryEnrichmentStatus,
    pub blurb: String,
    pub image_path: Option<PathBuf>,
    pub source_name: String,
    pub source_url: String,
    #[serde(default)]
    pub error_kind: Option<LibraryEnrichmentErrorKind>,
    #[serde(default)]
    pub attempt_kind: LibraryEnrichmentAttemptKind,
}

/// Metadata editor commands and notifications.
#[derive(Debug, Clone)]
pub enum MetadataMessage {
    OpenPropertiesForCurrentSelection,
    SelectPropertiesTab {
        tab_index: usize,
    },
    EditPropertiesField {
        index: usize,
        value: String,
    },
    StagePropertiesImageOverwrite {
        slot_index: usize,
        source_path: PathBuf,
    },
    StagePropertiesImageDelete {
        slot_index: usize,
    },
    OpenPropertiesExternalImageLocation {
        index: usize,
    },
    SaveProperties,
    CancelProperties,
    RequestTrackProperties {
        request_id: u64,
        path: PathBuf,
    },
    TrackPropertiesLoaded {
        request_id: u64,
        path: PathBuf,
        display_name: String,
        metadata_fields: Vec<MetadataEditorField>,
        media_info_fields: Vec<PropertiesMediaInfoField>,
        embedded_image_slots: Vec<PropertiesEmbeddedImageSlot>,
        external_images: Vec<PropertiesExternalImage>,
    },
    TrackPropertiesLoadFailed {
        request_id: u64,
        path: PathBuf,
        error: String,
    },
    SaveTrackProperties {
        request_id: u64,
        path: PathBuf,
        metadata_fields: Vec<MetadataEditorField>,
        image_overwrites: Vec<PropertiesImageOverwrite>,
        image_deletes: Vec<PropertiesImageDelete>,
    },
    TrackPropertiesSaved {
        request_id: u64,
        path: PathBuf,
        summary: TrackMetadataSummary,
        db_sync_warning: Option<String>,
    },
    TrackPropertiesSaveFailed {
        request_id: u64,
        path: PathBuf,
        error: String,
    },
    /// Load properties for multiple selected tracks (aggregates fields across all files).
    RequestMultiTrackProperties {
        request_id: u64,
        paths: Vec<PathBuf>,
    },
    /// Aggregated properties response for multiple selected tracks.
    MultiTrackPropertiesLoaded {
        request_id: u64,
        paths: Vec<PathBuf>,
        track_count: usize,
        /// Aggregated metadata fields — common fields only, values joined or replaced with
        /// "[multiple values]" per the display rules. `mixed = true` on non-uniform fields.
        metadata_fields: Vec<MetadataEditorField>,
        /// Aggregated embedded image slots. `has_multiple_images = true` when >1 track has
        /// an image in the same slot.
        embedded_image_slots: Vec<PropertiesEmbeddedImageSlot>,
        // Note: media_info_fields and external_images are intentionally absent — they are
        // hidden in multi-select mode.
    },
    /// Save changed properties to all selected tracks.
    SaveMultiTrackProperties {
        request_id: u64,
        paths: Vec<PathBuf>,
        /// Only the fields the user actually edited (unchanged aggregated fields excluded).
        metadata_fields: Vec<MetadataEditorField>,
        image_overwrites: Vec<PropertiesImageOverwrite>,
        image_deletes: Vec<PropertiesImageDelete>,
    },
    /// Per-file save results after a multi-track properties save.
    MultiTrackPropertiesSaved {
        request_id: u64,
        paths: Vec<PathBuf>,
        results: Vec<MultiTrackSaveResult>,
    },
    /// Fatal error that prevented the multi-track save worker from starting.
    MultiTrackPropertiesSaveFailed {
        request_id: u64,
        error: String,
    },
    AbortReplayGainScan {
        request_id: u64,
    },
    ScanReplayGainForPaths {
        request_id: u64,
        targets: Vec<ReplayGainScanTarget>,
        album_references: Vec<ReplayGainAlbumReference>,
        overwrite_existing: bool,
        loudness_standard: LoudnessStandard,
    },
    ReplayGainScanStarted {
        request_id: u64,
        total_tracks: usize,
    },
    ReplayGainScanProgress {
        request_id: u64,
        processed: usize,
        total_tracks: usize,
        updated: usize,
        skipped: usize,
        failed: usize,
        current_track_label: String,
    },
    ReplayGainScanCompleted {
        request_id: u64,
        total_tracks: usize,
        updated: usize,
        skipped: usize,
        failed: usize,
    },
    ReplayGainScanAborted {
        request_id: u64,
        total_tracks: usize,
        processed: usize,
        updated: usize,
        skipped: usize,
        failed: usize,
    },
    ReplayGainScanFailed {
        request_id: u64,
        error: String,
    },
}

/// One selected track target for a ReplayGain scan request.
#[derive(Debug, Clone)]
pub struct ReplayGainScanTarget {
    pub path: PathBuf,
    pub album: String,
    pub album_artist: String,
}

/// Full-library album reference set used to compute album ReplayGain values.
#[derive(Debug, Clone)]
pub struct ReplayGainAlbumReference {
    pub album: String,
    pub album_artist: String,
    pub paths: Vec<PathBuf>,
}

/// Selection item used to resolve library items to concrete track paths.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub enum LibrarySelectionSpec {
    Track { path: PathBuf },
    Artist { artist: String },
    Album { album: String, album_artist: String },
    Genre { genre: String },
    Decade { decade: String },
}

/// Source hint for track ingest operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    AddFilesDialog,
    AddFolderDialog,
}

/// Metadata patch keyed by stable track id.
#[derive(Debug, Clone)]
pub struct TrackMetadataPatch {
    pub track_id: String,
    pub summary: TrackMetadataSummary,
}

/// Minimal track row restored from storage.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RestoredTrack {
    /// Stable track id.
    pub id: String,
    /// File path on disk.
    pub path: PathBuf,
}

/// Playback queue source used for UI synchronization and routing semantics.
#[derive(Debug, Clone)]
pub enum PlaybackQueueSource {
    Playlist { playlist_id: String },
    Library,
}

/// ReplayGain selection strategy for a playback queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainPreference {
    Track,
    AlbumPreferred,
}

/// ReplayGain tag source used for the active gain adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainAdjustmentKind {
    Track,
    Album,
}

/// Active playback route selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackRoute {
    Local,
    Cast,
}

/// Immutable playback queue snapshot used to bootstrap playback state.
#[derive(Debug, Clone)]
pub struct PlaybackQueueRequest {
    pub source: PlaybackQueueSource,
    pub replay_gain_preference: ReplayGainPreference,
    pub tracks: Vec<RestoredTrack>,
    pub start_index: usize,
}

/// Dedicated high-volume payload for playlist bulk-import queues.
#[derive(Debug, Clone)]
pub struct PlaylistBulkImportRequest {
    pub paths: Vec<PathBuf>,
    pub source: ImportSource,
}

/// Minimal playlist metadata restored from storage.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PlaylistInfo {
    /// Stable playlist id.
    pub id: String,
    /// User-visible name.
    pub name: String,
}

/// One indexed track entry in the music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryTrack {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub year: String,
    pub track_number: String,
}

/// Favorites entity kind supported by local persistence and integrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FavoriteEntityKind {
    Track,
    Artist,
    Album,
}

/// Canonical favorite entity identity and display metadata.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FavoriteEntityRef {
    pub kind: FavoriteEntityKind,
    pub entity_key: String,
    pub display_primary: String,
    pub display_secondary: String,
    pub track_path: Option<PathBuf>,
    pub remote_profile_id: Option<String>,
    pub remote_item_id: Option<String>,
}

/// Favorites root category row payload.
#[derive(Debug, Clone)]
pub struct FavoriteCategory {
    pub kind: FavoriteEntityKind,
    pub title: String,
    pub count: usize,
}

/// Paged library query selector.
#[derive(Debug, Clone)]
pub enum LibraryViewQuery {
    Tracks,
    Artists,
    Albums,
    Genres,
    Decades,
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

/// One album aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryAlbum {
    pub album: String,
    pub album_artist: String,
    pub track_count: u32,
    pub representative_track_path: Option<PathBuf>,
}

/// One artist aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryArtist {
    pub artist: String,
    pub album_count: u32,
    pub track_count: u32,
}

/// One genre aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryGenre {
    pub genre: String,
    pub track_count: u32,
}

/// One decade aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryDecade {
    pub decade: String,
    pub track_count: u32,
}

/// Generic paged-entry payload for library pagination requests.
#[derive(Debug, Clone)]
pub enum LibraryEntryPayload {
    Track(LibraryTrack),
    Artist(LibraryArtist),
    Album(LibraryAlbum),
    Genre(LibraryGenre),
    Decade(LibraryDecade),
    FavoriteCategory(FavoriteCategory),
}

/// Technical metadata emitted for the currently active track.
#[derive(Debug, Clone)]
pub struct TechnicalMetadata {
    /// Codec/container shorthand.
    pub format: String,
    /// Estimated average bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Effective sample rate in Hz.
    pub sample_rate_hz: u32,
    /// Channel count detected from the source track.
    pub channel_count: u16,
    /// Estimated duration in milliseconds.
    pub duration_ms: u64,
    /// Source bit depth (e.g., 16, 24, 32).
    pub bits_per_sample: u16,
    /// ReplayGain tag source used for the applied adjustment.
    pub replay_gain_adjustment_kind: Option<ReplayGainAdjustmentKind>,
    /// Effective ReplayGain adjustment applied by the decoder, in dB.
    /// `None` means no active ReplayGain adjustment.
    pub replay_gain_adjustment_db: Option<f32>,
}

/// Concrete output stream sample type selected by the audio backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSampleFormat {
    F32,
    I16,
    U16,
    Unknown,
}

/// Actual output stream profile opened by the audio backend.
#[derive(Debug, Clone)]
pub struct OutputStreamInfo {
    pub device_name: String,
    pub sample_rate_hz: u32,
    pub channel_count: u16,
    pub bits_per_sample: u16,
    pub sample_format: OutputSampleFormat,
}

/// Playback path info describing how source audio maps to output stream settings.
#[derive(Debug, Clone)]
pub struct OutputPathInfo {
    pub source_sample_rate_hz: u32,
    pub source_channel_count: u16,
    pub output_stream: OutputStreamInfo,
    pub resampled: bool,
    pub channel_transform: Option<ChannelTransformKind>,
    pub dithered: bool,
}

/// Channel-transform strategy used when source/output channel counts differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelTransformKind {
    Downmix,
    ChannelMap,
}

/// Audio payload delivered from decoder to player.
#[derive(Debug, Clone)]
pub enum AudioPacket {
    TrackHeader {
        id: String,
        play_immediately: bool,
        technical_metadata: TechnicalMetadata,
        start_offset_ms: u64,
    },
    Samples {
        samples: Vec<f32>,
    },
    TrackFooter {
        id: String,
    },
}

/// Track identity and startup options used for decode requests.
#[derive(Debug, Clone)]
pub struct TrackIdentifier {
    /// Stable track id.
    pub id: String,
    /// File path on disk.
    pub path: PathBuf,
    /// ReplayGain strategy inherited from the queue that requested this track.
    pub replay_gain_preference: ReplayGainPreference,
    /// Whether playback should start immediately after header arrives.
    pub play_immediately: bool,
    /// Decode start position in milliseconds.
    pub start_offset_ms: u64,
}

/// Audio-domain commands and notifications.
#[derive(Debug, Clone)]
pub enum AudioMessage {
    DecodeTracks(Vec<TrackIdentifier>),
    RequestDecodeChunk { requested_samples: usize },
    StopDecoding,
    TrackCached(String, u64), // id, start_offset_ms
    TrackEvicted(String),
    AudioPacket(AudioPacket),
}

/// Playback-domain commands and notifications.
#[derive(Debug, Clone)]
pub enum PlaybackMessage {
    ReadyForPlayback(String),
    Play, // resume the active playback queue
    PlayActiveCollection,
    StartQueue(PlaybackQueueRequest),
    PlayTrackById(String), // play a specific track by identifier
    Stop,
    Pause,
    Next,
    Previous,
    TrackFinished(String),
    TrackStarted(TrackStarted),
    ClearPlayerCache,
    ClearNextTracks,
    Seek(f32),
    SetVolume(f32),
    TechnicalMetadataChanged(TechnicalMetadata),
    OutputPathChanged(OutputPathInfo),
    PlaybackProgress {
        elapsed_ms: u64,
        total_ms: u64,
    },
    CoverArtChanged {
        request_id: u64,
        requested_track_path: Option<PathBuf>,
        cover_art_path: Option<PathBuf>,
    },
    ListImageReady {
        source_path: PathBuf,
        kind: UiImageKind,
        variant: UiImageVariant,
    },
    MetadataDisplayChanged(Option<DetailedMetadata>),
}

/// One discoverable Google Cast target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastDeviceInfo {
    /// Stable cast target id (UUID string from mDNS `id=` txt record when available).
    pub id: String,
    /// User-facing receiver name.
    pub name: String,
    /// Receiver model as reported by mDNS (`md=`), when available.
    pub model: String,
    /// Receiver host name.
    pub host: String,
    /// Receiver IPv4/IPv6 address.
    pub address: String,
    /// Cast control port (typically 8009).
    pub port: u16,
}

/// High-level cast connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastConnectionState {
    Disconnected,
    Discovering,
    Connecting,
    Connected,
}

/// Cast media path used for the current track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastPlaybackPathKind {
    Direct,
    TranscodeWavPcm,
}

/// Cast subsystem commands and notifications.
#[derive(Debug, Clone)]
pub enum CastMessage {
    DiscoverDevices,
    DevicesUpdated(Vec<CastDeviceInfo>),
    Connect {
        device_id: String,
    },
    Disconnect,
    ConnectionStateChanged {
        state: CastConnectionState,
        device: Option<CastDeviceInfo>,
        reason: Option<String>,
    },
    LoadTrack {
        track_id: String,
        path: PathBuf,
        start_offset_ms: u64,
        metadata_summary: Option<TrackMetadataSummary>,
    },
    Play,
    Pause,
    Stop,
    SeekMs(u64),
    SetVolume(f32),
    PlaybackPathChanged {
        kind: CastPlaybackPathKind,
        description: String,
        transcode_output_metadata: Option<TechnicalMetadata>,
    },
    PlaybackError {
        track_id: Option<String>,
        message: String,
        can_retry_with_transcode: bool,
    },
}

/// Rich metadata used for UI display panels.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DetailedMetadata {
    /// Track title.
    pub title: String,
    /// Track artist.
    pub artist: String,
    /// Album title.
    pub album: String,
    /// Album artist.
    #[serde(default)]
    pub album_artist: String,
    /// Date string as discovered from tags.
    pub date: String,
    /// Genre label.
    pub genre: String,
}

/// One editable metadata row exposed by the Properties editor.
#[derive(Debug, Clone)]
pub struct MetadataEditorField {
    /// Stable field identifier.
    pub id: String,
    /// User-visible field name.
    pub field_name: String,
    /// Current editable value.
    pub value: String,
    /// Whether this field is part of the built-in common set.
    pub common: bool,
    /// Whether the value is an aggregated multi-track placeholder (2–3 values joined, or
    /// "[multiple values]" for 4+). When `true` the user is editing across multiple tracks.
    pub mixed: bool,
}

/// One read-only media info field shown in the Properties dialog.
#[derive(Debug, Clone)]
pub struct PropertiesMediaInfoField {
    /// Stable field identifier.
    pub id: String,
    /// User-visible field label.
    pub label: String,
    /// Display value.
    pub value: String,
}

/// One embedded-artwork slot shown in the Properties dialog.
#[derive(Debug, Clone)]
pub struct PropertiesEmbeddedImageSlot {
    /// Picture type code based on ID3 APIC numbering.
    pub picture_type_code: u8,
    /// User-visible slot label.
    pub label: String,
    /// Cached local image path used for preview, when available.
    pub image_path: Option<PathBuf>,
    /// Whether an embedded image exists for this slot.
    pub has_image: bool,
    /// Supplemental read-only details.
    pub details: String,
    /// Whether this slot is part of the built-in common set.
    pub common: bool,
    /// Whether more than one selected track has an image in this slot (multi-select only).
    /// When `true` the preview is absent and a "Multiple" indicator is shown instead.
    pub has_multiple_images: bool,
}

/// One discovered external image candidate for the selected track.
#[derive(Debug, Clone)]
pub struct PropertiesExternalImage {
    /// Base filename (for display).
    pub file_name: String,
    /// User-visible image-type label resolved from filename conventions.
    pub label: String,
    /// Supplemental read-only details.
    pub details: String,
    /// Full filesystem path.
    pub path: PathBuf,
}

/// One staged embedded-artwork overwrite to apply on save.
#[derive(Debug, Clone)]
pub struct PropertiesImageOverwrite {
    /// Target picture type code based on ID3 APIC numbering.
    pub picture_type_code: u8,
    /// Source image path selected by the user.
    pub source_path: PathBuf,
}

/// One staged embedded-artwork delete to apply on save.
#[derive(Debug, Clone)]
pub struct PropertiesImageDelete {
    /// Target picture type code based on ID3 APIC numbering.
    pub picture_type_code: u8,
}

/// Metadata summary used to refresh playlist/library views after save.
#[derive(Debug, Clone)]
pub struct TrackMetadataSummary {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub date: String,
    pub genre: String,
    pub year: String,
    pub track_number: String,
}

/// Per-track result for a multi-track properties save operation.
#[derive(Debug, Clone)]
pub struct MultiTrackSaveResult {
    /// File path that was processed.
    pub path: PathBuf,
    /// Metadata summary used to refresh playlist/library rows on success.
    pub summary: Option<TrackMetadataSummary>,
    /// Error message when saving this specific file failed.
    pub error: Option<String>,
    /// Non-fatal DB sync warning from this specific file.
    pub db_sync_warning: Option<String>,
}

/// Runtime configuration updates and hardware notifications.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
#[allow(dead_code)]
pub enum ConfigDeltaEntry {
    Output(OutputConfigDelta),
    Cast(CastConfigDelta),
    Ui(UiConfigDelta),
    Library(LibraryConfigDelta),
    Buffering(BufferingConfigDelta),
    Integrations(IntegrationsConfigDelta),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputConfigDelta {
    pub output_device_name: Option<String>,
    pub output_device_auto: Option<bool>,
    pub channel_count: Option<u16>,
    pub sample_rate_khz: Option<u32>,
    pub bits_per_sample: Option<u16>,
    pub channel_count_auto: Option<bool>,
    pub sample_rate_auto: Option<bool>,
    pub bits_per_sample_auto: Option<bool>,
    pub resampler_quality: Option<ResamplerQuality>,
    pub dither_on_bitdepth_reduce: Option<bool>,
    pub downmix_higher_channel_tracks: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CastConfigDelta {
    pub allow_transcode_fallback: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiConfigDelta {
    pub show_layout_edit_intro: Option<bool>,
    pub show_tooltips: Option<bool>,
    pub auto_scroll_to_playing_track: Option<bool>,
    pub use_replaygain: Option<bool>,
    pub loudness_standard: Option<LoudnessStandard>,
    pub playlist_album_art_column_min_width_px: Option<u32>,
    pub playlist_album_art_column_max_width_px: Option<u32>,
    pub layout: Option<LayoutConfig>,
    pub playlist_columns: Option<Vec<PlaylistColumnConfig>>,
    pub window_width: Option<u32>,
    pub window_height: Option<u32>,
    pub volume: Option<f32>,
    pub playback_order: Option<UiPlaybackOrder>,
    pub repeat_mode: Option<UiRepeatMode>,
    pub crossfade_enabled: Option<bool>,
    pub crossfade_duration_seconds: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibraryConfigDelta {
    pub folders: Option<Vec<String>>,
    pub online_metadata_enabled: Option<bool>,
    pub online_metadata_prompt_pending: Option<bool>,
    pub include_playlist_tracks_in_library: Option<bool>,
    pub list_image_max_edge_px: Option<u32>,
    pub cover_art_cache_max_size_mb: Option<u32>,
    pub cover_art_memory_cache_max_size_mb: Option<u32>,
    pub artist_image_memory_cache_max_size_mb: Option<u32>,
    pub image_memory_cache_ttl_secs: Option<u32>,
    pub artist_image_cache_ttl_days: Option<u32>,
    pub artist_image_cache_max_size_mb: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BufferingConfigDelta {
    pub player_low_watermark_ms: Option<u32>,
    pub player_target_buffer_ms: Option<u32>,
    pub player_request_interval_ms: Option<u32>,
    pub decoder_request_chunk_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IntegrationsConfigDelta {
    pub backends: Option<Vec<BackendProfileConfig>>,
}

/// Runtime configuration updates and hardware notifications.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ConfigMessage {
    ConfigChanged(Vec<ConfigDeltaEntry>),
    RuntimeOutputSampleRateChanged { sample_rate_hz: u32 },
    AudioDeviceOpened { stream_info: OutputStreamInfo },
    SetRuntimeOutputRate { sample_rate_hz: u32, reason: String },
    ClearRuntimeOutputRateOverride,
    OutputDeviceCapabilitiesChanged { verified_sample_rates: Vec<u32> },
}

/// Registered backend kind used by integration profiles and track sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    LocalFs,
    OpenSubsonic,
}

/// High-level runtime connectivity state for one backend profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// Immutable snapshot for one configured backend profile.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BackendProfileSnapshot {
    pub profile_id: String,
    pub backend_kind: BackendKind,
    pub display_name: String,
    pub endpoint: String,
    pub username: String,
    pub configured: bool,
    pub connection_state: BackendConnectionState,
    pub status_text: Option<String>,
}

/// Immutable integration snapshot distributed on the event bus.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BackendSnapshot {
    pub version: u64,
    pub profiles: Vec<BackendProfileSnapshot>,
}

/// Integration-domain commands and notifications.
#[derive(Debug, Clone)]
pub enum IntegrationMessage {
    RequestSnapshot,
    UpsertBackendProfile {
        profile: BackendProfileSnapshot,
        password: Option<String>,
        connect_now: bool,
    },
    #[allow(dead_code)]
    RemoveBackendProfile {
        profile_id: String,
    },
    #[allow(dead_code)]
    ConnectBackendProfile {
        profile_id: String,
    },
    TestBackendConnection {
        profile_id: String,
    },
    DisconnectBackendProfile {
        profile_id: String,
    },
    SyncBackendProfile {
        profile_id: String,
    },
    #[allow(dead_code)]
    SetBackendConnectionState {
        profile_id: String,
        state: BackendConnectionState,
        status_text: Option<String>,
    },
    BackendSnapshotUpdated(BackendSnapshot),
    OpenSubsonicLibraryTracksUpdated {
        profile_id: String,
        tracks: Vec<LibraryTrack>,
    },
    OpenSubsonicPlaylistsUpdated {
        profile_id: String,
        playlists: Vec<RemotePlaylistSnapshot>,
    },
    OpenSubsonicFavoriteTracksUpdated {
        profile_id: String,
        tracks: Vec<LibraryTrack>,
    },
    PushOpenSubsonicTrackFavoriteUpdate {
        profile_id: String,
        song_id: String,
        favorited: bool,
        entity_key: String,
    },
    PushOpenSubsonicPlaylistUpdate {
        profile_id: String,
        remote_playlist_id: String,
        local_playlist_id: String,
        track_song_ids: Vec<String>,
    },
    CreateOpenSubsonicPlaylistFromLocal {
        profile_id: String,
        local_playlist_id: String,
        name: String,
        track_song_ids: Vec<String>,
    },
    OpenSubsonicPlaylistWritebackResult {
        local_playlist_id: String,
        success: bool,
        error: Option<String>,
    },
    OpenSubsonicPlaylistCreateResult {
        profile_id: String,
        local_playlist_id: String,
        remote_playlist_id: Option<String>,
        success: bool,
        error: Option<String>,
    },
    OpenSubsonicTrackFavoriteUpdateResult {
        profile_id: String,
        entity_key: String,
        favorited: bool,
        success: bool,
        error: Option<String>,
    },
    BackendOperationFailed {
        profile_id: Option<String>,
        action: String,
        error: String,
    },
}

/// Remote playlist snapshot emitted by integration sync events.
#[derive(Debug, Clone)]
pub struct RemotePlaylistSnapshot {
    pub remote_playlist_id: String,
    pub name: String,
    pub tracks: Vec<RemotePlaylistTrackSnapshot>,
}

/// One remote playlist track snapshot with display metadata.
#[derive(Debug, Clone)]
pub struct RemotePlaylistTrackSnapshot {
    pub item_id: String,
    pub path: PathBuf,
    pub summary: TrackMetadataSummary,
}
