use std::{path::PathBuf, rc::Rc};

use id3::{Tag, TagLike};
use log::{debug, error, trace};
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
    bus_sender: Sender<protocol::Message>,
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
        bus_sender: Sender<protocol::Message>,
    ) -> Self {
        Self {
            ui: ui.clone(),
            bus_receiver: bus_receiver,
            bus_sender: bus_sender,
        }
    }

    fn read_track_metadata(&self, path: &PathBuf) -> TrackMetadata {
        debug!("Reading metadata for: {}", path.display());

        // Try ID3 tags first
        match Tag::read_from_path(path) {
            Ok(tag) => {
                let title = tag.title().unwrap_or("");
                let artist = tag.artist().unwrap_or("");
                let album = tag.album().unwrap_or("");

                if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                    debug!(
                        "Found ID3 tags: title='{}', artist='{}', album='{}'",
                        title, artist, album
                    );
                    return TrackMetadata {
                        title: title.to_string(),
                        artist: artist.to_string(),
                        album: album.to_string(),
                    };
                }
                debug!("ID3 tags were empty or incomplete");
            }
            Err(e) => {
                debug!("No ID3 tags found: {}", e);
            }
        }

        // Fall back to APE tags
        match ape::read_from_path(path) {
            Ok(ape_tag) => {
                let title: &str = ape_tag.item("title").unwrap().try_into().unwrap_or("");

                let artist: &str = ape_tag.item("artist").unwrap().try_into().unwrap_or("");

                let album: &str = ape_tag.item("album").unwrap().try_into().unwrap_or("");

                if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                    debug!(
                        "Found APE tags: title='{}', artist='{}', album='{}'",
                        title, artist, album
                    );
                    return TrackMetadata {
                        title: title.to_string(),
                        artist: artist.to_string(),
                        album: album.to_string(),
                    };
                }
                debug!("APE tags were empty or incomplete");
            }
            Err(e) => {
                debug!("No APE tags found: {}", e);
            }
        }

        // Fall back to FLAC tags
        match metaflac::Tag::read_from_path(path) {
            Ok(flac_tag) => {
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
                    debug!(
                        "Found FLAC tags: title='{}', artist='{}', album='{}'",
                        title, artist, album
                    );
                    return TrackMetadata {
                        title: title.to_string(),
                        artist: artist.to_string(),
                        album: album.to_string(),
                    };
                }
                debug!("FLAC tags were empty or incomplete");
            }
            Err(e) => {
                debug!("No FLAC tags found: {}", e);
            }
        }

        // If no tags found, use filename as title
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();

        debug!("No tags found, using filename: {}", filename);

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
                        debug!("Loading track: {}", path.display());
                        let tags: TrackMetadata = self.read_track_metadata(&path);

                        // Push the track to the playlist
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");
                            track_model.push(ModelRc::new(VecModel::from(vec![
                                StandardListViewItem::from(""),
                                StandardListViewItem::from(tags.title.as_str()),
                                StandardListViewItem::from(tags.artist.as_str()),
                                StandardListViewItem::from(tags.album.as_str()),
                            ])));
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTrack(index)) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");
                            if index < track_model.row_count() {
                                track_model.remove(index);
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
                                .expect("We know we set a VecModel earlier");

                            let mut title = String::new();
                            let mut artist = String::new();

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        title = item.row_data(1).unwrap().text.to_string();
                                        artist = item.row_data(2).unwrap().text.to_string();
                                    } else {
                                        if let Some(first_col) = item.row_data(0) {
                                            if !first_col.text.is_empty() {
                                                item.set_row_data(
                                                    0,
                                                    StandardListViewItem::from(""),
                                                );
                                            }
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
                                    .expect("We know we set a VecModel earlier");
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
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlaybackProgress {
                        elapsed_secs,
                        total_secs,
                    }) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let elapsed_mins = elapsed_secs / 60;
                            let elapsed_rem_secs = elapsed_secs % 60;
                            let total_mins = total_secs / 60;
                            let total_rem_secs = total_secs % 60;

                            ui.set_time_info(
                                format!(
                                    "{:02}:{:02} / {:02}:{:02}",
                                    elapsed_mins, elapsed_rem_secs, total_mins, total_rem_secs
                                )
                                .into(),
                            );
                            if total_secs > 0 {
                                ui.set_position_percentage(elapsed_secs as f32 / total_secs as f32);
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
                                .expect("We know we set a VecModel earlier");

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
                        debug!("UiManager: received TrackStarted message: {}", index);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");

                            let mut title = String::new();
                            let mut artist = String::new();

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        title = item.row_data(1).unwrap().text.to_string();
                                        artist = item.row_data(2).unwrap().text.to_string();
                                    } else {
                                        if let Some(first_col) = item.row_data(0) {
                                            if !first_col.text.is_empty() {
                                                item.set_row_data(
                                                    0,
                                                    StandardListViewItem::from(""),
                                                );
                                            }
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
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackFinished(
                        index,
                    )) => {
                        debug!("UiManager: received TrackFinished message: {}", index);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");
                            let item = track_model.row_data(index);
                            if let Some(item) = item {
                                item.set_row_data(0, StandardListViewItem::from(""));
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::RepeatModeChanged(
                        repeat_on,
                    )) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_repeat_on(repeat_on);
                        });
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::PlaylistIndicesChanged {
                            playing_index,
                            selected_index,
                        },
                    ) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_playing_track_index(
                                playing_index.map(|i| i as i32).unwrap_or(-1),
                            );
                            ui.set_selected_track_index(selected_index as i32);

                            // Refresh indicators
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if playing_index == Some(i) {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                    } else {
                                        if let Some(first_col) = item.row_data(0) {
                                            if !first_col.text.is_empty() {
                                                item.set_row_data(
                                                    0,
                                                    StandardListViewItem::from(""),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                    protocol::Message::Playlist(_) => {
                        trace!("UiManager: received unsupported playlist message");
                    }
                    _ => {
                        trace!("UiManager: received unsupported message {:?}", message);
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Ignore lag as we've increased the bus capacity
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    error!("UiManager: bus closed");
                    break;
                }
            }
        }
    }
}
