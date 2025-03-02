use std::{
    ops::Deref,
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

    pub fn run(&mut self) {
        loop {
            while let Ok(message) = self.bus_receiver.blocking_recv() {
                match message {
                    protocol::Message::Playlist(playlist_message) => match playlist_message {
                        protocol::PlaylistMessage::LoadTrack(path) => {
                            debug!("Loading track: {}", path.display());
                            let tag = Tag::read_from_path(&path).unwrap_or_default();
                            let artist = tag.artist().unwrap_or("").to_string();
                            let title = tag.title().unwrap_or("").to_string();
                            let album = tag.album().unwrap_or("").to_string();

                            // Push the track to the playlist
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                    .expect("We know we set a VecModel earlier");
                                track_model.push(ModelRc::new(VecModel::from(vec![
                                    StandardListViewItem::from(title.as_str()),
                                    StandardListViewItem::from(artist.as_str()),
                                    StandardListViewItem::from(album.as_str()),
                                ])));
                            });
                        }
                    },
                    _ => {
                        trace!("UiManager: received unsupported message {:?}", message);
                    }
                }
            }
            error!("Bad message received by playlist manager, trying again");
        }
    }
}
