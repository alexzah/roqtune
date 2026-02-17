//! Integration runtime coordinator.
//!
//! This manager is the bus-owned state holder for backend integration profiles
//! and remote sync output (library tracks + playlists).

use std::collections::HashMap;

use log::{debug, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::backends::opensubsonic::OpenSubsonicAdapter;
use crate::backends::{BackendProfileAuth, MediaBackendAdapter};
use crate::integration_uri::encode_opensubsonic_track_uri;
use crate::protocol::{
    BackendConnectionState, BackendKind, BackendProfileSnapshot, BackendSnapshot,
    IntegrationMessage, LibraryTrack, Message, RemotePlaylistSnapshot, RemotePlaylistTrackSnapshot,
    TrackMetadataSummary,
};

/// Coordinates integration profile state and snapshot fan-out over the event bus.
pub struct IntegrationManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    profiles: HashMap<String, BackendProfileSnapshot>,
    passwords: HashMap<String, String>,
    snapshot_version: u64,
    opensubsonic_adapter: OpenSubsonicAdapter,
}

impl IntegrationManager {
    /// Creates a manager bound to bus channels.
    pub fn new(bus_consumer: Receiver<Message>, bus_producer: Sender<Message>) -> Self {
        Self {
            bus_consumer,
            bus_producer,
            profiles: HashMap::new(),
            passwords: HashMap::new(),
            snapshot_version: 0,
            opensubsonic_adapter: OpenSubsonicAdapter::new(),
        }
    }

    fn emit_snapshot(&mut self) {
        self.snapshot_version = self.snapshot_version.saturating_add(1);
        let mut profiles: Vec<BackendProfileSnapshot> = self.profiles.values().cloned().collect();
        profiles.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        let snapshot = BackendSnapshot {
            version: self.snapshot_version,
            profiles,
        };
        let _ = self.bus_producer.send(Message::Integration(
            IntegrationMessage::BackendSnapshotUpdated(snapshot),
        ));
    }

    fn profile_auth(&self, profile_id: &str) -> Result<BackendProfileAuth, String> {
        let profile = self
            .profiles
            .get(profile_id)
            .ok_or_else(|| format!("unknown profile: {profile_id}"))?;
        let password = self
            .passwords
            .get(profile_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "missing cached OpenSubsonic credential for profile '{}'. Save credentials in Settings and reconnect.",
                    profile_id
                )
            })?;
        Ok(BackendProfileAuth {
            profile_id: profile.profile_id.clone(),
            endpoint: profile.endpoint.clone(),
            username: profile.username.clone(),
            password,
        })
    }

    fn upsert_profile(
        &mut self,
        profile: BackendProfileSnapshot,
        password: Option<String>,
        connect_now: bool,
    ) {
        if let Some(password) = password {
            self.passwords.insert(profile.profile_id.clone(), password);
        }
        let profile_id = profile.profile_id.clone();
        self.profiles.insert(profile_id.clone(), profile);
        self.emit_snapshot();
        if connect_now {
            self.connect_profile(&profile_id);
        }
    }

    fn remove_profile(&mut self, profile_id: &str) {
        let removed_profile = self.profiles.remove(profile_id);
        self.passwords.remove(profile_id);
        if let Some(profile) = removed_profile {
            if profile.backend_kind == BackendKind::OpenSubsonic {
                let _ = self.bus_producer.send(Message::Integration(
                    IntegrationMessage::OpenSubsonicLibraryTracksUpdated {
                        profile_id: profile_id.to_string(),
                        tracks: Vec::new(),
                    },
                ));
                let _ = self.bus_producer.send(Message::Integration(
                    IntegrationMessage::OpenSubsonicPlaylistsUpdated {
                        profile_id: profile_id.to_string(),
                        playlists: Vec::new(),
                    },
                ));
            }
            self.emit_snapshot();
        }
    }

    fn set_profile_connection_state(
        &mut self,
        profile_id: &str,
        state: BackendConnectionState,
        status_text: Option<String>,
    ) {
        if let Some(profile) = self.profiles.get_mut(profile_id) {
            profile.connection_state = state;
            profile.status_text = status_text;
            self.emit_snapshot();
        } else {
            warn!(
                "IntegrationManager: connection state update ignored for unknown profile {}",
                profile_id
            );
        }
    }

    fn emit_operation_failed(&self, profile_id: Option<String>, action: &str, error: String) {
        let _ = self.bus_producer.send(Message::Integration(
            IntegrationMessage::BackendOperationFailed {
                profile_id,
                action: action.to_string(),
                error,
            },
        ));
    }

    fn sync_opensubsonic_profile(
        &mut self,
        profile_id: &str,
        auth: &BackendProfileAuth,
    ) -> Result<(), String> {
        let tracks = self.opensubsonic_adapter.fetch_library_tracks(auth)?;
        let library_tracks: Vec<LibraryTrack> = tracks
            .iter()
            .map(|track| LibraryTrack {
                id: format!("subsonic:{}:{}", profile_id, track.item_id),
                path: encode_opensubsonic_track_uri(
                    &auth.profile_id,
                    &track.item_id,
                    &auth.endpoint,
                    &auth.username,
                    track.format_hint.as_deref(),
                )
                .into(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                album_artist: track.artist.clone(),
                genre: track.genre.clone(),
                year: track.year.clone(),
                track_number: track.track_number.clone(),
            })
            .collect();
        let _ = self.bus_producer.send(Message::Integration(
            IntegrationMessage::OpenSubsonicLibraryTracksUpdated {
                profile_id: profile_id.to_string(),
                tracks: library_tracks,
            },
        ));

        let playlists = self.opensubsonic_adapter.fetch_playlists(auth)?;
        let remote_playlists: Vec<RemotePlaylistSnapshot> = playlists
            .into_iter()
            .map(|playlist| RemotePlaylistSnapshot {
                remote_playlist_id: playlist.remote_playlist_id,
                name: playlist.name,
                tracks: playlist
                    .tracks
                    .into_iter()
                    .map(|track| RemotePlaylistTrackSnapshot {
                        item_id: track.item_id.clone(),
                        path: encode_opensubsonic_track_uri(
                            &auth.profile_id,
                            &track.item_id,
                            &auth.endpoint,
                            &auth.username,
                            track.format_hint.as_deref(),
                        )
                        .into(),
                        summary: TrackMetadataSummary {
                            title: track.title,
                            artist: track.artist.clone(),
                            album: track.album,
                            album_artist: track.artist,
                            date: track.year.clone(),
                            genre: track.genre,
                            year: track.year,
                            track_number: track.track_number,
                        },
                    })
                    .collect(),
            })
            .collect();
        let _ = self.bus_producer.send(Message::Integration(
            IntegrationMessage::OpenSubsonicPlaylistsUpdated {
                profile_id: profile_id.to_string(),
                playlists: remote_playlists,
            },
        ));
        Ok(())
    }

    fn connect_profile(&mut self, profile_id: &str) {
        let Some(profile) = self.profiles.get(profile_id).cloned() else {
            return;
        };
        match profile.backend_kind {
            BackendKind::OpenSubsonic => {
                self.set_profile_connection_state(
                    profile_id,
                    BackendConnectionState::Connecting,
                    Some("Connecting...".to_string()),
                );
                let auth = match self.profile_auth(profile_id) {
                    Ok(auth) => auth,
                    Err(error) => {
                        self.set_profile_connection_state(
                            profile_id,
                            BackendConnectionState::Error,
                            Some(error.clone()),
                        );
                        self.emit_operation_failed(Some(profile_id.to_string()), "connect", error);
                        return;
                    }
                };
                if let Err(error) = self.opensubsonic_adapter.test_connection(&auth) {
                    self.set_profile_connection_state(
                        profile_id,
                        BackendConnectionState::Error,
                        Some(error.clone()),
                    );
                    self.emit_operation_failed(Some(profile_id.to_string()), "connect", error);
                    return;
                }
                self.set_profile_connection_state(
                    profile_id,
                    BackendConnectionState::Connected,
                    Some("Connected".to_string()),
                );
                if let Err(error) = self.sync_opensubsonic_profile(profile_id, &auth) {
                    self.set_profile_connection_state(
                        profile_id,
                        BackendConnectionState::Error,
                        Some(error.clone()),
                    );
                    self.emit_operation_failed(Some(profile_id.to_string()), "sync", error);
                }
            }
            BackendKind::LocalFs => {}
        }
    }

    fn test_profile_connection(&mut self, profile_id: &str) {
        let auth = match self.profile_auth(profile_id) {
            Ok(auth) => auth,
            Err(error) => {
                self.set_profile_connection_state(
                    profile_id,
                    BackendConnectionState::Error,
                    Some(error.clone()),
                );
                self.emit_operation_failed(Some(profile_id.to_string()), "test", error);
                return;
            }
        };
        let result = self.opensubsonic_adapter.test_connection(&auth);
        match result {
            Ok(()) => {
                self.set_profile_connection_state(
                    profile_id,
                    BackendConnectionState::Connected,
                    Some("Connection test succeeded".to_string()),
                );
            }
            Err(error) => {
                self.set_profile_connection_state(
                    profile_id,
                    BackendConnectionState::Error,
                    Some(error.clone()),
                );
                self.emit_operation_failed(Some(profile_id.to_string()), "test", error);
            }
        }
    }

    fn disconnect_profile(&mut self, profile_id: &str) {
        let backend_kind = self
            .profiles
            .get(profile_id)
            .map(|profile| profile.backend_kind);
        self.set_profile_connection_state(
            profile_id,
            BackendConnectionState::Disconnected,
            Some("Disconnected".to_string()),
        );
        if backend_kind == Some(BackendKind::OpenSubsonic) {
            let _ = self.bus_producer.send(Message::Integration(
                IntegrationMessage::OpenSubsonicLibraryTracksUpdated {
                    profile_id: profile_id.to_string(),
                    tracks: Vec::new(),
                },
            ));
            let _ = self.bus_producer.send(Message::Integration(
                IntegrationMessage::OpenSubsonicPlaylistsUpdated {
                    profile_id: profile_id.to_string(),
                    playlists: Vec::new(),
                },
            ));
        }
    }

    fn sync_profile(&mut self, profile_id: &str) {
        let auth = match self.profile_auth(profile_id) {
            Ok(auth) => auth,
            Err(error) => {
                self.emit_operation_failed(Some(profile_id.to_string()), "sync", error.clone());
                self.set_profile_connection_state(
                    profile_id,
                    BackendConnectionState::Error,
                    Some(error),
                );
                return;
            }
        };
        if let Err(error) = self.sync_opensubsonic_profile(profile_id, &auth) {
            self.emit_operation_failed(Some(profile_id.to_string()), "sync", error.clone());
            self.set_profile_connection_state(
                profile_id,
                BackendConnectionState::Error,
                Some(error),
            );
        } else {
            self.set_profile_connection_state(
                profile_id,
                BackendConnectionState::Connected,
                Some("Synced".to_string()),
            );
        }
    }

    fn push_playlist_update(
        &mut self,
        profile_id: &str,
        remote_playlist_id: &str,
        local_playlist_id: &str,
        track_song_ids: Vec<String>,
    ) {
        let auth = match self.profile_auth(profile_id) {
            Ok(auth) => auth,
            Err(error) => {
                let _ = self.bus_producer.send(Message::Integration(
                    IntegrationMessage::OpenSubsonicPlaylistWritebackResult {
                        local_playlist_id: local_playlist_id.to_string(),
                        success: false,
                        error: Some(error),
                    },
                ));
                return;
            }
        };
        let result = self.opensubsonic_adapter.replace_playlist_tracks(
            &auth,
            remote_playlist_id,
            &track_song_ids,
        );
        match result {
            Ok(()) => {
                debug!(
                    "IntegrationManager: OpenSubsonic playlist '{}' writeback succeeded",
                    local_playlist_id
                );
                let _ = self.bus_producer.send(Message::Integration(
                    IntegrationMessage::OpenSubsonicPlaylistWritebackResult {
                        local_playlist_id: local_playlist_id.to_string(),
                        success: true,
                        error: None,
                    },
                ));
            }
            Err(error) => {
                self.emit_operation_failed(
                    Some(profile_id.to_string()),
                    "playlist_writeback",
                    error.clone(),
                );
                let _ = self.bus_producer.send(Message::Integration(
                    IntegrationMessage::OpenSubsonicPlaylistWritebackResult {
                        local_playlist_id: local_playlist_id.to_string(),
                        success: false,
                        error: Some(error),
                    },
                ));
            }
        }
    }

    /// Starts the blocking event loop.
    pub fn run(&mut self) {
        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(Message::Integration(IntegrationMessage::RequestSnapshot)) => {
                    self.emit_snapshot();
                }
                Ok(Message::Integration(IntegrationMessage::UpsertBackendProfile {
                    profile,
                    password,
                    connect_now,
                })) => {
                    self.upsert_profile(profile, password, connect_now);
                }
                Ok(Message::Integration(IntegrationMessage::RemoveBackendProfile {
                    profile_id,
                })) => {
                    self.remove_profile(&profile_id);
                }
                Ok(Message::Integration(IntegrationMessage::ConnectBackendProfile {
                    profile_id,
                })) => {
                    self.connect_profile(&profile_id);
                }
                Ok(Message::Integration(IntegrationMessage::TestBackendConnection {
                    profile_id,
                })) => {
                    self.test_profile_connection(&profile_id);
                }
                Ok(Message::Integration(IntegrationMessage::DisconnectBackendProfile {
                    profile_id,
                })) => {
                    self.disconnect_profile(&profile_id);
                }
                Ok(Message::Integration(IntegrationMessage::SyncBackendProfile { profile_id })) => {
                    self.sync_profile(&profile_id);
                }
                Ok(Message::Integration(IntegrationMessage::PushOpenSubsonicPlaylistUpdate {
                    profile_id,
                    remote_playlist_id,
                    local_playlist_id,
                    track_song_ids,
                })) => {
                    self.push_playlist_update(
                        &profile_id,
                        &remote_playlist_id,
                        &local_playlist_id,
                        track_song_ids,
                    );
                }
                Ok(Message::Integration(IntegrationMessage::SetBackendConnectionState {
                    profile_id,
                    state,
                    status_text,
                })) => {
                    self.set_profile_connection_state(&profile_id, state, status_text);
                }
                Ok(Message::Integration(IntegrationMessage::BackendSnapshotUpdated(_)))
                | Ok(Message::Integration(IntegrationMessage::BackendOperationFailed { .. }))
                | Ok(Message::Integration(
                    IntegrationMessage::OpenSubsonicLibraryTracksUpdated { .. },
                ))
                | Ok(Message::Integration(IntegrationMessage::OpenSubsonicPlaylistsUpdated {
                    ..
                }))
                | Ok(Message::Integration(
                    IntegrationMessage::OpenSubsonicPlaylistWritebackResult { .. },
                ))
                | Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        "IntegrationManager lagged on control bus, skipped {} message(s)",
                        skipped
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IntegrationManager;
    use crate::protocol::{
        BackendConnectionState, BackendKind, BackendProfileSnapshot, IntegrationMessage, Message,
    };
    use tokio::sync::broadcast;

    fn test_profile(profile_id: &str) -> BackendProfileSnapshot {
        BackendProfileSnapshot {
            profile_id: profile_id.to_string(),
            backend_kind: BackendKind::OpenSubsonic,
            display_name: "Home Server".to_string(),
            endpoint: "https://music.example.com".to_string(),
            username: "alice".to_string(),
            configured: true,
            connection_state: BackendConnectionState::Disconnected,
            status_text: None,
        }
    }

    #[test]
    fn test_upsert_profile_emits_snapshot() {
        let (bus_sender, _) = broadcast::channel(16);
        let mut manager = IntegrationManager::new(bus_sender.subscribe(), bus_sender.clone());
        let mut observer = bus_sender.subscribe();

        manager.upsert_profile(test_profile("subsonic-home"), None, false);

        let message = observer
            .try_recv()
            .expect("snapshot message should be emitted");
        let Message::Integration(IntegrationMessage::BackendSnapshotUpdated(snapshot)) = message
        else {
            panic!("unexpected message emitted by integration manager");
        };
        assert_eq!(snapshot.version, 1);
        assert_eq!(snapshot.profiles.len(), 1);
        assert_eq!(snapshot.profiles[0].profile_id, "subsonic-home");
        assert_eq!(snapshot.profiles[0].backend_kind, BackendKind::OpenSubsonic);
    }

    #[test]
    fn test_set_connection_state_updates_snapshot() {
        let (bus_sender, _) = broadcast::channel(16);
        let mut manager = IntegrationManager::new(bus_sender.subscribe(), bus_sender.clone());
        let mut observer = bus_sender.subscribe();
        manager.upsert_profile(test_profile("subsonic-home"), None, false);
        let _ = observer.try_recv();

        manager.set_profile_connection_state(
            "subsonic-home",
            BackendConnectionState::Connected,
            Some("Connected".to_string()),
        );

        let message = observer
            .try_recv()
            .expect("connection-state update should emit snapshot");
        let Message::Integration(IntegrationMessage::BackendSnapshotUpdated(snapshot)) = message
        else {
            panic!("unexpected message emitted by integration manager");
        };
        assert_eq!(snapshot.version, 2);
        assert_eq!(
            snapshot.profiles[0].connection_state,
            BackendConnectionState::Connected
        );
        assert_eq!(
            snapshot.profiles[0].status_text.as_deref(),
            Some("Connected")
        );
    }

    #[test]
    fn test_remove_profile_emits_snapshot_only_when_profile_exists() {
        let (bus_sender, _) = broadcast::channel(16);
        let mut manager = IntegrationManager::new(bus_sender.subscribe(), bus_sender.clone());
        let mut observer = bus_sender.subscribe();
        manager.upsert_profile(test_profile("subsonic-home"), None, false);
        let _ = observer.try_recv();

        manager.remove_profile("subsonic-home");
        let mut snapshot = None;
        for _ in 0..4 {
            let Ok(message) = observer.try_recv() else {
                break;
            };
            if let Message::Integration(IntegrationMessage::BackendSnapshotUpdated(next)) = message
            {
                snapshot = Some(next);
                break;
            }
        }
        let snapshot = snapshot.expect("removing existing profile should emit snapshot");
        assert_eq!(snapshot.version, 2);
        assert!(snapshot.profiles.is_empty());

        manager.remove_profile("does-not-exist");
        assert!(
            observer.try_recv().is_err(),
            "removing missing profile should not emit a snapshot"
        );
    }
}
