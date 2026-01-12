use std::{path::PathBuf, rc::Rc};

use id3::{Tag, TagLike};
use log::debug;
use slint::{Model, ModelRc, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{protocol, AppWindow};

pub struct UiState {
    pub track_model: Rc<VecModel<ModelRc<StandardListViewItem>>>,
}

// Manages the playlist
pub struct UiManager {
    ui: slint::Weak<AppWindow>,
    bus_receiver: Receiver<protocol::Message>,
}

struct TrackMetadata {
    title: String,
    artist: String,
    album: String,
}

impl UiManager {
    pub fn new(
        ui: slint::Weak<AppWindow>,
        bus_receiver: Receiver<protocol::Message>,
        _bus_sender: Sender<protocol::Message>,
    ) -> Self {
        Self {
            ui: ui.clone(),
            bus_receiver,
        }
    }

    fn read_track_metadata(&self, path: &PathBuf) -> TrackMetadata {
        debug!("Reading metadata for: {}", path.display());

        // Try ID3 tags first
        if let Ok(tag) = Tag::read_from_path(path) {
            let title = tag.title().unwrap_or("");
            let artist = tag.artist().unwrap_or("");
            let album = tag.album().unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                };
            }
        }

        // Fall back to APE tags
        if let Ok(ape_tag) = ape::read_from_path(path) {
            let title: &str = ape_tag.item("title").unwrap().try_into().unwrap_or("");
            let artist: &str = ape_tag.item("artist").unwrap().try_into().unwrap_or("");
            let album: &str = ape_tag.item("album").unwrap().try_into().unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                };
            }
        }

        // Fall back to FLAC tags
        if let Ok(flac_tag) = metaflac::Tag::read_from_path(path) {
            let title = flac_tag
                .get_vorbis("title")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let artist = flac_tag
                .get_vorbis("artist")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let album = flac_tag
                .get_vorbis("album")
                .and_then(|mut i| i.next())
                .unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                };
            }
        }

        // If no tags found, use filename as title
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();

        TrackMetadata {
            title: filename,
            artist: "".to_string(),
            album: "".to_string(),
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::LoadTrack(path)) => {
                        let tags = self.read_track_metadata(&path);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");
                            track_model.push(ModelRc::new(VecModel::from(vec![
                                StandardListViewItem::from(""),
                                StandardListViewItem::from(tags.title.as_str()),
                                StandardListViewItem::from(tags.artist.as_str()),
                                StandardListViewItem::from(tags.album.as_str()),
                            ])));

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");
                            selection_model.push(false);
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTrack(index)) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");
                            if index < track_model.row_count() {
                                track_model.remove(index);
                            }

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");
                            if index < selection_model.row_count() {
                                selection_model.remove(index);
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackByIndex(
                        index,
                    )) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let mut title = String::new();
                            let mut artist = String::new();

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        title = item.row_data(1).unwrap().text.to_string();
                                        artist = item.row_data(2).unwrap().text.to_string();
                                    } else if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                            ui.set_playing_track_index(index as i32);
                            if !artist.is_empty() {
                                ui.set_status_text(format!("{} - {}", artist, title).into());
                            } else {
                                ui.set_status_text(title.into());
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let playing_index = ui.get_playing_track_index();
                            if playing_index >= 0 {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                    .expect("VecModel expected");
                                if (playing_index as usize) < track_model.row_count() {
                                    if let Some(item) = track_model.row_data(playing_index as usize)
                                    {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        let title = item.row_data(1).unwrap().text.to_string();
                                        let artist = item.row_data(2).unwrap().text.to_string();
                                        if !artist.is_empty() {
                                            ui.set_status_text(
                                                format!("{} - {}", artist, title).into(),
                                            );
                                        } else {
                                            ui.set_status_text(title.into());
                                        }
                                    }
                                }
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let playing_index = ui.get_playing_track_index();
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            if playing_index >= 0
                                && (playing_index as usize) < track_model.row_count()
                            {
                                if let Some(item) = track_model.row_data(playing_index as usize) {
                                    item.set_row_data(0, StandardListViewItem::from("⏸️"));
                                }
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlaybackProgress {
                        elapsed_ms,
                        total_ms,
                    }) => {
                        // debug!(
                        //     "UiManager: Playback progress: {}/{} ms",
                        //     elapsed_ms, total_ms
                        // );
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let elapsed_secs = elapsed_ms / 1000;
                            let elapsed_mins = elapsed_secs / 60;
                            let elapsed_rem_secs = elapsed_secs % 60;
                            let total_secs = total_ms / 1000;
                            let total_mins = total_secs / 60;
                            let total_rem_secs = total_secs % 60;

                            ui.set_time_info(
                                format!(
                                    "{:02}:{:02} / {:02}:{:02}",
                                    elapsed_mins, elapsed_rem_secs, total_mins, total_rem_secs
                                )
                                .into(),
                            );
                            if total_ms > 0 {
                                ui.set_position_percentage(elapsed_ms as f32 / total_ms as f32);
                            }
                            ui.set_current_position_text(
                                format!("{:02}:{:02}", elapsed_mins, elapsed_rem_secs).into(),
                            );
                            ui.set_total_duration_text(
                                format!("{:02}:{:02}", total_mins, total_rem_secs).into(),
                            );
                        });
                    }
                    protocol::Message::Playback(
                        protocol::PlaybackMessage::TechnicalMetadataChanged(meta),
                    ) => {
                        debug!("UiManager: Technical metadata changed: {:?}", meta);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_technical_info(
                                format!(
                                    "{} | {} kbps | {} Hz",
                                    meta.format, meta.bitrate_kbps, meta.sample_rate_hz
                                )
                                .into(),
                            );
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_technical_info("".into());
                            ui.set_time_info("".into());
                            ui.set_status_text("No track selected".into());
                            ui.set_position_percentage(0.0);
                            ui.set_current_position_text("0:00".into());
                            ui.set_total_duration_text("0:00".into());

                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                            ui.set_playing_track_index(-1);
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted(index)) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let mut title = String::new();
                            let mut artist = String::new();

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        title = item.row_data(1).unwrap().text.to_string();
                                        artist = item.row_data(2).unwrap().text.to_string();
                                    } else if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                            ui.set_selected_track_index(index as i32);
                            ui.set_playing_track_index(index as i32);
                            if !artist.is_empty() {
                                ui.set_status_text(format!("{} - {}", artist, title).into());
                            } else {
                                ui.set_status_text(title.into());
                            }
                        });
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::PlaylistIndicesChanged {
                            playing_index,
                            selected_indices,
                            is_playing,
                            repeat_on,
                        },
                    ) => {
                        let selected_indices_clone = selected_indices.clone();
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_playing_track_index(
                                playing_index.map(|i| i as i32).unwrap_or(-1),
                            );
                            ui.set_repeat_on(repeat_on);

                            // Set generic selected index
                            ui.set_selected_track_index(
                                selected_indices_clone
                                    .first()
                                    .copied()
                                    .map(|i| i as i32)
                                    .unwrap_or(-1),
                            );

                            // Set all selected indices for UI highlighting
                            let slint_indices: Vec<i32> =
                                selected_indices_clone.iter().map(|&i| i as i32).collect();
                            ui.set_selected_indices(slint::ModelRc::from(Rc::new(
                                slint::VecModel::from(slint_indices),
                            )));

                            // Update selection models
                            let track_model_strong = ui.get_track_model();
                            let num_tracks = track_model_strong.row_count();
                            let mut selections = vec![false; num_tracks];
                            for &idx in selected_indices_clone.iter() {
                                if idx < num_tracks {
                                    selections[idx] = true;
                                }
                            }
                            ui.set_selection_model(ModelRc::from(Rc::new(VecModel::from(
                                selections,
                            ))));

                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if playing_index == Some(i) {
                                        let emoji = if is_playing { "▶️" } else { "⏸️" };
                                        item.set_row_data(0, StandardListViewItem::from(emoji));
                                    } else if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                        indices,
                        to,
                    }) => {
                        let indices_clone = indices.clone();
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");

                            let mut sorted_indices = indices_clone.clone();
                            sorted_indices.sort_by(|a, b| b.cmp(a)); // reverse for safe removal

                            let mut moved_rows = Vec::new();
                            let mut moved_selections = Vec::new();

                            for &idx in sorted_indices.iter() {
                                if idx < track_model.row_count() {
                                    moved_rows.push(track_model.row_data(idx).unwrap());
                                    moved_selections.push(selection_model.row_data(idx).unwrap());
                                    track_model.remove(idx);
                                    selection_model.remove(idx);
                                }
                            }
                            moved_rows.reverse();
                            moved_selections.reverse();

                            let mut actual_to = to;
                            for &idx in indices_clone.iter() {
                                if idx < to {
                                    actual_to -= 1;
                                }
                            }
                            actual_to = actual_to.min(track_model.row_count());

                            for (i, row) in moved_rows.into_iter().enumerate() {
                                track_model.insert(actual_to + i, row);
                                selection_model.insert(actual_to + i, moved_selections[i]);
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::RepeatModeChanged(
                        repeat_on,
                    )) => {
                        debug!("UiManager: Repeat mode changed: {}", repeat_on);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_repeat_on(repeat_on);
                        });
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
