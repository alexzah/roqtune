//! OS media controls bridge (MPRIS/SMTC/Now Playing).
//!
//! This manager connects the runtime event bus to platform media control
//! integrations via `souvlaki`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{info, warn};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig, SeekDirection,
};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::protocol::{Message, PlaybackMessage, PlaylistMessage};

const MEDIA_CONTROLS_DISPLAY_NAME: &str = "Roqtune";
const MEDIA_CONTROLS_DBUS_NAME: &str = "roqtune";
const SEEK_STEP_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, Default)]
struct ControlState {
    is_playing: bool,
    elapsed_ms: u64,
    total_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackPublishState {
    Stopped,
    Paused,
    Playing,
}

/// Handles OS media control events and publishes app playback state.
pub struct MediaControlsManager {
    bus_consumer: Receiver<Message>,
    control_state: Arc<Mutex<ControlState>>,
    controls: Option<MediaControls>,
    current_track_path: Option<PathBuf>,
    last_published_playback: Option<PlaybackPublishState>,
    last_published_metadata_track_path: Option<PathBuf>,
    last_published_metadata_total_ms: u64,
}

impl MediaControlsManager {
    /// Creates a manager and attempts to initialize platform media controls.
    pub fn new(bus_consumer: Receiver<Message>, bus_producer: Sender<Message>) -> Self {
        let control_state = Arc::new(Mutex::new(ControlState::default()));
        let controls = Self::create_controls(bus_producer.clone(), Arc::clone(&control_state));

        Self {
            bus_consumer,
            control_state,
            controls,
            current_track_path: None,
            last_published_playback: None,
            last_published_metadata_track_path: None,
            last_published_metadata_total_ms: 0,
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn create_controls(
        bus_producer: Sender<Message>,
        control_state: Arc<Mutex<ControlState>>,
    ) -> Option<MediaControls> {
        let mut controls = match MediaControls::new(PlatformConfig {
            display_name: MEDIA_CONTROLS_DISPLAY_NAME,
            dbus_name: MEDIA_CONTROLS_DBUS_NAME,
            hwnd: None,
        }) {
            Ok(controls) => controls,
            Err(err) => {
                warn!(
                    "MediaControlsManager: failed to create media controls backend: {}",
                    err
                );
                return None;
            }
        };

        if let Err(err) = controls.attach(move |event| {
            let snapshot = match control_state.lock() {
                Ok(state) => *state,
                Err(poisoned) => *poisoned.into_inner(),
            };

            if let Some(playback_message) = Self::map_control_event(event, snapshot) {
                let _ = bus_producer.send(Message::Playback(playback_message));
            }
        }) {
            warn!(
                "MediaControlsManager: failed to attach media controls handler: {}",
                err
            );
            return None;
        }

        Some(controls)
    }

    #[cfg(target_os = "windows")]
    fn create_controls(
        _bus_producer: Sender<Message>,
        _control_state: Arc<Mutex<ControlState>>,
    ) -> Option<MediaControls> {
        // Souvlaki requires an HWND on Windows, which is not currently wired from Slint.
        warn!(
            "MediaControlsManager: Windows media controls are disabled because HWND wiring is not configured"
        );
        None
    }

    fn map_control_event(event: MediaControlEvent, state: ControlState) -> Option<PlaybackMessage> {
        match event {
            MediaControlEvent::Play => Some(PlaybackMessage::PlayActiveCollection),
            MediaControlEvent::Pause => Some(PlaybackMessage::Pause),
            MediaControlEvent::Toggle => {
                if state.is_playing {
                    Some(PlaybackMessage::Pause)
                } else {
                    Some(PlaybackMessage::PlayActiveCollection)
                }
            }
            MediaControlEvent::Next => Some(PlaybackMessage::Next),
            MediaControlEvent::Previous => Some(PlaybackMessage::Previous),
            MediaControlEvent::Stop => Some(PlaybackMessage::Stop),
            MediaControlEvent::SetPosition(position) => {
                Self::seek_message_from_target_ms(state, position.0.as_millis() as u64)
            }
            MediaControlEvent::SeekBy(direction, delta) => {
                let delta_ms = delta.as_millis() as u64;
                let target_ms = match direction {
                    SeekDirection::Forward => state.elapsed_ms.saturating_add(delta_ms),
                    SeekDirection::Backward => state.elapsed_ms.saturating_sub(delta_ms),
                };
                Self::seek_message_from_target_ms(state, target_ms)
            }
            MediaControlEvent::Seek(direction) => {
                let target_ms = match direction {
                    SeekDirection::Forward => state.elapsed_ms.saturating_add(SEEK_STEP_MS),
                    SeekDirection::Backward => state.elapsed_ms.saturating_sub(SEEK_STEP_MS),
                };
                Self::seek_message_from_target_ms(state, target_ms)
            }
            MediaControlEvent::SetVolume(_)
            | MediaControlEvent::OpenUri(_)
            | MediaControlEvent::Raise
            | MediaControlEvent::Quit => None,
        }
    }

    fn seek_message_from_target_ms(state: ControlState, target_ms: u64) -> Option<PlaybackMessage> {
        if state.total_ms == 0 {
            return None;
        }
        let clamped_ms = target_ms.min(state.total_ms);
        let percentage = (clamped_ms as f32 / state.total_ms as f32).clamp(0.0, 1.0);
        Some(PlaybackMessage::Seek(percentage))
    }

    fn update_control_state<F>(&self, update: F)
    where
        F: FnOnce(&mut ControlState),
    {
        match self.control_state.lock() {
            Ok(mut state) => update(&mut state),
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                update(&mut state);
            }
        }
    }

    fn control_state_snapshot(&self) -> ControlState {
        match self.control_state.lock() {
            Ok(state) => *state,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn desired_playback_publish_state(&self) -> PlaybackPublishState {
        if self.current_track_path.is_none() {
            return PlaybackPublishState::Stopped;
        }

        if self.control_state_snapshot().is_playing {
            PlaybackPublishState::Playing
        } else {
            PlaybackPublishState::Paused
        }
    }

    fn publish_playback_if_needed(&mut self) {
        let desired_state = self.desired_playback_publish_state();
        if self.last_published_playback == Some(desired_state) {
            return;
        }

        let Some(controls) = self.controls.as_mut() else {
            return;
        };

        let playback = match desired_state {
            PlaybackPublishState::Stopped => MediaPlayback::Stopped,
            PlaybackPublishState::Paused => MediaPlayback::Paused { progress: None },
            PlaybackPublishState::Playing => MediaPlayback::Playing { progress: None },
        };

        if let Err(err) = controls.set_playback(playback) {
            warn!(
                "MediaControlsManager: failed to publish playback state {:?}: {}",
                desired_state, err
            );
            return;
        }

        self.last_published_playback = Some(desired_state);
    }

    fn title_from_path(path: &Path) -> String {
        path.file_stem()
            .and_then(|name| name.to_str())
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| "Unknown Title".to_string())
    }

    fn publish_metadata_if_needed(&mut self) {
        let snapshot = self.control_state_snapshot();
        let track_path = self.current_track_path.clone();
        let total_ms = if track_path.is_some() {
            snapshot.total_ms
        } else {
            0
        };

        if self.last_published_metadata_track_path == track_path
            && self.last_published_metadata_total_ms == total_ms
        {
            return;
        }

        let Some(controls) = self.controls.as_mut() else {
            return;
        };

        let publish_result = if let Some(path) = track_path.as_ref() {
            let title = Self::title_from_path(path);
            let duration = (total_ms > 0).then(|| Duration::from_millis(total_ms));
            controls.set_metadata(MediaMetadata {
                title: Some(title.as_str()),
                artist: None,
                album: None,
                cover_url: None,
                duration,
            })
        } else {
            controls.set_metadata(MediaMetadata::default())
        };

        if let Err(err) = publish_result {
            warn!("MediaControlsManager: failed to publish metadata: {}", err);
            return;
        }

        self.last_published_metadata_track_path = track_path;
        self.last_published_metadata_total_ms = total_ms;
    }

    fn handle_message(&mut self, message: Message) {
        match message {
            Message::Playback(PlaybackMessage::Play) => {
                self.update_control_state(|state| state.is_playing = true);
                self.publish_playback_if_needed();
            }
            Message::Playback(PlaybackMessage::Pause) => {
                self.update_control_state(|state| state.is_playing = false);
                self.publish_playback_if_needed();
            }
            Message::Playback(PlaybackMessage::Stop) => {
                self.update_control_state(|state| {
                    state.is_playing = false;
                    state.elapsed_ms = 0;
                    state.total_ms = 0;
                });
                self.current_track_path = None;
                self.publish_playback_if_needed();
                self.publish_metadata_if_needed();
            }
            Message::Playback(PlaybackMessage::PlaybackProgress {
                elapsed_ms,
                total_ms,
            }) => {
                self.update_control_state(|state| {
                    state.elapsed_ms = elapsed_ms;
                    state.total_ms = total_ms;
                });
                self.publish_metadata_if_needed();
            }
            Message::Playlist(PlaylistMessage::PlaylistIndicesChanged {
                playing_track_path,
                is_playing,
                ..
            }) => {
                self.update_control_state(|state| {
                    state.is_playing = is_playing;
                    if playing_track_path.is_none() {
                        state.elapsed_ms = 0;
                        state.total_ms = 0;
                    }
                });
                self.current_track_path = playing_track_path;
                self.publish_playback_if_needed();
                self.publish_metadata_if_needed();
            }
            _ => {}
        }
    }

    /// Starts the blocking manager loop.
    pub fn run(&mut self) {
        info!("MediaControlsManager: started");
        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(message) => self.handle_message(message),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("MediaControlsManager: bus lagged by {} messages", skipped);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlState, MediaControlsManager};
    use crate::protocol::PlaybackMessage;
    use souvlaki::{MediaControlEvent, MediaPosition, SeekDirection};
    use std::time::Duration;

    fn assert_seek_message(message: Option<PlaybackMessage>, expected: f32) {
        match message {
            Some(PlaybackMessage::Seek(value)) => {
                assert!((value - expected).abs() < f32::EPSILON);
            }
            _ => panic!("expected PlaybackMessage::Seek"),
        }
    }

    #[test]
    fn test_toggle_event_pauses_when_currently_playing() {
        let state = ControlState {
            is_playing: true,
            elapsed_ms: 0,
            total_ms: 0,
        };
        let message = MediaControlsManager::map_control_event(MediaControlEvent::Toggle, state);
        assert!(matches!(message, Some(PlaybackMessage::Pause)));
    }

    #[test]
    fn test_toggle_event_plays_when_currently_paused() {
        let state = ControlState {
            is_playing: false,
            elapsed_ms: 0,
            total_ms: 0,
        };
        let message = MediaControlsManager::map_control_event(MediaControlEvent::Toggle, state);
        assert!(matches!(
            message,
            Some(PlaybackMessage::PlayActiveCollection)
        ));
    }

    #[test]
    fn test_set_position_event_maps_to_seek_percentage() {
        let state = ControlState {
            is_playing: true,
            elapsed_ms: 0,
            total_ms: 200_000,
        };
        let message = MediaControlsManager::map_control_event(
            MediaControlEvent::SetPosition(MediaPosition(Duration::from_millis(50_000))),
            state,
        );
        assert_seek_message(message, 0.25);
    }

    #[test]
    fn test_seek_by_forward_maps_to_seek_percentage() {
        let state = ControlState {
            is_playing: true,
            elapsed_ms: 80_000,
            total_ms: 200_000,
        };
        let message = MediaControlsManager::map_control_event(
            MediaControlEvent::SeekBy(SeekDirection::Forward, Duration::from_millis(20_000)),
            state,
        );
        assert_seek_message(message, 0.5);
    }

    #[test]
    fn test_seek_without_duration_is_ignored() {
        let state = ControlState {
            is_playing: true,
            elapsed_ms: 10_000,
            total_ms: 0,
        };
        let message = MediaControlsManager::map_control_event(
            MediaControlEvent::SeekBy(SeekDirection::Backward, Duration::from_millis(5_000)),
            state,
        );
        assert!(message.is_none());
    }
}
