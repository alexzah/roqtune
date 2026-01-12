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
    selected_indices: Vec<usize>,
    anchor_index: Option<usize>,
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

        Playlist {
            tracks: Vec::new(),
            playing_track_index: None,
            selected_indices: Vec::new(),
            anchor_index: None,
            is_playing: false,
            playback_order: PlaybackOrder::Default,
            repeat_mode: RepeatMode::Off,
            shuffled_indices: Vec::new(),
            rng_seed: seed,
        }
    }

    pub fn add_track(&mut self, track: Track) {
        self.tracks.push(track);
        // Update shuffled indices when adding a new track
        if self.playback_order == PlaybackOrder::Shuffle {
            self.generate_shuffle_order(
                self.playing_track_index
                    .or(self.selected_indices.first().copied()),
            );
        }
    }

    pub fn set_playing(&mut self, is_playing: bool) {
        self.is_playing = is_playing;
    }

    pub fn set_selected_indices(&mut self, mut indices: Vec<usize>) {
        indices.retain(|&i| i < self.tracks.len());
        indices.sort();
        indices.dedup();
        self.selected_indices = indices;
    }

    pub fn get_selected_indices(&self) -> Vec<usize> {
        self.selected_indices.clone()
    }

    pub fn get_selected_track_index(&self) -> usize {
        self.selected_indices.first().copied().unwrap_or(0)
    }

    pub fn get_playing_track_index(&self) -> Option<usize> {
        self.playing_track_index
    }

    pub fn set_playing_track_index(&mut self, index: Option<usize>) {
        self.playing_track_index = index;
    }

    pub fn select_track_multi(&mut self, index: usize, ctrl: bool, shift: bool) {
        if index >= self.tracks.len() {
            return;
        }

        if shift {
            // Range selection from anchor to index
            let anchor = self.anchor_index.unwrap_or(index);
            let start = anchor.min(index);
            let end = anchor.max(index);

            self.selected_indices.clear();
            for i in start..=end {
                self.selected_indices.push(i);
            }
        } else if ctrl {
            // Toggle selection
            if let Some(pos) = self.selected_indices.iter().position(|&i| i == index) {
                self.selected_indices.remove(pos);
            } else {
                self.selected_indices.push(index);
            }
            self.anchor_index = Some(index);
        } else {
            // Single selection
            self.selected_indices.clear();
            self.selected_indices.push(index);
            self.anchor_index = Some(index);
        }
        self.selected_indices.sort();
        self.selected_indices.dedup();
    }

    pub fn deselect_all(&mut self) {
        self.selected_indices.clear();
        self.anchor_index = None;
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing
    }

    pub fn get_track(&self, index: usize) -> &Track {
        &self.tracks[index]
    }

    pub fn num_tracks(&self) -> usize {
        self.tracks.len()
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

        // Update selected indices
        self.selected_indices.retain(|&idx| idx != index);
        for i in 0..self.selected_indices.len() {
            if self.selected_indices[i] > index {
                self.selected_indices[i] -= 1;
            }
        }

        // Update anchor
        if let Some(anchor) = self.anchor_index {
            if anchor == index {
                self.anchor_index = None;
            } else if anchor > index {
                self.anchor_index = Some(anchor - 1);
            }
        }

        // Update shuffle indices
        if !self.shuffled_indices.is_empty() {
            self.shuffled_indices.retain(|&i| i != index);
            for i in 0..self.shuffled_indices.len() {
                if self.shuffled_indices[i] > index {
                    self.shuffled_indices[i] -= 1;
                }
            }
        }

        debug!(
            "Playlist: Deleted track at {}. New playing_idx={:?}, selected_indices={:?}, shuffle_len={}",
            index,
            self.playing_track_index,
            self.selected_indices,
            self.shuffled_indices.len()
        );
    }

    pub fn get_track_id(&self, index: usize) -> String {
        self.tracks[index].id.clone()
    }

    pub fn move_tracks(&mut self, mut indices: Vec<usize>, to: usize) {
        if indices.is_empty() {
            return;
        }
        indices.sort();
        indices.dedup();

        // 1. Identify which tracks are being moved
        let mut moved_tracks = Vec::new();
        // Remove from the end to keep indices stable while removing
        for &idx in indices.iter().rev() {
            if idx < self.tracks.len() {
                moved_tracks.push(self.tracks.remove(idx));
            }
        }
        moved_tracks.reverse();

        // 2. Calculate the target insertion point in the remaining list
        let mut actual_to = to;
        for &idx in indices.iter() {
            if idx < to {
                actual_to -= 1;
            }
        }
        actual_to = actual_to.min(self.tracks.len());

        // 3. Re-insert them
        for (i, track) in moved_tracks.into_iter().enumerate() {
            self.tracks.insert(actual_to + i, track);
        }

        // 4. Update indices (playing, selected, shuffle)
        let total_count = self.tracks.len();
        let mut old_to_new = vec![0; total_count + indices.len()];
        let moved_indices_set: std::collections::HashSet<usize> = indices.iter().cloned().collect();

        let mut remaining_old_indices = Vec::new();
        for i in 0..(total_count + indices.len()) {
            if !moved_indices_set.contains(&i) {
                remaining_old_indices.push(i);
            }
        }

        let mut new_order = Vec::new();
        for _ in 0..actual_to {
            new_order.push(remaining_old_indices.remove(0));
        }
        for &idx in indices.iter() {
            new_order.push(idx);
        }
        new_order.extend(remaining_old_indices);

        for (new_pos, &old_pos) in new_order.iter().enumerate() {
            old_to_new[old_pos] = new_pos;
        }

        if let Some(playing_idx) = self.playing_track_index {
            self.playing_track_index = Some(old_to_new[playing_idx]);
        }

        for i in 0..self.selected_indices.len() {
            self.selected_indices[i] = old_to_new[self.selected_indices[i]];
        }
        self.selected_indices.sort();

        if let Some(anchor) = self.anchor_index {
            self.anchor_index = Some(old_to_new[anchor]);
        }

        for i in 0..self.shuffled_indices.len() {
            self.shuffled_indices[i] = old_to_new[self.shuffled_indices[i]];
        }

        debug!(
            "Playlist: Moved tracks {:?} to {}. New playing_idx={:?}, selected_indices={:?}",
            indices, to, self.playing_track_index, self.selected_indices
        );
    }

    pub fn get_next_track_index(&mut self, current_index: usize) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        match self.playback_order {
            PlaybackOrder::Default => {
                let next_index = current_index + 1;
                if next_index < self.tracks.len() {
                    Some(next_index)
                } else if self.repeat_mode == RepeatMode::On {
                    Some(0)
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
                    if position + 1 < self.shuffled_indices.len() {
                        Some(self.shuffled_indices[position + 1])
                    } else if self.repeat_mode == RepeatMode::On {
                        self.shuffled_indices.first().copied()
                    } else {
                        None
                    }
                } else {
                    self.generate_shuffle_order(Some(current_index));
                    self.get_next_track_index(current_index)
                }
            }
            PlaybackOrder::Random => {
                if self.tracks.len() > 1 {
                    let mut rng = StdRng::from_seed(self.rng_seed);
                    let mut next_index;
                    loop {
                        next_index = rng.random_range(0..self.tracks.len());
                        if next_index != current_index {
                            break;
                        }
                    }
                    let mut new_seed = [0u8; 32];
                    for (i, val) in new_seed.iter_mut().enumerate() {
                        *val = self.rng_seed[i].wrapping_add(1);
                    }
                    self.rng_seed = new_seed;
                    Some(next_index)
                } else if self.tracks.len() == 1 {
                    if self.repeat_mode == RepeatMode::On {
                        Some(0)
                    } else {
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
                        Some(self.shuffled_indices[position - 1])
                    } else if self.repeat_mode == RepeatMode::On {
                        self.shuffled_indices.last().copied()
                    } else {
                        None
                    }
                } else {
                    self.generate_shuffle_order(Some(current_index));
                    self.get_previous_track_index(current_index)
                }
            }
            PlaybackOrder::Random => {
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
            self.generate_shuffle_order(Some(self.get_selected_track_index()));
        }
    }

    fn generate_shuffle_order(&mut self, first_track_index: Option<usize>) {
        let track_count = self.tracks.len();
        if track_count == 0 {
            self.shuffled_indices = Vec::new();
            return;
        }

        let mut indices: Vec<usize> = (0..track_count).collect();
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("Failed to generate random seed");
        self.rng_seed = seed;
        let mut rng = StdRng::from_seed(self.rng_seed);

        for i in (1..track_count).rev() {
            let j = rng.random_range(0..=i);
            indices.swap(i, j);
        }

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
            if order == PlaybackOrder::Shuffle {
                self.generate_shuffle_order(
                    self.playing_track_index
                        .or(self.selected_indices.first().copied()),
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

    pub fn get_repeat_mode(&self) -> RepeatMode {
        self.repeat_mode
    }
}
