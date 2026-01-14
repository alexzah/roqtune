use crate::protocol::{PlaybackOrder, RepeatMode};
use log::debug;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Track {
    pub path: PathBuf,
    pub id: String,
}

#[derive(Clone)]
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

    pub fn move_tracks(&mut self, mut indices: Vec<usize>, to_gap: usize) {
        let len = self.tracks.len();
        if len == 0 {
            self.selected_indices.clear();
            return;
        }

        indices.sort_unstable();
        indices.dedup();
        indices.retain(|&i| i < len);

        if indices.is_empty() {
            return;
        }

        let to_gap = to_gap.min(len);

        let first = indices[0];
        let last = *indices.last().unwrap();
        let block_len = indices.len();

        if to_gap >= first && to_gap <= last + 1 {
            self.selected_indices = indices;
            return;
        }

        let mut moved = Vec::with_capacity(block_len);
        for &i in indices.iter().rev() {
            moved.push(self.tracks.remove(i));
        }
        moved.reverse();

        let removed_before = indices.iter().filter(|&&i| i < to_gap).count();
        let mut insert_at = to_gap.saturating_sub(removed_before);

        insert_at = insert_at.min(self.tracks.len());

        for (offset, t) in moved.into_iter().enumerate() {
            self.tracks.insert(insert_at + offset, t);
        }

        self.selected_indices = (insert_at..insert_at + block_len).collect();

        debug!(
            "Playlist: Moved tracks {:?} to gap {}. New playing_idx={:?}, selected_indices={:?}",
            indices, to_gap, self.playing_track_index, self.selected_indices
        );
    }

    pub fn get_next_track_index(&mut self, current_index: usize) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        if self.repeat_mode == RepeatMode::Track {
            return Some(current_index);
        }

        match self.playback_order {
            PlaybackOrder::Default => {
                let next_index = current_index + 1;
                if next_index < self.tracks.len() {
                    Some(next_index)
                } else if self.repeat_mode == RepeatMode::Playlist {
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
                    } else if self.repeat_mode == RepeatMode::Playlist {
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
                } else {
                    // Random is infinite even if repeat is off, but if there's only one track
                    // we return 0 if repeat is not Off. Wait, user said Random can be infinite in both.
                    Some(0)
                }
            }
        }
    }

    pub fn get_previous_track_index(&mut self, current_index: usize) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }

        if self.repeat_mode == RepeatMode::Track {
            return Some(current_index);
        }

        match self.playback_order {
            PlaybackOrder::Default => {
                if current_index > 0 {
                    Some(current_index - 1)
                } else if self.repeat_mode == RepeatMode::Playlist {
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
                    } else if self.repeat_mode == RepeatMode::Playlist {
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

    pub fn toggle_repeat(&mut self) -> RepeatMode {
        self.repeat_mode = match self.repeat_mode {
            RepeatMode::Off => RepeatMode::Playlist,
            RepeatMode::Playlist => RepeatMode::Track,
            RepeatMode::Track => RepeatMode::Off,
        };
        self.repeat_mode
    }

    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.repeat_mode = mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_track(id: &str) -> Track {
        Track {
            path: PathBuf::from(format!("/music/{}", id)),
            id: id.to_string(),
        }
    }

    fn assert_order(playlist: &Playlist, expected: Vec<&str>) {
        let actual: Vec<String> = playlist.tracks.iter().map(|t| t.id.clone()).collect();
        let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(actual, expected, "Track order mismatch");
    }

    fn assert_selected(playlist: &Playlist, expected: Vec<usize>) {
        assert_eq!(
            playlist.selected_indices, expected,
            "Selected indices mismatch"
        );
    }

    #[test]
    fn test_move_single_track_up() {
        // Move track A (pos 0) to gap 2 -> [B, A, C, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 2);

        assert_order(&playlist, vec!["B", "A", "C", "D"]);
        assert_selected(&playlist, vec![1]);
    }

    #[test]
    fn test_move_single_track_down() {
        // Move track D (pos 3) to gap 1 -> [A, D, B, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![3], 1);

        assert_order(&playlist, vec!["A", "D", "B", "C"]);
        assert_selected(&playlist, vec![1]);
    }

    #[test]
    fn test_move_single_track_to_start() {
        // Move track C (pos 2) to gap 0 -> [C, A, B, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2], 0);

        assert_order(&playlist, vec!["C", "A", "B", "D"]);
        assert_selected(&playlist, vec![0]);
    }

    #[test]
    fn test_move_single_track_to_end() {
        // Move track A (pos 0) to gap 4 -> [B, C, D, A]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 4);

        assert_order(&playlist, vec!["B", "C", "D", "A"]);
        assert_selected(&playlist, vec![3]);
    }

    #[test]
    fn test_move_multi_select_up() {
        // Move tracks A,B (pos 0,1) to gap 3 -> [C, A, B, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 3);

        assert_order(&playlist, vec!["C", "A", "B", "D"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_move_multi_select_down() {
        // Move tracks C,D (pos 2,3) to gap 1 -> [A, C, D, B]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2, 3], 1);

        assert_order(&playlist, vec!["A", "C", "D", "B"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_move_multi_select_to_start() {
        // Move tracks B,C (pos 1,2) to gap 0 -> [B, C, A, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2], 0);

        assert_order(&playlist, vec!["B", "C", "A", "D"]);
        assert_selected(&playlist, vec![0, 1]);
    }

    #[test]
    fn test_move_multi_select_to_end() {
        // Move tracks A,B (pos 0,1) to gap 4 -> [C, D, A, B]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 4);

        assert_order(&playlist, vec!["C", "D", "A", "B"]);
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_move_selected_tracks_updates_positions() {
        // Verify selected_indices are updated to new positions after move
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        // Select A, B
        playlist.selected_indices = vec![0, 1];

        // Move A, B to gap 4 (end)
        playlist.move_tracks(vec![0, 1], 4);

        // Selected should now be at positions 2, 3
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_move_tracks_across_whole_list() {
        // Move tracks A,B,C (pos 0,1,2) to position 4 (end) -> [D, A, B, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1, 2], 4);

        assert_order(&playlist, vec!["D", "A", "B", "C"]);
        assert_selected(&playlist, vec![1, 2, 3]);
    }

    #[test]
    fn test_move_tracks_from_middle_to_start() {
        // Move tracks B,C,D (pos 1,2,3) to position 0 -> [B, C, D, A]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2, 3], 0);

        assert_order(&playlist, vec!["B", "C", "D", "A"]);
        assert_selected(&playlist, vec![0, 1, 2]);
    }

    #[test]
    fn test_move_empty() {
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));

        playlist.move_tracks(vec![], 1);

        assert_order(&playlist, vec!["A", "B"]);
    }

    #[test]
    fn test_move_to_same_position() {
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));

        // Move B to position 1 (where it already is)
        playlist.move_tracks(vec![1], 1);

        assert_order(&playlist, vec!["A", "B", "C"]);
        assert_selected(&playlist, vec![1]);
    }

    // Gap-index semantic tests
    // Gap 0 = before element 0
    // Gap 1 = between element 0 and 1
    // Gap 2 = between element 1 and 2
    // Gap N = after element N-1

    #[test]
    fn test_gap_move_single_first_to_gap_0_noop() {
        // Moving A (first element) to gap 0 is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 0);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![0]);
    }

    #[test]
    fn test_gap_move_single_first_to_gap_1_noop() {
        // Moving A to gap 1 (after A) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 1);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![0]);
    }

    #[test]
    fn test_gap_move_single_first_to_gap_2() {
        // Moving A to gap 2 (between B and C) -> [B, A, C, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 2);

        assert_order(&playlist, vec!["B", "A", "C", "D"]);
        assert_selected(&playlist, vec![1]);
    }

    #[test]
    fn test_gap_move_single_first_to_gap_3() {
        // Moving A to gap 3 (between C and D) -> [B, C, A, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 3);

        assert_order(&playlist, vec!["B", "C", "A", "D"]);
        assert_selected(&playlist, vec![2]);
    }

    #[test]
    fn test_gap_move_single_first_to_gap_4() {
        // Moving A to gap 4 (after D, end) -> [B, C, D, A]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0], 4);

        assert_order(&playlist, vec!["B", "C", "D", "A"]);
        assert_selected(&playlist, vec![3]);
    }

    #[test]
    fn test_gap_move_single_middle_to_gap_0() {
        // Moving C to gap 0 (before A) -> [C, A, B, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2], 0);

        assert_order(&playlist, vec!["C", "A", "B", "D"]);
        assert_selected(&playlist, vec![0]);
    }

    #[test]
    fn test_gap_move_single_middle_to_gap_1_noop() {
        // Moving C to gap 1 (after A) -> [A, C, B, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2], 1);

        assert_order(&playlist, vec!["A", "C", "B", "D"]);
        assert_selected(&playlist, vec![1]);
    }

    #[test]
    fn test_gap_move_single_middle_to_gap_2_noop() {
        // Moving C to gap 2 (after C) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2], 2);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![2]);
    }

    #[test]
    fn test_gap_move_single_middle_to_gap_3() {
        // Moving C to gap 3 (after C) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2], 3);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![2]);
    }

    #[test]
    fn test_gap_move_single_last_to_gap_0() {
        // Moving D to gap 0 (before A) -> [D, A, B, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![3], 0);

        assert_order(&playlist, vec!["D", "A", "B", "C"]);
        assert_selected(&playlist, vec![0]);
    }

    #[test]
    fn test_gap_move_single_last_to_gap_1() {
        // Moving D to gap 1 (after A) -> [A, D, B, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![3], 1);

        assert_order(&playlist, vec!["A", "D", "B", "C"]);
        assert_selected(&playlist, vec![1]);
    }

    #[test]
    fn test_gap_move_single_last_to_gap_2() {
        // Moving D to gap 2 (after B) -> [A, B, D, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![3], 2);

        assert_order(&playlist, vec!["A", "B", "D", "C"]);
        assert_selected(&playlist, vec![2]);
    }

    #[test]
    fn test_gap_move_single_last_to_gap_3_noop() {
        // Moving D to gap 3 (after D) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![3], 3);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![3]);
    }

    #[test]
    fn test_gap_move_single_last_to_gap_4_noop() {
        // Moving D to gap 4 (after D) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![3], 4);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![3]);
    }

    #[test]
    fn test_gap_move_multi_first_two_to_gap_0_noop() {
        // Moving A,B to gap 0 is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 0);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![0, 1]);
    }

    #[test]
    fn test_gap_move_multi_first_two_to_gap_1_noop() {
        // Moving A,B to gap 1 (after A) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 1);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![0, 1]);
    }

    #[test]
    fn test_gap_move_multi_first_two_to_gap_2_noop() {
        // Moving A,B to gap 2 (after B) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 2);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![0, 1]);
    }

    #[test]
    fn test_gap_move_multi_first_two_to_gap_3() {
        // Moving A,B to gap 3 (between C and D) -> [C, A, B, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 3);

        assert_order(&playlist, vec!["C", "A", "B", "D"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_gap_move_multi_first_two_to_gap_4() {
        // Moving A,B to gap 4 (after D) -> [C, D, A, B]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1], 4);

        assert_order(&playlist, vec!["C", "D", "A", "B"]);
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_gap_move_multi_middle_two_to_gap_0() {
        // Moving B,C to gap 0 (before A) -> [B, C, A, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2], 0);

        assert_order(&playlist, vec!["B", "C", "A", "D"]);
        assert_selected(&playlist, vec![0, 1]);
    }

    #[test]
    fn test_gap_move_multi_middle_two_to_gap_1_noop() {
        // Moving B,C to gap 1 (after A) -> [A, B, C, D]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2], 1);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_gap_move_multi_middle_two_to_gap_2_noop() {
        // Moving B,C to gap 2 (after C) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2], 2);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_gap_move_multi_middle_two_to_gap_3_noop() {
        // Moving B,C to gap 3 (after C) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2], 3);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_gap_move_multi_middle_two_to_gap_4() {
        // Moving B,C to gap 4 (after D) -> [A, D, B, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2], 4);

        assert_order(&playlist, vec!["A", "D", "B", "C"]);
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_gap_move_multi_last_two_to_gap_0() {
        // Moving C,D to gap 0 (before A) -> [C, D, A, B]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2, 3], 0);

        assert_order(&playlist, vec!["C", "D", "A", "B"]);
        assert_selected(&playlist, vec![0, 1]);
    }

    #[test]
    fn test_gap_move_multi_last_two_to_gap_1() {
        // Moving C,D to gap 1 (after A) -> [A, C, D, B]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2, 3], 1);

        assert_order(&playlist, vec!["A", "C", "D", "B"]);
        assert_selected(&playlist, vec![1, 2]);
    }

    #[test]
    fn test_gap_move_multi_last_two_to_gap_2_noop() {
        // Moving C,D to gap 2 (after C) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2, 3], 2);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_gap_move_multi_last_two_to_gap_3_noop() {
        // Moving C,D to gap 3 (after D) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2, 3], 3);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_gap_move_multi_last_two_to_gap_4_noop() {
        // Moving C,D to gap 4 (after D) is a no-op
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![2, 3], 4);

        assert_order(&playlist, vec!["A", "B", "C", "D"]);
        assert_selected(&playlist, vec![2, 3]);
    }

    #[test]
    fn test_gap_move_all_three_to_gap_4() {
        // Moving A,B,C to gap 4 (end) -> [D, A, B, C]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![0, 1, 2], 4);

        assert_order(&playlist, vec!["D", "A", "B", "C"]);
        assert_selected(&playlist, vec![1, 2, 3]);
    }

    #[test]
    fn test_gap_move_all_three_from_middle_to_gap_0() {
        // Moving B,C,D to gap 0 -> [B, C, D, A]
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));
        playlist.add_track(make_track("D"));

        playlist.move_tracks(vec![1, 2, 3], 0);

        assert_order(&playlist, vec!["B", "C", "D", "A"]);
        assert_selected(&playlist, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_empty_list() {
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));

        playlist.move_tracks(vec![], 1);

        assert_order(&playlist, vec!["A", "B"]);
    }

    #[test]
    fn test_gap_out_of_bounds() {
        let mut playlist = Playlist::new();
        playlist.add_track(make_track("A"));
        playlist.add_track(make_track("B"));
        playlist.add_track(make_track("C"));

        // Moving A to gap 10 (clamped to 3, which is after C) -> [B, C, A]
        playlist.move_tracks(vec![0], 10);

        assert_order(&playlist, vec!["B", "C", "A"]);
    }
}
