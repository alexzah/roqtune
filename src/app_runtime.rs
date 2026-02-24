//! Application runtime bootstrap and top-level orchestration.

use std::{
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

use log::{info, warn};
use slint::{ComponentHandle, LogicalSize, VecModel};
use tokio::sync::broadcast;

use crate::{
    app_bootstrap::services::{spawn_background_services, BackgroundServicesConfig},
    app_callbacks::bus_forwarding::{
        register_bus_forwarding_callbacks, BusForwardingCallbacksContext,
    },
    app_context::{AppSharedState, PersistencePaths, RuntimeConfigHandles, UiHandles},
    audio_runtime_reactor::{spawn_runtime_event_reactor, RuntimeEventReactorContext},
    config::Config,
    config_persistence::{
        hydrate_ui_columns_from_layout, load_layout_file, persist_state_files,
        system_layout_template_text,
    },
    opensubsonic_controller::{
        find_opensubsonic_backend, keyring_unavailable_error, opensubsonic_profile_snapshot,
        resolve_opensubsonic_password, OpenSubsonicPasswordResolution, OPENSUBSONIC_PROFILE_ID,
    },
    output_option_selection::bootstrap_output_settings_options,
    protocol::{
        CastMessage, ConfigMessage, IntegrationMessage, Message, PlaybackMessage, PlaylistMessage,
    },
    runtime_config::{
        OutputRuntimeSignature, RuntimeAudioState, RuntimeOutputOverride, StagedAudioSettings,
    },
    setup_app_state_associations, sidebar_width_from_window,
    ui_manager::UiState,
    AppWindow,
};

/// Owns startup wiring and launches the running Slint application instance.
pub(crate) struct AppRuntime {
    ui: AppWindow,
    config_state: Arc<Mutex<Config>>,
    config_file: PathBuf,
    layout_file: PathBuf,
}

impl AppRuntime {
    /// Builds the runtime by loading config/layout state and wiring all services/callbacks.
    pub(crate) fn build() -> Result<Self, Box<dyn std::error::Error>> {
        let configured_backend = std::env::var("SLINT_BACKEND").unwrap_or_else(|_| {
            info!("SLINT_BACKEND not set. Defaulting to winit-software");
            "winit-software".to_string()
        });
        #[cfg(target_os = "windows")]
        info!("Windows build: Slint accessibility feature is disabled");
        let backend_selector =
            slint::BackendSelector::new().backend_name(configured_backend.clone());
        backend_selector
            .select()
            .map_err(|err| format!("Failed to initialize Slint backend: {}", err))?;

        let ui = AppWindow::new()?;
        let ui_state = UiState {
            track_model: Rc::new(VecModel::from(vec![])),
        };

        let config_root = dirs::config_dir().unwrap().join("roqtune");
        let config_file = config_root.join("config.toml");
        let layout_file = config_root.join("layout.toml");

        if let Err(err) = std::fs::create_dir_all(&config_root) {
            return Err(format!(
                "Failed to create config directory {}: {}",
                config_root.display(),
                err
            )
            .into());
        }

        if !config_file.exists() {
            let default_config = crate::sanitize_config(Config::default());
            info!(
                "Config file not found. Creating default config. path={}",
                config_file.display()
            );
            std::fs::write(
                config_file.clone(),
                toml::to_string(&default_config).unwrap(),
            )
            .unwrap();
        }

        if !layout_file.exists() {
            info!(
                "Layout file not found. Creating default layout file. path={}",
                layout_file.display()
            );
            std::fs::write(layout_file.clone(), system_layout_template_text()).unwrap();
        }

        let config_content = std::fs::read_to_string(config_file.clone()).unwrap();
        let mut config =
            crate::sanitize_config(toml::from_str::<Config>(&config_content).unwrap_or_default());
        config.ui.layout = load_layout_file(&layout_file);
        hydrate_ui_columns_from_layout(&mut config);
        let config = crate::sanitize_config(config);
        // Use a config-only snapshot so startup never blocks on hardware probe.
        // Full device capability probing is refreshed asynchronously at runtime.
        let initial_output_options = bootstrap_output_settings_options(&config);
        let runtime_output_override = Arc::new(Mutex::new(RuntimeOutputOverride::default()));
        let runtime_override_snapshot = {
            let state = runtime_output_override
                .lock()
                .expect("runtime output override lock poisoned");
            state.clone()
        };
        let runtime_config = crate::resolve_runtime_config(
            &config,
            &initial_output_options,
            Some(&runtime_override_snapshot),
        );
        crate::image_pipeline::configure_runtime_limits(
            runtime_config.library.list_image_max_edge_px,
            runtime_config.library.cover_art_cache_max_size_mb,
            runtime_config.library.artist_image_cache_max_size_mb,
        );
        let runtime_audio_state = Arc::new(Mutex::new(RuntimeAudioState {
            output: config.output.clone(),
            cast: config.cast.clone(),
        }));

        ui.window().set_size(LogicalSize::new(
            config.ui.window_width as f32,
            config.ui.window_height as f32,
        ));
        ui.set_sidebar_width_px(sidebar_width_from_window(config.ui.window_width));
        let initial_workspace_width_px = config.ui.window_width.max(1);
        let initial_workspace_height_px = config.ui.window_height.max(1);

        setup_app_state_associations(&ui, &ui_state);
        crate::apply_config_to_ui(
            &ui,
            &config,
            &initial_output_options,
            initial_workspace_width_px,
            initial_workspace_height_px,
        );

        let config_state = Arc::new(Mutex::new(config.clone()));
        let output_options = Arc::new(Mutex::new(initial_output_options.clone()));
        // Start empty so the first runtime inventory snapshot always registers as a change.
        let output_device_inventory = Arc::new(Mutex::new(Vec::new()));
        let layout_workspace_size = Arc::new(Mutex::new((
            initial_workspace_width_px,
            initial_workspace_height_px,
        )));
        let layout_undo_stack: Arc<Mutex<Vec<crate::layout::LayoutConfig>>> =
            Arc::new(Mutex::new(Vec::new()));
        let layout_redo_stack: Arc<Mutex<Vec<crate::layout::LayoutConfig>>> =
            Arc::new(Mutex::new(Vec::new()));
        let last_runtime_config = Arc::new(Mutex::new(runtime_config.clone()));
        let last_runtime_signature = Arc::new(Mutex::new(OutputRuntimeSignature::from_output(
            &runtime_config.output,
        )));

        let (bus_sender, _) = broadcast::channel(8192);
        let playback_session_active = Arc::new(AtomicBool::new(false));
        let staged_audio_settings: Arc<Mutex<Option<StagedAudioSettings>>> =
            Arc::new(Mutex::new(None));
        let opensubsonic_session_passwords: Arc<Mutex<HashMap<String, String>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (playlist_bulk_import_tx, playlist_bulk_import_rx) =
            mpsc::sync_channel::<crate::protocol::PlaylistBulkImportRequest>(64);
        let (library_scan_progress_tx, library_scan_progress_rx) =
            mpsc::sync_channel::<crate::protocol::LibraryMessage>(512);
        let shared_state = AppSharedState {
            bus_sender: bus_sender.clone(),
            config_state: Arc::clone(&config_state),
            ui_handles: UiHandles {
                ui_handle: ui.as_weak().clone(),
                layout_workspace_size: Arc::clone(&layout_workspace_size),
            },
            runtime_handles: RuntimeConfigHandles {
                output_options: Arc::clone(&output_options),
                runtime_output_override: Arc::clone(&runtime_output_override),
                runtime_audio_state: Arc::clone(&runtime_audio_state),
                staged_audio_settings: Arc::clone(&staged_audio_settings),
                last_runtime_signature: Arc::clone(&last_runtime_signature),
                last_runtime_config: Arc::clone(&last_runtime_config),
            },
            persistence_paths: PersistencePaths {
                config_file: config_file.clone(),
            },
            playback_session_active: Arc::clone(&playback_session_active),
            opensubsonic_session_passwords: Arc::clone(&opensubsonic_session_passwords),
            layout_undo_stack: Arc::clone(&layout_undo_stack),
            layout_redo_stack: Arc::clone(&layout_redo_stack),
            playlist_bulk_import_tx: playlist_bulk_import_tx.clone(),
        };

        {
            let mut playback_state_receiver = bus_sender.subscribe();
            let playback_session_active_clone = Arc::clone(&playback_session_active);
            thread::spawn(move || loop {
                match playback_state_receiver.blocking_recv() {
                    Ok(Message::Playlist(PlaylistMessage::PlaylistIndicesChanged {
                        is_playing,
                        playing_index,
                        ..
                    })) => {
                        playback_session_active_clone
                            .store(is_playing || playing_index.is_some(), Ordering::Relaxed);
                    }
                    Ok(Message::Playback(PlaybackMessage::TrackStarted(_)))
                    | Ok(Message::Playback(PlaybackMessage::Play))
                    | Ok(Message::Playback(PlaybackMessage::PlayActiveCollection)) => {
                        playback_session_active_clone.store(true, Ordering::Relaxed);
                    }
                    Ok(Message::Playback(PlaybackMessage::Stop)) => {
                        playback_session_active_clone.store(false, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            "Main playback-state listener lagged on control bus, skipped {} message(s)",
                            skipped
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            });
        }

        crate::app_callbacks::imports_library::register_imports_library_callbacks(
            &ui,
            &shared_state,
        );
        register_bus_forwarding_callbacks(
            &ui,
            BusForwardingCallbacksContext {
                bus_sender: bus_sender.clone(),
                ui_handle: ui.as_weak().clone(),
            },
        );
        crate::app_callbacks::subsonic_settings::register_subsonic_settings_callbacks(
            &ui,
            &shared_state,
        );
        crate::app_callbacks::playlist_editing::register_playlist_editing_callbacks(
            &ui,
            &shared_state,
        );
        crate::app_callbacks::playlist_columns::register_playlist_column_callbacks(
            &ui,
            &shared_state,
        );
        crate::app_callbacks::layout_editor::register_layout_editor_callbacks(&ui, &shared_state);
        crate::app_callbacks::settings_ui::register_settings_ui_callbacks(&ui, &shared_state);

        let mut startup_subsonic_session_prompt: Option<(String, String, String)> = None;
        let startup_opensubsonic_seed = find_opensubsonic_backend(&config).map(|backend| {
            let (password, status_text) = match resolve_opensubsonic_password(
                OPENSUBSONIC_PROFILE_ID,
                &opensubsonic_session_passwords,
            ) {
                OpenSubsonicPasswordResolution::Saved(password) => (
                    Some(password),
                    Some("Restored from credential store".to_string()),
                ),
                OpenSubsonicPasswordResolution::SessionOnly(password) => (
                    Some(password),
                    Some("Using session-only credential".to_string()),
                ),
                OpenSubsonicPasswordResolution::Missing => {
                    let status = if backend.enabled {
                        "Missing saved password".to_string()
                    } else {
                        "Restored from config".to_string()
                    };
                    (None, Some(status))
                }
                OpenSubsonicPasswordResolution::KeyringError(error) => {
                    warn!(
                        "Failed to load OpenSubsonic credential from credential store: {}",
                        error
                    );
                    if backend.enabled
                        && !backend.username.trim().is_empty()
                        && !backend.endpoint.trim().is_empty()
                        && keyring_unavailable_error(error.as_str())
                    {
                        startup_subsonic_session_prompt = Some((
                            backend.username.clone(),
                            backend.endpoint.clone(),
                            "System keyring is unavailable. Enter your password for this session."
                                .to_string(),
                        ));
                    }
                    (
                        None,
                        Some("System keyring unavailable; session password required".to_string()),
                    )
                }
            };
            let snapshot = opensubsonic_profile_snapshot(backend, status_text);
            let connect_now = backend.enabled && password.is_some();
            (snapshot, password, connect_now)
        });
        spawn_background_services(BackgroundServicesConfig {
            bus_sender: bus_sender.clone(),
            ui_handle: ui.as_weak().clone(),
            initial_online_metadata_enabled: config.library.online_metadata_enabled,
            initial_online_metadata_prompt_pending: config.library.online_metadata_prompt_pending,
            playlist_bulk_import_rx,
            library_scan_progress_tx,
            library_scan_progress_rx,
            startup_opensubsonic_seed,
        });

        let _ = bus_sender.send(Message::Integration(IntegrationMessage::RequestSnapshot));

        spawn_runtime_event_reactor(RuntimeEventReactorContext {
            bus_sender: bus_sender.clone(),
            config_state: Arc::clone(&config_state),
            output_options: Arc::clone(&output_options),
            output_device_inventory: Arc::clone(&output_device_inventory),
            ui_handle: ui.as_weak().clone(),
            layout_workspace_size: Arc::clone(&layout_workspace_size),
            runtime_output_override: Arc::clone(&runtime_output_override),
            runtime_audio_state: Arc::clone(&runtime_audio_state),
            staged_audio_settings: Arc::clone(&staged_audio_settings),
            last_runtime_signature: Arc::clone(&last_runtime_signature),
            last_runtime_config: Arc::clone(&last_runtime_config),
            playback_session_active: Arc::clone(&playback_session_active),
        });

        // Playlist columns are global layout state from `layout.toml`; startup must not request
        // playlist-scoped column ordering.
        let _ = bus_sender.send(Message::Config(ConfigMessage::ConfigLoaded(
            runtime_config.clone(),
        )));
        let _ = bus_sender.send(Message::Config(
            ConfigMessage::OutputDeviceCapabilitiesChanged {
                verified_sample_rates: initial_output_options.verified_sample_rate_values.clone(),
            },
        ));
        let _ = bus_sender.send(Message::Playback(PlaybackMessage::SetVolume(
            config.ui.volume,
        )));
        let _ = bus_sender.send(Message::Cast(CastMessage::DiscoverDevices));

        if let Some((username, endpoint, status)) = startup_subsonic_session_prompt {
            ui.set_subsonic_session_prompt_username(username.into());
            ui.set_subsonic_session_prompt_endpoint(endpoint.into());
            ui.set_subsonic_session_prompt_password("".into());
            ui.set_subsonic_session_prompt_status(status.into());
            ui.set_show_subsonic_session_password_prompt(true);
        }

        Ok(Self {
            ui,
            config_state,
            config_file,
            layout_file,
        })
    }

    /// Starts the UI event loop after all runtime services are registered.
    pub(crate) fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        self.ui.run()?;

        let final_config = {
            let state = self
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        persist_state_files(&final_config, &self.config_file, &self.layout_file);

        info!("Application exiting");
        Ok(())
    }
}
