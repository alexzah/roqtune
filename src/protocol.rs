use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub output: OutputConfig,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OutputConfig {
    pub channel_count: u16,
    pub sample_rate_khz: u32,
    pub bits_per_sample: u16,
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
pub enum PlaylistMessage {
    LoadTrack(PathBuf),
    DeleteTrack(usize),
    TrackStarted(usize),
    TrackFinished,
    ChangePlaybackOrder(PlaybackOrder),
    ToggleRepeat,
    RepeatModeChanged(bool),
    PlaylistIndicesChanged {
        playing_index: Option<usize>,
        selected_indices: Vec<usize>,
        is_playing: bool,
        repeat_on: bool,
    },
    SelectTrackMulti {
        index: usize,
        ctrl: bool,
        shift: bool,
    },
    ReorderTracks {
        indices: Vec<usize>,
        to: usize,
    },
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
    StopDecoding,
    TrackCached(String),
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
    TrackStarted(String),
    ClearPlayerCache,
    Seek(f32),
    TechnicalMetadataChanged(TechnicalMetadata),
    PlaybackProgress { elapsed_ms: u64, total_ms: u64 },
}

#[derive(Debug, Clone)]
pub enum ConfigMessage {
    ConfigChanged(Config),
    AudioDeviceOpened { sample_rate: u32, channels: u16 },
}
