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
}

#[derive(Debug, Clone)]
pub enum AudioMessage {
    BufferReady {
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
    },
}

#[derive(Debug, Clone)]
pub enum PlaybackMessage {
    Play,
    TrackFinished,
}
