use std::path::PathBuf;

pub struct Track {
    pub path: PathBuf,
}

pub struct Playlist {
    tracks: Vec<Track>,
    playing_track_index: Option<usize>,
    selected_track_index: usize,
    is_playing: bool
}

impl Playlist {
    pub fn new() -> Playlist {
        return Playlist {
            tracks: Vec::new(),
            playing_track_index: None,
            selected_track_index: 0,
            is_playing: false
        };
    }

    pub fn add_track(&mut self, track: Track) {
        self.tracks.push(track);
    }

    pub fn set_playing(&mut self, is_playing: bool) {
        self.is_playing = is_playing;
    }

    pub fn set_selected_track(&mut self, index: usize) {
        self.selected_track_index = index;
    }

    pub fn get_selected_track_index(&self) -> usize {
        return self.selected_track_index;
    }

    pub fn get_playing_track_index(&self) -> Option<usize> {
        return self.playing_track_index;
    }

    pub fn set_playing_track_index(&mut self, index: Option<usize>) {
        self.playing_track_index = index;
    }

    pub fn is_playing(&self) -> bool {
        return self.is_playing;
    }

    pub fn get_selected_track(&self) -> &Track {
        return &self.tracks[self.selected_track_index];
    }

    pub fn num_tracks(&self) -> usize {
        return self.tracks.len();
    }
}