use rand::{rngs::StdRng, Rng, SeedableRng};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlaybackOrder {
    Sequential, // Play tracks in order
    Shuffle,    // Randomize order once and play through that order
    Random,     // Pick a random track each time
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RepeatMode {
    Off, // Stop after reaching the end of playlist
    On,  // Repeat playlist from the beginning
}

pub struct Track {
    pub path: PathBuf,
}

pub struct Playlist {
    tracks: Vec<Track>,
    playing_track_index: Option<usize>,
    selected_track_index: usize,
    is_playing: bool,
    playback_order: PlaybackOrder,
    repeat_mode: RepeatMode,
    shuffled_indices: Vec<usize>,
    // Use StdRng instead of ThreadRng for thread safety
    rng_seed: [u8; 32],
}

impl Playlist {
    pub fn new() -> Playlist {
        // Generate a random seed
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("Failed to generate random seed");

        return Playlist {
            tracks: Vec::new(),
            playing_track_index: None,
            selected_track_index: 0,
            is_playing: false,
            playback_order: PlaybackOrder::Sequential,
            repeat_mode: RepeatMode::Off,
            shuffled_indices: Vec::new(),
            rng_seed: seed,
        };
    }

    pub fn add_track(&mut self, track: Track) {
        self.tracks.push(track);
        // Update shuffled indices when adding a new track
        if self.playback_order == PlaybackOrder::Shuffle {
            self.generate_shuffle_order();
        }
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

    pub fn get_track(&self, index: usize) -> &Track {
        return &self.tracks[index];
    }

    pub fn num_tracks(&self) -> usize {
        return self.tracks.len();
    }

    pub fn delete_track(&mut self, index: usize) {
        self.tracks.remove(index);
    }

    // Get the next track index based on the current playback order and repeat mode
    pub fn get_next_track_index(&mut self, current_index: usize) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        match self.playback_order {
            PlaybackOrder::Sequential => {
                // Return the next track in sequence
                let next_index = current_index + 1;
                if next_index < self.tracks.len() {
                    Some(next_index)
                } else if self.repeat_mode == RepeatMode::On {
                    // If repeat is on, wrap back to the beginning
                    Some(0)
                } else {
                    // End of playlist and repeat is off
                    None
                }
            }
            PlaybackOrder::Shuffle => {
                // If we haven't generated shuffled indices yet, do it now
                if self.shuffled_indices.is_empty() {
                    self.generate_shuffle_order();
                }

                // Find the current position in the shuffled list
                if let Some(position) = self
                    .shuffled_indices
                    .iter()
                    .position(|&i| i == current_index)
                {
                    if position + 1 < self.shuffled_indices.len() {
                        // Return the next track in the shuffled order
                        Some(self.shuffled_indices[position + 1])
                    } else if self.repeat_mode == RepeatMode::On {
                        // If repeat is on, wrap back to the beginning of shuffled list
                        self.shuffled_indices.first().copied()
                    } else {
                        // End of shuffled playlist and repeat is off
                        None
                    }
                } else {
                    // Current index not found in shuffle list (shouldn't happen)
                    // Just return the first track in the shuffle
                    self.shuffled_indices.first().copied()
                }
            }
            PlaybackOrder::Random => {
                // Pick a random track that's not the current one
                if self.tracks.len() > 1 {
                    let mut rng = StdRng::from_seed(self.rng_seed);
                    let mut next_index;
                    loop {
                        next_index = rng.gen_range(0..self.tracks.len());
                        if next_index != current_index {
                            break;
                        }
                    }
                    // Update the seed for next time
                    let mut new_seed = [0u8; 32];
                    for (i, val) in new_seed.iter_mut().enumerate() {
                        *val = self.rng_seed[i].wrapping_add(1);
                    }
                    self.rng_seed = new_seed;

                    Some(next_index)
                } else if self.tracks.len() == 1 {
                    // Only one track, just play it again if repeat is on
                    if self.repeat_mode == RepeatMode::On {
                        Some(0)
                    } else {
                        // If repeat is off and we've just played the only track, stop
                        None
                    }
                } else {
                    None
                }
            }
        }
    }

    // Generate a random order for all tracks
    fn generate_shuffle_order(&mut self) {
        let track_count = self.tracks.len();
        let mut indices: Vec<usize> = (0..track_count).collect();

        // Create a new RNG with our seed
        let mut rng = StdRng::from_seed(self.rng_seed);

        // Shuffle the indices
        for i in (1..track_count).rev() {
            let j = rng.gen_range(0..=i);
            indices.swap(i, j);
        }

        // Update the seed for next time
        let mut new_seed = [0u8; 32];
        for (i, val) in new_seed.iter_mut().enumerate() {
            *val = self.rng_seed[i].wrapping_add(1);
        }
        self.rng_seed = new_seed;

        self.shuffled_indices = indices;
    }

    pub fn set_playback_order(&mut self, order: PlaybackOrder) {
        if self.playback_order != order {
            self.playback_order = order;

            // If changing to shuffle, generate the shuffle order
            if order == PlaybackOrder::Shuffle {
                self.generate_shuffle_order();
            }
        }
    }

    pub fn get_playback_order(&self) -> PlaybackOrder {
        self.playback_order
    }

    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.repeat_mode = mode;
    }

    pub fn get_repeat_mode(&self) -> RepeatMode {
        self.repeat_mode
    }

    // Get first track index based on current playback order
    pub fn get_first_track_index(&mut self) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        match self.playback_order {
            PlaybackOrder::Sequential => Some(0),
            PlaybackOrder::Shuffle => {
                if self.shuffled_indices.is_empty() {
                    self.generate_shuffle_order();
                }
                self.shuffled_indices.first().copied()
            }
            PlaybackOrder::Random => {
                let mut rng = StdRng::from_seed(self.rng_seed);
                let index = rng.gen_range(0..self.tracks.len());

                // Update the seed for next time
                let mut new_seed = [0u8; 32];
                for (i, val) in new_seed.iter_mut().enumerate() {
                    *val = self.rng_seed[i].wrapping_add(1);
                }
                self.rng_seed = new_seed;

                Some(index)
            }
        }
    }
}
