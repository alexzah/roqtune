use log::{debug, error, trace};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{
    playlist::{Playlist, Track},
    protocol,
};

// Manages the playlist
pub struct PlaylistManager {
    playlist: Playlist,
    bus_consumer: Receiver<protocol::Message>,
    bus_producer: Sender<protocol::Message>,
}

impl PlaylistManager {
    pub fn new(
        playlist: Playlist,
        bus_consumer: Receiver<protocol::Message>,
        bus_producer: Sender<protocol::Message>,
    ) -> Self {
        Self {
            playlist: playlist,
            bus_consumer,
            bus_producer,
        }
    }

    pub fn run(&mut self) {
        loop {
            while let Ok(message) = self.bus_consumer.blocking_recv() {
                match message {
                    protocol::Message::Playlist(playlist_message) => match playlist_message {
                        protocol::PlaylistMessage::LoadTrack(path) => {
                            debug!("PlaylistManager: Loading track {:?}", path);
                            self.playlist.add_track(Track { path: path });
                        }
                    },
                    _ => {
                        trace!(
                            "PlaylistManager: ignoring unsupported message {:?}",
                            message
                        );
                    }
                }
            }
            error!("PlaylistManager: bad message received, trying again");
        }
    }
}
