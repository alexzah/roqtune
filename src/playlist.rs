use crate::protocol::PlaybackOrder;
use log::debug;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::path::PathBuf;
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RepeatMode {
    Off, // Stop after reaching the end of playlist
    On,  // Repeat playlist from the beginning
}

pub struct Track {
    pub path: PathBuf,
    pub id: String,
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
            playback_order: PlaybackOrder::Default,
            repeat_mode: RepeatMode::Off,
            shuffled_indices: Vec::new(),
            rng_seed: seed,
        };
    }

    pub fn add_track(&mut self, track: Track) {
        self.tracks.push(track);
        // Update shuffled indices when adding a new track
        if self.playback_order == PlaybackOrder::Shuffle {
            self.generate_shuffle_order(
                self.playing_track_index.or(Some(self.selected_track_index)),
            );
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
        if index >= self.tracks.len() {
            return;
        }

        self.tracks.remove(index);

        // Update playing track index
        if let Some(playing_idx) = self.playing_track_index {
            if playing_idx == index {
                self.playing_track_index = None;
                self.is_playing = false;
            } else if playing_idx > index {
                self.playing_track_index = Some(playing_idx - 1);
            }
        }

        // Update selected track index
        if self.selected_track_index == index {
            if self.tracks.is_empty() {
                self.selected_track_index = 0;
            } else if self.selected_track_index >= self.tracks.len() {
                self.selected_track_index = self.tracks.len() - 1;
            }
        } else if self.selected_track_index > index {
            self.selected_track_index -= 1;
        }

        // Update shuffle indices
        if !self.shuffled_indices.is_empty() {
            // Remove the deleted index and shift all indices greater than it
            self.shuffled_indices.retain(|&i| i != index);
            for i in 0..self.shuffled_indices.len() {
                if self.shuffled_indices[i] > index {
                    self.shuffled_indices[i] -= 1;
                }
            }
        }

        debug!(
            "Playlist: Deleted track at {}. New playing_idx={:?}, selected_idx={}, shuffle_len={}",
            index,
            self.playing_track_index,
            self.selected_track_index,
            self.shuffled_indices.len()
        );
    }

    pub fn get_track_id(&self, index: usize) -> String {
        return self.tracks[index].id.clone();
    }

    pub fn move_track(&mut self, from: usize, to: usize) {
        if from >= self.tracks.len() || to >= self.tracks.len() || from == to {
            return;
        }

        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);

        // Helper to update an index after a move
        let update_idx = |idx: usize| -> usize {
            if idx == from {
                to
            } else if from < to {
                if idx > from && idx <= to {
                    idx - 1
                } else {
                    idx
                }
            } else {
                // from > to
                if idx >= to && idx < from {
                    idx + 1
                } else {
                    idx
                }
            }
        };

        // Update playing track index
        if let Some(playing_idx) = self.playing_track_index {
            self.playing_track_index = Some(update_idx(playing_idx));
        }

        // Update selected track index
        self.selected_track_index = update_idx(self.selected_track_index);

        // Update shuffle indices
        for i in 0..self.shuffled_indices.len() {
            self.shuffled_indices[i] = update_idx(self.shuffled_indices[i]);
        }

        debug!(
            "Playlist: Moved track from {} to {}. New playing_idx={:?}, selected_idx={}",
            from, to, self.playing_track_index, self.selected_track_index
        );
    }

    // Get the next track index based on the current playback order and repeat mode
    pub fn get_next_track_index(&mut self, current_index: usize) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        match self.playback_order {
            PlaybackOrder::Default => {
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
                    self.generate_shuffle_order(Some(current_index));
                }

                // Find the current position in the shuffled list
                if let Some(position) = self
                    .shuffled_indices
                    .iter()
                    .position(|&i| i == current_index)
                {
                    if position + 1 < self.shuffled_indices.len() {
                        // Return the next track in the shuffled order
                        let next_track = self.shuffled_indices[position + 1];
                        debug!(
                            "Playlist: Advancing shuffle (position {}/{}): Track {} -> Track {}",
                            position + 1,
                            self.shuffled_indices.len(),
                            current_index,
                            next_track
                        );
                        Some(next_track)
                    } else if self.repeat_mode == RepeatMode::On {
                        // If repeat is on, wrap back to the beginning of shuffled list
                        let next_track = self.shuffled_indices.first().copied();
                        debug!(
                            "Playlist: Wrapping shuffle (repeat on): Track {} -> Track {:?}",
                            current_index, next_track
                        );
                        next_track
                    } else {
                        // End of shuffled playlist and repeat is off
                        debug!("Playlist: Shuffle reached end (repeat off)");
                        None
                    }
                } else {
                    // Current index not found in shuffle list (e.g., track just added)
                    // Re-generate shuffle starting with current
                    self.generate_shuffle_order(Some(current_index));
                    self.get_next_track_index(current_index)
                }
            }
            PlaybackOrder::Random => {
                // Pick a random track that's not the current one
                if self.tracks.len() > 1 {
                    let mut rng = StdRng::from_seed(self.rng_seed);
                    let mut next_index;
                    loop {
                        next_index = rng.random_range(0..self.tracks.len());
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

    pub fn get_previous_track_index(&mut self, current_index: usize) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        match self.playback_order {
            PlaybackOrder::Default => {
                if current_index > 0 {
                    Some(current_index - 1)
                } else if self.repeat_mode == RepeatMode::On {
                    Some(self.tracks.len() - 1)
                } else {
                    None
                }
            }
            PlaybackOrder::Shuffle => {
                if self.shuffled_indices.is_empty() {
                    self.generate_shuffle_order(Some(current_index));
                }

                if let Some(position) = self
                    .shuffled_indices
                    .iter()
                    .position(|&i| i == current_index)
                {
                    if position > 0 {
                        let prev_track = self.shuffled_indices[position - 1];
                        debug!(
                            "Playlist: Previous in shuffle (position {}/{}): Track {} -> Track {}",
                            position - 1,
                            self.shuffled_indices.len(),
                            current_index,
                            prev_track
                        );
                        Some(prev_track)
                    } else if self.repeat_mode == RepeatMode::On {
                        let prev_track = self.shuffled_indices.last().copied();
                        debug!(
                            "Playlist: Wrapping shuffle back (repeat on): Track {} -> Track {:?}",
                            current_index, prev_track
                        );
                        prev_track
                    } else {
                        debug!("Playlist: Shuffle reached start (repeat off)");
                        None
                    }
                } else {
                    self.generate_shuffle_order(Some(current_index));
                    self.get_previous_track_index(current_index)
                }
            }
            PlaybackOrder::Random => {
                // Random doesn't really have a "previous", just pick another random one
                if self.tracks.len() > 1 {
                    let mut rng = StdRng::from_seed(self.rng_seed);
                    let mut next_index;
                    loop {
                        next_index = rng.random_range(0..self.tracks.len());
                        if next_index != current_index {
                            break;
                        }
                    }
                    Some(next_index)
                } else {
                    Some(0)
                }
            }
        }
    }

    pub fn force_re_randomize_shuffle(&mut self) {
        if self.playback_order == PlaybackOrder::Shuffle {
            self.generate_shuffle_order(Some(self.selected_track_index));
        }
    }

    // Generate a random order for all tracks
    fn generate_shuffle_order(&mut self, first_track_index: Option<usize>) {
        let track_count = self.tracks.len();
        if track_count == 0 {
            self.shuffled_indices = Vec::new();
            return;
        }

        let mut indices: Vec<usize> = (0..track_count).collect();

        // Use a completely fresh seed from the OS for every new shuffle
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("Failed to generate random seed");
        self.rng_seed = seed;
        let mut rng = StdRng::from_seed(self.rng_seed);

        // Shuffle the indices
        for i in (1..track_count).rev() {
            let j = rng.random_range(0..=i);
            indices.swap(i, j);
        }

        // If a first track is specified, move it to the front
        if let Some(first_idx) = first_track_index {
            if let Some(pos) = indices.iter().position(|&i| i == first_idx) {
                indices.remove(pos);
                indices.insert(0, first_idx);
            }
        }

        debug!("Playlist: New shuffle sequence: {:?}", indices);
        self.shuffled_indices = indices;
    }

    pub fn set_playback_order(&mut self, order: PlaybackOrder) {
        if self.playback_order != order {
            self.playback_order = order;

            // If changing to shuffle, generate the shuffle order
            if order == PlaybackOrder::Shuffle {
                self.generate_shuffle_order(
                    self.playing_track_index.or(Some(self.selected_track_index)),
                );
            }
        }
    }

    pub fn toggle_repeat(&mut self) -> bool {
        self.repeat_mode = match self.repeat_mode {
            RepeatMode::Off => RepeatMode::On,
            RepeatMode::On => RepeatMode::Off,
        };
        self.repeat_mode == RepeatMode::On
    }
}
