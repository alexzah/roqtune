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
    SelectTrack(usize),
    TrackStarted(usize),
    TrackFinished(usize),
    ChangePlaybackOrder(PlaybackOrder),
    ToggleRepeat,
    RepeatModeChanged(bool),
    PlaylistIndicesChanged {
        playing_index: Option<usize>,
        selected_index: usize,
        is_playing: bool,
    },
}

#[derive(Debug, Clone)]
pub struct TechnicalMetadata {
    pub format: String,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub duration_secs: u64,
}

#[derive(Debug, Clone)]
pub enum AudioPacket {
    TrackHeader {
        id: String,
        play_immediately: bool,
        technical_metadata: TechnicalMetadata,
    },
    Samples {
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
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
}

#[derive(Debug, Clone)]
pub enum AudioMessage {
    DecodeTracks(Vec<TrackIdentifier>),
    StopDecoding,
    AudioPacket(AudioPacket),
    TrackCached(String),
    TrackEvictedFromCache(String),
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
    TechnicalMetadataChanged(TechnicalMetadata),
    PlaybackProgress { elapsed_secs: u64, total_secs: u64 },
}

#[derive(Debug, Clone)]
pub enum ConfigMessage {
    ConfigChanged(Config),
    AudioDeviceOpened { sample_rate: u32, channels: u16 },
}
