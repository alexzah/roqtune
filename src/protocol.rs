use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Message {
    Playlist(PlaylistMessage),
    Audio(AudioMessage),
    Playback(PlaybackMessage),
}

#[derive(Debug, Clone)]
pub enum PlaylistMessage {
    LoadTrack(PathBuf),
    DeleteTrack(usize),
    SelectTrack(usize),
    TrackStarted(usize),
    TrackFinished(usize),
}

#[derive(Debug, Clone)]
pub enum AudioPacket {
    TrackHeader {
        id: String,
        play_immediately: bool,
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
    ReadyForPlayback,
    Play,                    // play the currently selected track
    PlayTrackByIndex(usize), // play a specific track by index
    PlayTrackById(String),   // play a specific track by identifier
    Stop,
    TrackFinished(String),
    TrackStarted(String),
    ClearPlayerCache,
}
