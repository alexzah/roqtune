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
pub enum ConfigMessage {
    ConfigChanged(Config),
    AudioDeviceOpened { sample_rate: u32, channels: u16 },
}
