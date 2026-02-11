//! Event-bus protocol shared by all runtime components.
//!
//! This module defines all message payloads exchanged between playlist logic,
//! decoding, playback, UI, and runtime configuration handlers.

use std::path::PathBuf;

use crate::config::Config;

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
    Config(ConfigMessage),
}

/// Track traversal strategy for next/previous operations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackOrder {
    Default,
    Shuffle,
    Random,
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
#[derive(Debug, Clone)]
pub enum PlaylistMessage {
    LoadTrack(PathBuf),
    DeleteTracks(Vec<usize>),
    DeleteSelected,
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
    PlayLibraryQueue {
        tracks: Vec<RestoredTrack>,
        start_index: usize,
    },
    TracksInserted {
        tracks: Vec<RestoredTrack>,
        insert_at: usize,
    },
    OpenPlaylistSearch,
    ClosePlaylistSearch,
    SetPlaylistSearchQuery(String),
    ClearPlaylistFilterView,
    CyclePlaylistSortByColumn(usize),
    RequestApplyFilterView,
    ApplyFilterViewSnapshot(Vec<usize>),
    PlaylistViewportWidthChanged(u32),
    DeselectAll,
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
    SwitchPlaylist {
        id: String,
    },
    SwitchPlaylistByIndex(usize),
    PlaylistsRestored(Vec<PlaylistInfo>),
    ActivePlaylistChanged(String),
    ActivePlaylistColumnOrder(Option<Vec<String>>),
    SetActivePlaylistColumnOrder(Vec<String>),
    RequestActivePlaylistColumnOrder,
    ActivePlaylistColumnWidthOverrides(Option<Vec<PlaylistColumnWidthOverride>>),
    SetActivePlaylistColumnWidthOverride {
        column_key: String,
        width_px: u32,
        persist: bool,
    },
    ClearActivePlaylistColumnWidthOverride {
        column_key: String,
        persist: bool,
    },
    RequestActivePlaylistColumnWidthOverrides,
    TrackFinished,
    TrackStarted {
        index: usize,
        playlist_id: String,
    },
    PlaylistIndicesChanged {
        playing_playlist_id: Option<String>,
        playing_index: Option<usize>,
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
}

/// Library-domain commands and notifications.
#[derive(Debug, Clone)]
pub enum LibraryMessage {
    SetCollectionMode(i32),
    SelectRootSection(i32),
    SelectListItem {
        index: usize,
        ctrl: bool,
        shift: bool,
        context_click: bool,
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
    RequestScan,
    RequestSongs,
    RequestArtists,
    RequestAlbums,
    RequestGenres,
    RequestDecades,
    RequestArtistDetail {
        artist: String,
    },
    RequestAlbumSongs {
        album: String,
        album_artist: String,
    },
    RequestGenreSongs {
        genre: String,
    },
    RequestDecadeSongs {
        decade: String,
    },
    ScanStarted,
    ScanCompleted {
        indexed_tracks: usize,
    },
    ScanFailed(String),
    SongsResult(Vec<LibrarySong>),
    ArtistsResult(Vec<LibraryArtist>),
    AlbumsResult(Vec<LibraryAlbum>),
    GenresResult(Vec<LibraryGenre>),
    DecadesResult(Vec<LibraryDecade>),
    ArtistDetailResult {
        artist: String,
        albums: Vec<LibraryAlbum>,
        songs: Vec<LibrarySong>,
    },
    AlbumSongsResult {
        album: String,
        album_artist: String,
        songs: Vec<LibrarySong>,
    },
    GenreSongsResult {
        genre: String,
        songs: Vec<LibrarySong>,
    },
    DecadeSongsResult {
        decade: String,
        songs: Vec<LibrarySong>,
    },
    AddToPlaylistsCompleted {
        playlist_count: usize,
        track_count: usize,
    },
    AddToPlaylistsFailed(String),
    ToastTimeout {
        generation: u64,
    },
}

/// Selection item used to resolve library items to concrete track paths.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub enum LibrarySelectionSpec {
    Song { path: PathBuf },
    Artist { artist: String },
    Album { album: String, album_artist: String },
    Genre { genre: String },
    Decade { decade: String },
}

/// Persisted per-column width override for one playlist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct PlaylistColumnWidthOverride {
    /// Stable column key (`{title}` or `custom:Name|Format`).
    pub column_key: String,
    /// Width override in logical pixels.
    pub width_px: u32,
}

/// Minimal track row restored from storage.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RestoredTrack {
    /// Stable track id.
    pub id: String,
    /// File path on disk.
    pub path: PathBuf,
}

/// Minimal playlist metadata restored from storage.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PlaylistInfo {
    /// Stable playlist id.
    pub id: String,
    /// User-visible name.
    pub name: String,
}

/// One indexed song entry in the music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibrarySong {
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

/// One album aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryAlbum {
    pub album: String,
    pub album_artist: String,
    pub song_count: u32,
    pub representative_track_path: Option<PathBuf>,
}

/// One artist aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryArtist {
    pub artist: String,
    pub album_count: u32,
    pub song_count: u32,
}

/// One genre aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryGenre {
    pub genre: String,
    pub song_count: u32,
}

/// One decade aggregate entry in the indexed music library.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryDecade {
    pub decade: String,
    pub song_count: u32,
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
    /// Estimated duration in milliseconds.
    pub duration_ms: u64,
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
    Play,                    // play the currently selected track
    PlayTrackByIndex(usize), // play a specific track by index
    PlayTrackById(String),   // play a specific track by identifier
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
    PlaybackProgress { elapsed_ms: u64, total_ms: u64 },
    CoverArtChanged(Option<PathBuf>),
    MetadataDisplayChanged(Option<DetailedMetadata>),
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
    /// Date string as discovered from tags.
    pub date: String,
    /// Genre label.
    pub genre: String,
}

/// Runtime configuration updates and hardware notifications.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ConfigMessage {
    ConfigChanged(Config),
    AudioDeviceOpened { sample_rate: u32, channels: u16 },
}
