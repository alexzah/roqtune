use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Config {
    pub output: OutputConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub buffering: BufferingConfig,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct OutputConfig {
    pub channel_count: u16,
    pub sample_rate_khz: u32,
    pub bits_per_sample: u16,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct UiConfig {
    #[serde(default = "default_true")]
    pub show_album_art: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BufferingConfig {
    #[serde(default = "default_player_low_watermark_ms")]
    pub player_low_watermark_ms: u32,
    #[serde(default = "default_player_target_buffer_ms")]
    pub player_target_buffer_ms: u32,
    #[serde(default = "default_player_request_interval_ms")]
    pub player_request_interval_ms: u32,
    #[serde(default = "default_decoder_request_chunk_ms")]
    pub decoder_request_chunk_ms: u32,
}

impl Default for BufferingConfig {
    fn default() -> Self {
        Self {
            player_low_watermark_ms: default_player_low_watermark_ms(),
            player_target_buffer_ms: default_player_target_buffer_ms(),
            player_request_interval_ms: default_player_request_interval_ms(),
            decoder_request_chunk_ms: default_decoder_request_chunk_ms(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_player_low_watermark_ms() -> u32 {
    12_000
}

fn default_player_target_buffer_ms() -> u32 {
    24_000
}

fn default_player_request_interval_ms() -> u32 {
    120
}

fn default_decoder_request_chunk_ms() -> u32 {
    1_500
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RepeatMode {
    Off,      // Stop after reaching the end of playlist
    Playlist, // Repeat playlist from the beginning
    Track,    // Repeat current track
}

#[derive(Debug, Clone)]
pub enum Message {
    Playlist(PlaylistMessage),
    Audio(AudioMessage),
    Playback(PlaybackMessage),
    Config(ConfigMessage),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaybackOrder {
    Default,
    Shuffle,
    Random,
}

#[derive(Debug, Clone)]
pub struct TrackStarted {
    pub id: String,
    pub start_offset_ms: u64,
}

#[derive(Debug, Clone)]
pub enum PlaylistMessage {
    LoadTrack(PathBuf),
    DeleteTracks(Vec<usize>),
    DeleteSelected,
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
    UpdateMetadata {
        id: String,
        metadata: DetailedMetadata,
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
    TrackFinished,
    TrackStarted {
        index: usize,
        playlist_id: String,
        metadata: DetailedMetadata,
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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RestoredTrack {
    pub id: String,
    pub path: PathBuf,
    pub metadata: DetailedMetadata,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct TechnicalMetadata {
    pub format: String,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub duration_ms: u64,
}

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

#[derive(Debug, Clone)]
pub struct TrackIdentifier {
    pub id: String,
    pub path: PathBuf,
    pub play_immediately: bool,
    pub start_offset_ms: u64,
}

#[derive(Debug, Clone)]
pub enum AudioMessage {
    DecodeTracks(Vec<TrackIdentifier>),
    RequestDecodeChunk { requested_samples: usize },
    StopDecoding,
    TrackCached(String, u64), // id, start_offset_ms
    AudioPacket(AudioPacket),
}

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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DetailedMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub date: String,
    pub genre: String,
}

#[derive(Debug, Clone)]
pub enum ConfigMessage {
    ConfigChanged(Config),
    AudioDeviceOpened { sample_rate: u32, channels: u16 },
}
