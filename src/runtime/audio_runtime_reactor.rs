//! Runtime reactor that reconciles config, device, and session events for audio settings.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

use log::{debug, warn};
use tokio::sync::broadcast;

use crate::{
    config::Config,
    opensubsonic_controller::OPENSUBSONIC_PROFILE_ID,
    protocol::{self, ConfigMessage, IntegrationMessage, Message, PlaylistMessage},
    runtime_config::{
        config_diff_is_runtime_sample_rate_only, publish_runtime_config_delta,
        runtime_output_override_snapshot, update_last_runtime_config_snapshot,
        OutputRuntimeSignature, RuntimeAudioState, RuntimeOutputOverride, StagedAudioSettings,
    },
    AppWindow, OutputSettingsOptions,
};

/// Shared handles required by the runtime event reactor thread.
pub struct RuntimeEventReactorContext {
    /// Shared event bus producer.
    pub bus_sender: broadcast::Sender<Message>,
    /// Persisted config state snapshot.
    pub config_state: Arc<Mutex<Config>>,
    /// Detected output option inventory.
    pub output_options: Arc<Mutex<OutputSettingsOptions>>,
    /// Last known device inventory for change detection.
    pub output_device_inventory: Arc<Mutex<Vec<String>>>,
    /// Weak handle to the root UI.
    pub ui_handle: slint::Weak<AppWindow>,
    /// Workspace dimensions used for layout application.
    pub layout_workspace_size: Arc<Mutex<(u32, u32)>>,
    /// Runtime-only output overrides.
    pub runtime_output_override: Arc<Mutex<RuntimeOutputOverride>>,
    /// Runtime-acknowledged audio state.
    pub runtime_audio_state: Arc<Mutex<RuntimeAudioState>>,
    /// Pending staged settings awaiting runtime application.
    pub staged_audio_settings: Arc<Mutex<Option<StagedAudioSettings>>>,
    /// Last output signature applied at runtime.
    pub last_runtime_signature: Arc<Mutex<OutputRuntimeSignature>>,
    /// Last runtime config snapshot emitted to consumers.
    pub last_runtime_config: Arc<Mutex<Config>>,
    /// Playback activity flag used for low-disruption apply paths.
    pub playback_session_active: Arc<AtomicBool>,
}

/// Spawns the runtime event reactor thread and starts processing bus messages.
pub fn spawn_runtime_event_reactor(context: RuntimeEventReactorContext) {
    let RuntimeEventReactorContext {
        bus_sender,
        config_state,
        output_options,
        output_device_inventory,
        ui_handle,
        layout_workspace_size,
        runtime_output_override,
        runtime_audio_state,
        staged_audio_settings,
        last_runtime_signature,
        last_runtime_config,
        playback_session_active,
    } = context;

    let mut device_event_receiver = bus_sender.subscribe();
    let bus_sender_clone = bus_sender.clone();
    let mut runtime_sample_rate_switch_in_progress = false;
    thread::spawn(move || loop {
        match device_event_receiver.blocking_recv() {
            Ok(Message::Config(ConfigMessage::SetRuntimeOutputRate {
                sample_rate_hz,
                reason,
            })) => {
                {
                    let mut runtime_override = runtime_output_override
                        .lock()
                        .expect("runtime output override lock poisoned");
                    runtime_override.sample_rate_hz = Some(sample_rate_hz.clamp(8_000, 192_000));
                }
                debug!(
                    "Runtime output sample-rate override set to {} Hz ({})",
                    sample_rate_hz, reason
                );
                let persisted_config = {
                    let state = config_state.lock().expect("config state lock poisoned");
                    state.clone()
                };
                let options_snapshot = {
                    let options = output_options.lock().expect("output options lock poisoned");
                    options.clone()
                };
                let runtime_override = runtime_output_override_snapshot(&runtime_output_override);
                let runtime = crate::resolve_effective_runtime_config(
                    &persisted_config,
                    &options_snapshot,
                    Some(&runtime_override),
                    &runtime_audio_state,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                {
                    let mut last_signature = last_runtime_signature
                        .lock()
                        .expect("runtime signature lock poisoned");
                    *last_signature = runtime_signature;
                }
                update_last_runtime_config_snapshot(&last_runtime_config, runtime.clone());
                let _ = bus_sender_clone.send(Message::Config(
                    ConfigMessage::RuntimeOutputSampleRateChanged {
                        sample_rate_hz: runtime.output.sample_rate_khz,
                    },
                ));
                runtime_sample_rate_switch_in_progress = true;
            }
            Ok(Message::Config(ConfigMessage::ClearRuntimeOutputRateOverride)) => {
                {
                    let mut runtime_override = runtime_output_override
                        .lock()
                        .expect("runtime output override lock poisoned");
                    runtime_override.sample_rate_hz = None;
                }
                debug!("Runtime output sample-rate override cleared");
                let mut force_broadcast_runtime = false;
                if let Some(staged) = {
                    let mut pending = staged_audio_settings
                        .lock()
                        .expect("staged audio settings lock poisoned");
                    pending.take()
                } {
                    let mut runtime_audio = runtime_audio_state
                        .lock()
                        .expect("runtime audio state lock poisoned");
                    runtime_audio.output = staged.output;
                    runtime_audio.cast = staged.cast;
                    force_broadcast_runtime = true;
                }
                let persisted_config = {
                    let state = config_state.lock().expect("config state lock poisoned");
                    state.clone()
                };
                let options_snapshot = {
                    let options = output_options.lock().expect("output options lock poisoned");
                    options.clone()
                };
                let runtime_override = runtime_output_override_snapshot(&runtime_output_override);
                let runtime = crate::resolve_effective_runtime_config(
                    &persisted_config,
                    &options_snapshot,
                    Some(&runtime_override),
                    &runtime_audio_state,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                let previous_runtime = {
                    let previous = last_runtime_config
                        .lock()
                        .expect("last runtime config lock poisoned");
                    previous.clone()
                };
                let playback_active = playback_session_active.load(Ordering::Relaxed);
                let should_broadcast_runtime = {
                    let mut last_signature = last_runtime_signature
                        .lock()
                        .expect("runtime signature lock poisoned");
                    if !playback_active && !force_broadcast_runtime {
                        // Keep runtime metadata in sync without forcing an idle-time audio-device reopen.
                        *last_signature = runtime_signature;
                        false
                    } else if *last_signature == runtime_signature {
                        force_broadcast_runtime
                    } else {
                        *last_signature = runtime_signature;
                        true
                    }
                };
                if should_broadcast_runtime {
                    if config_diff_is_runtime_sample_rate_only(&previous_runtime, &runtime) {
                        update_last_runtime_config_snapshot(&last_runtime_config, runtime.clone());
                        let _ = bus_sender_clone.send(Message::Config(
                            ConfigMessage::RuntimeOutputSampleRateChanged {
                                sample_rate_hz: runtime.output.sample_rate_khz,
                            },
                        ));
                        runtime_sample_rate_switch_in_progress = true;
                    } else {
                        let _ = publish_runtime_config_delta(
                            &bus_sender_clone,
                            &last_runtime_config,
                            runtime,
                        );
                    }
                } else {
                    update_last_runtime_config_snapshot(&last_runtime_config, runtime);
                }
            }
            Ok(Message::Config(ConfigMessage::AudioDeviceOpened { .. })) => {
                if runtime_sample_rate_switch_in_progress {
                    runtime_sample_rate_switch_in_progress = false;
                    debug!("Skipping output options re-detection after runtime sample-rate switch");
                    continue;
                }
                let current_inventory = crate::snapshot_output_device_names(&cpal::default_host());
                let inventory_changed = {
                    let mut inventory = output_device_inventory
                        .lock()
                        .expect("output device inventory lock poisoned");
                    if *inventory == current_inventory {
                        false
                    } else {
                        *inventory = current_inventory;
                        true
                    }
                };
                if !inventory_changed {
                    debug!(
                        "Skipping output options re-detection after audio-device open: inventory unchanged"
                    );
                    continue;
                }
                let persisted_config = {
                    let state = config_state.lock().expect("config state lock poisoned");
                    state.clone()
                };
                let detected_options = crate::detect_output_settings_options(&persisted_config);
                {
                    let mut options = output_options.lock().expect("output options lock poisoned");
                    *options = detected_options.clone();
                }

                let _ = bus_sender_clone.send(Message::Config(
                    ConfigMessage::OutputDeviceCapabilitiesChanged {
                        verified_sample_rates: detected_options.verified_sample_rate_values.clone(),
                    },
                ));

                let runtime_override = runtime_output_override_snapshot(&runtime_output_override);
                let runtime = crate::resolve_effective_runtime_config(
                    &persisted_config,
                    &detected_options,
                    Some(&runtime_override),
                    &runtime_audio_state,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                let should_broadcast_runtime = {
                    let mut last_signature = last_runtime_signature
                        .lock()
                        .expect("runtime signature lock poisoned");
                    if *last_signature == runtime_signature {
                        false
                    } else {
                        *last_signature = runtime_signature;
                        true
                    }
                };
                if should_broadcast_runtime {
                    let _ = publish_runtime_config_delta(
                        &bus_sender_clone,
                        &last_runtime_config,
                        runtime,
                    );
                } else {
                    update_last_runtime_config_snapshot(&last_runtime_config, runtime);
                }

                let ui_weak = ui_handle.clone();
                let config_for_ui = persisted_config.clone();
                let options_for_ui = detected_options.clone();
                let (workspace_width_px, workspace_height_px) =
                    crate::workspace_size_snapshot(&layout_workspace_size);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        crate::apply_config_to_ui(
                            &ui,
                            &config_for_ui,
                            &options_for_ui,
                            workspace_width_px,
                            workspace_height_px,
                        );
                    }
                });
            }
            Ok(Message::Playlist(PlaylistMessage::RemoteDetachConfirmationRequested {
                playlist_id,
                playlist_name,
            })) => {
                let ui_weak = ui_handle.clone();
                let message = format!(
                    "This edit will detach '{}' from OpenSubsonic sync and keep it local-only. Continue?",
                    playlist_name
                );
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_remote_detach_target_playlist_id(playlist_id.into());
                        ui.set_remote_detach_confirm_message(message.into());
                        ui.set_show_remote_detach_confirm(true);
                    }
                });
            }
            Ok(Message::Playlist(PlaylistMessage::RemotePlaylistWritebackState {
                playlist_id,
                success,
                error,
            })) => {
                let ui_weak = ui_handle.clone();
                let status = if success {
                    format!("OpenSubsonic playlist sync complete ({playlist_id})")
                } else {
                    format!(
                        "OpenSubsonic playlist sync failed ({playlist_id}): {}",
                        error.unwrap_or_else(|| "unknown error".to_string())
                    )
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_subsonic_status(status.into());
                    }
                });
            }
            Ok(Message::Integration(IntegrationMessage::BackendSnapshotUpdated(snapshot))) => {
                let ui_weak = ui_handle.clone();
                let status = snapshot
                    .profiles
                    .iter()
                    .find(|profile| profile.profile_id == OPENSUBSONIC_PROFILE_ID)
                    .map(|profile| {
                        profile.status_text.clone().unwrap_or_else(|| {
                            match profile.connection_state {
                                protocol::BackendConnectionState::Connected => {
                                    "Connected".to_string()
                                }
                                protocol::BackendConnectionState::Connecting => {
                                    "Connecting...".to_string()
                                }
                                protocol::BackendConnectionState::Disconnected => {
                                    "Disconnected".to_string()
                                }
                                protocol::BackendConnectionState::Error => "Error".to_string(),
                            }
                        })
                    })
                    .unwrap_or_else(|| "Not configured".to_string());
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_subsonic_status(status.into());
                    }
                });
            }
            Ok(Message::Integration(IntegrationMessage::BackendOperationFailed {
                profile_id,
                action,
                error,
            })) => {
                if profile_id.as_deref() != Some(OPENSUBSONIC_PROFILE_ID) {
                    continue;
                }
                let ui_weak = ui_handle.clone();
                let status = format!("OpenSubsonic {action} failed: {error}");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_subsonic_status(status.into());
                    }
                });
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    "Main device-event listener lagged on control bus, skipped {} message(s)",
                    skipped
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    });
}
