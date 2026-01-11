use std::{
    ops::Deref,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
};

use id3::{Tag, TagLike};
use log::{debug, error, trace};
use slint::{ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{
    playlist::{Playlist, Track},
    protocol, AppWindow,
};

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
                let title: &str = ape_tag.item("title").unwrap().try_into().unwrap();

                let artist: &str = ape_tag.item("artist").unwrap().try_into().unwrap();

                let album: &str = ape_tag.item("album").unwrap().try_into().unwrap();

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
                let title = flac_tag.get_vorbis("title").unwrap().next().unwrap_or("");
                let artist = flac_tag.get_vorbis("artist").unwrap().next().unwrap_or("");
                let album = flac_tag.get_vorbis("album").unwrap().next().unwrap_or("");

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
                            track_model.remove(index);
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

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                    } else {
                                        let current_text = item.row_data(0).unwrap().text;
                                        if !current_text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                            ui.set_playing_track_index(index as i32);
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("We know we set a VecModel earlier");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    let current_text = item.row_data(0).unwrap().text;
                                    if !current_text.is_empty() {
                                        item.set_row_data(0, StandardListViewItem::from(""));
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

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                    } else {
                                        let current_text = item.row_data(0).unwrap().text;
                                        if !current_text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }

                            ui.set_selected_track_index(index as i32);
                            ui.set_playing_track_index(index as i32);
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
