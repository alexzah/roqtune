mod audio_decoder;
mod audio_player;
mod config;
mod db_manager;
mod playlist;
mod playlist_manager;
mod protocol;
mod ui_manager;

use std::{
    collections::BTreeSet,
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
};

use audio_decoder::AudioDecoder;
use audio_player::AudioPlayer;
use config::{BufferingConfig, Config, OutputConfig, UiConfig};
use cpal::traits::{DeviceTrait, HostTrait};
use db_manager::DbManager;
use log::{debug, info};
use playlist::Playlist;
use playlist_manager::PlaylistManager;
use protocol::{ConfigMessage, Message, PlaybackMessage, PlaylistMessage};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::broadcast;
use ui_manager::{UiManager, UiState};

slint::include_modules!();

fn setup_app_state_associations(ui: &AppWindow, ui_state: &UiState) {
    ui.set_track_model(ModelRc::from(ui_state.track_model.clone()));
}

fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "non-string panic payload".to_string()
}

#[derive(Debug, Clone)]
struct OutputSettingsOptions {
    channel_values: Vec<u16>,
    sample_rate_values: Vec<u32>,
    bits_per_sample_values: Vec<u16>,
}

fn filter_common_u16(detected: &BTreeSet<u16>, common_values: &[u16], fallback: u16) -> Vec<u16> {
    let mut filtered: Vec<u16> = common_values
        .iter()
        .copied()
        .filter(|value| detected.contains(value))
        .collect();
    if filtered.is_empty() {
        if detected.contains(&fallback) {
            filtered.push(fallback);
        } else if let Some(first_detected) = detected.iter().next().copied() {
            filtered.push(first_detected);
        } else {
            filtered.push(fallback);
        }
    }
    filtered
}

fn filter_common_u32(detected: &BTreeSet<u32>, common_values: &[u32], fallback: u32) -> Vec<u32> {
    let mut filtered: Vec<u32> = common_values
        .iter()
        .copied()
        .filter(|value| detected.contains(value))
        .collect();
    if filtered.is_empty() {
        if detected.contains(&fallback) {
            filtered.push(fallback);
        } else if let Some(first_detected) = detected.iter().next().copied() {
            filtered.push(first_detected);
        } else {
            filtered.push(fallback);
        }
    }
    filtered
}

fn detect_output_settings_options(config: &Config) -> OutputSettingsOptions {
    const COMMON_SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
    const COMMON_CHANNEL_COUNTS: [u16; 4] = [2, 1, 6, 8];
    const COMMON_BIT_DEPTHS: [u16; 3] = [16, 24, 32];
    const PROBE_SAMPLE_RATES: [u32; 13] = [
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000,
        176_400, 192_000,
    ];

    let mut channels = BTreeSet::new();
    let mut sample_rates = BTreeSet::new();
    let mut bits_per_sample = BTreeSet::new();

    let host = cpal::default_host();
    if let Some(device) = host.default_output_device() {
        if let Ok(configs) = device.supported_output_configs() {
            for output_config in configs {
                channels.insert(output_config.channels().max(1));
                bits_per_sample.insert((output_config.sample_format().sample_size() * 8) as u16);

                let min_rate = output_config.min_sample_rate().0;
                let max_rate = output_config.max_sample_rate().0;
                sample_rates.insert(min_rate);
                sample_rates.insert(max_rate);
                for probe_rate in PROBE_SAMPLE_RATES {
                    if probe_rate >= min_rate && probe_rate <= max_rate {
                        sample_rates.insert(probe_rate);
                    }
                }
            }
        }
    }

    if channels.is_empty() {
        channels.insert(config.output.channel_count.max(1));
    }
    if sample_rates.is_empty() {
        sample_rates.insert(config.output.sample_rate_khz.max(8_000));
    }
    if bits_per_sample.is_empty() {
        bits_per_sample.insert(config.output.bits_per_sample.max(8));
    }

    let channel_values = filter_common_u16(
        &channels,
        &COMMON_CHANNEL_COUNTS,
        config.output.channel_count.max(1),
    );
    let sample_rate_values = filter_common_u32(
        &sample_rates,
        &COMMON_SAMPLE_RATES,
        config.output.sample_rate_khz.max(8_000),
    );
    let bits_per_sample_values = filter_common_u16(
        &bits_per_sample,
        &COMMON_BIT_DEPTHS,
        config.output.bits_per_sample.max(8),
    );

    OutputSettingsOptions {
        channel_values,
        sample_rate_values,
        bits_per_sample_values,
    }
}

fn sanitize_config(config: Config) -> Config {
    let clamped_channels = config.output.channel_count.clamp(1, 8);
    let clamped_sample_rate_hz = config.output.sample_rate_khz.clamp(8_000, 192_000);
    let clamped_bits = config.output.bits_per_sample.clamp(8, 32);
    let clamped_low_watermark = config.buffering.player_low_watermark_ms.max(500);
    let clamped_target = config
        .buffering
        .player_target_buffer_ms
        .max(clamped_low_watermark.saturating_add(500))
        .clamp(1_000, 120_000);
    let clamped_interval = config.buffering.player_request_interval_ms.max(20);
    let clamped_decoder_chunk = config.buffering.decoder_request_chunk_ms.max(100);

    Config {
        output: OutputConfig {
            channel_count: clamped_channels,
            sample_rate_khz: clamped_sample_rate_hz,
            bits_per_sample: clamped_bits,
        },
        ui: UiConfig {
            show_album_art: config.ui.show_album_art,
        },
        buffering: BufferingConfig {
            player_low_watermark_ms: clamped_low_watermark,
            player_target_buffer_ms: clamped_target,
            player_request_interval_ms: clamped_interval,
            decoder_request_chunk_ms: clamped_decoder_chunk,
        },
    }
}

fn apply_config_to_ui(ui: &AppWindow, config: &Config, output_options: &OutputSettingsOptions) {
    ui.set_show_album_art(config.ui.show_album_art);

    let channel_options: Vec<slint::SharedString> = output_options
        .channel_values
        .iter()
        .map(|value| value.to_string().into())
        .collect();
    let sample_rate_options: Vec<slint::SharedString> = output_options
        .sample_rate_values
        .iter()
        .map(|value| format!("{} Hz", value).into())
        .collect();
    let bit_depth_options: Vec<slint::SharedString> = output_options
        .bits_per_sample_values
        .iter()
        .map(|value| format!("{} bit", value).into())
        .collect();

    let mut channel_options_with_other = channel_options;
    channel_options_with_other.push("Other...".into());
    let mut sample_rate_options_with_other = sample_rate_options;
    sample_rate_options_with_other.push("Other...".into());
    let mut bit_depth_options_with_other = bit_depth_options;
    bit_depth_options_with_other.push("Other...".into());

    ui.set_settings_channel_options(ModelRc::from(Rc::new(VecModel::from(
        channel_options_with_other,
    ))));
    ui.set_settings_sample_rate_options(ModelRc::from(Rc::new(VecModel::from(
        sample_rate_options_with_other,
    ))));
    ui.set_settings_bits_per_sample_options(ModelRc::from(Rc::new(VecModel::from(
        bit_depth_options_with_other,
    ))));

    let channel_index = output_options
        .channel_values
        .iter()
        .position(|value| *value == config.output.channel_count)
        .unwrap_or(output_options.channel_values.len());
    let sample_rate_index = output_options
        .sample_rate_values
        .iter()
        .position(|value| *value == config.output.sample_rate_khz)
        .unwrap_or(output_options.sample_rate_values.len());
    let bits_index = output_options
        .bits_per_sample_values
        .iter()
        .position(|value| *value == config.output.bits_per_sample)
        .unwrap_or(output_options.bits_per_sample_values.len());

    ui.set_settings_channel_index(channel_index as i32);
    ui.set_settings_sample_rate_index(sample_rate_index as i32);
    ui.set_settings_bits_per_sample_index(bits_index as i32);
    ui.set_settings_channel_custom_value(config.output.channel_count.to_string().into());
    ui.set_settings_sample_rate_custom_value(config.output.sample_rate_khz.to_string().into());
    ui.set_settings_bits_per_sample_custom_value(config.output.bits_per_sample.to_string().into());
    ui.set_settings_show_album_art(config.ui.show_album_art);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut clog = colog::default_builder();
    clog.filter(None, log::LevelFilter::Debug);
    clog.init();

    std::panic::set_hook(Box::new(|panic_info| {
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("unnamed");
        log::error!("panic in thread '{}': {}", thread_name, panic_info);
    }));

    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
        info!("SLINT_BACKEND not set. Defaulting to winit-software");
    }

    // Setup ui state
    let ui = AppWindow::new()?;

    let ui_state = UiState {
        track_model: Rc::new(VecModel::from(vec![])),
    };

    let config_dir = dirs::config_dir().unwrap();
    let config_file = config_dir.join("music_player.toml");

    if !config_file.exists() {
        let default_config = Config::default();

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

    let config_content = std::fs::read_to_string(config_file.clone()).unwrap();
    let config = sanitize_config(toml::from_str::<Config>(&config_content).unwrap_or_default());
    let output_options = Arc::new(detect_output_settings_options(&config));

    setup_app_state_associations(&ui, &ui_state);
    apply_config_to_ui(&ui, &config, output_options.as_ref());
    let config_state = Arc::new(Mutex::new(config.clone()));

    // Bus for communication between components
    let (bus_sender, _) = broadcast::channel(1024);

    let bus_sender_clone = bus_sender.clone();

    // Setup file dialog
    ui.on_open_file(move || {
        debug!("Opening file dialog");
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Audio Files", &["mp3", "wav", "ogg", "flac"])
            .pick_files()
        {
            for path in paths {
                debug!("Sending load track message for {:?}", path);
                let _ = bus_sender_clone.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::LoadTrack(path),
                ));
            }
        }
    });

    // Wire up play button
    let bus_sender_clone = bus_sender.clone();
    ui.on_play(move || {
        debug!("Play button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Play));
    });

    // Wire up stop button
    let bus_sender_clone = bus_sender.clone();
    ui.on_stop(move || {
        debug!("Stop button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Stop));
    });

    // Wire up next button
    let bus_sender_clone = bus_sender.clone();
    ui.on_next(move || {
        debug!("Next button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Next));
    });

    // Wire up previous button
    let bus_sender_clone = bus_sender.clone();
    ui.on_previous(move || {
        debug!("Previous button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Previous));
    });

    // Wire up pause button
    let bus_sender_clone = bus_sender.clone();
    ui.on_pause(move || {
        debug!("Pause button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Pause));
    });

    // Handle track click from overlay
    let bus_sender_clone = bus_sender.clone();
    ui.on_handle_track_click(move |index, ctrl, shift| {
        debug!(
            "Track clicked at index {:?} (ctrl={}, shift={})",
            index, ctrl, shift
        );
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::SelectTrackMulti {
            index: index as usize,
            ctrl,
            shift,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_deselect_all(move || {
        debug!("Deselect all requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeselectAll));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_item_double_click(move |index| {
        debug!("Playlist item double-clicked: {}", index);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::PlayTrackByIndex(
            index as usize,
        )));
        ui_handle_clone
            .upgrade()
            .unwrap()
            .set_selected_track_index(index);
        ui_handle_clone
            .upgrade()
            .unwrap()
            .set_playing_track_index(index);
    });

    // Wire up delete track handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_delete_selected_tracks(move || {
        debug!("Delete selected tracks requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeleteSelected));
    });

    // Wire up reorder track handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_reorder_tracks(move |indices, to| {
        let indices_vec: Vec<usize> = indices.iter().map(|i| i as usize).collect();
        debug!("Reorder tracks requested: from {:?} to {}", indices_vec, to);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ReorderTracks {
            indices: indices_vec,
            to: to as usize,
        }));
    });

    // Wire up pointer down handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_on_pointer_down(move |index, ctrl, shift| {
        debug!(
            "Pointer down at index {:?} (ctrl={}, shift={})",
            index, ctrl, shift
        );
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnPointerDown {
            index: index as usize,
            ctrl,
            shift,
        }));
    });

    // Wire up drag start handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_on_drag_start(move |pressed_index| {
        debug!("Drag start at index {:?}", pressed_index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragStart {
            pressed_index: pressed_index as usize,
        }));
    });

    // Wire up drag move handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_on_drag_move(move |drop_gap| {
        debug!("Drag move to gap {:?}", drop_gap);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragMove {
            drop_gap: drop_gap as usize,
        }));
    });

    // Wire up drag end handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_on_drag_end(move |drop_gap| {
        debug!("Drag end at gap {:?}", drop_gap);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragEnd {
            drop_gap: drop_gap as usize,
        }));
    });

    // Wire up sequence selector
    let bus_sender_clone = bus_sender.clone();
    ui.on_playback_order_changed(move |index| {
        debug!("Playback order changed: {}", index);
        match index {
            0 => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Default),
                ));
            }
            1 => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Shuffle),
                ));
            }
            2 => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Random),
                ));
            }
            _ => {
                debug!("Invalid playback order index: {}", index);
            }
        }
    });

    // Wire up repeat toggle
    let bus_sender_clone = bus_sender.clone();
    ui.on_toggle_repeat(move || {
        debug!("Repeat toggle clicked");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ToggleRepeat));
    });

    // Wire up seek handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_seek_to(move |percentage| {
        debug!("Seek requested to {}%", percentage * 100.0);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Seek(percentage)));
    });

    // Wire up volume handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_volume_changed(move |volume| {
        let clamped = volume.clamp(0.0, 1.0);
        debug!("Volume changed to {}%", clamped * 100.0);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::SetVolume(clamped)));
    });

    // Wire up settings open handler
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_open_settings(move || {
        let current_config = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(&ui, &current_config, output_options_clone.as_ref());
            ui.set_show_settings_dialog(true);
        }
    });

    // Wire up settings apply handler
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_apply_settings(
        move |channel_index,
              sample_rate_index,
              bits_per_sample_index,
              show_album_art,
              channel_custom_value,
              sample_rate_custom_value,
              bits_custom_value| {
            let previous_config = {
                let state = config_state_clone
                    .lock()
                    .expect("config state lock poisoned");
                state.clone()
            };

            let channel_idx = channel_index.max(0) as usize;
            let sample_rate_idx = sample_rate_index.max(0) as usize;
            let bits_idx = bits_per_sample_index.max(0) as usize;

            let channel_count = if channel_idx < output_options_clone.channel_values.len() {
                output_options_clone.channel_values[channel_idx]
            } else {
                channel_custom_value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(previous_config.output.channel_count)
            };
            let sample_rate_hz = if sample_rate_idx < output_options_clone.sample_rate_values.len() {
                output_options_clone.sample_rate_values[sample_rate_idx]
            } else {
                sample_rate_custom_value
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(previous_config.output.sample_rate_khz)
            };
            let bits_per_sample = if bits_idx < output_options_clone.bits_per_sample_values.len() {
                output_options_clone.bits_per_sample_values[bits_idx]
            } else {
                bits_custom_value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(previous_config.output.bits_per_sample)
            };

            let next_config = sanitize_config(Config {
                output: OutputConfig {
                    channel_count,
                    sample_rate_khz: sample_rate_hz,
                    bits_per_sample,
                },
                ui: UiConfig { show_album_art },
                buffering: previous_config.buffering.clone(),
            });

            let mut restart_required_changes = Vec::new();
            if previous_config.output.channel_count != next_config.output.channel_count {
                restart_required_changes.push("output channels");
            }
            if previous_config.output.sample_rate_khz != next_config.output.sample_rate_khz {
                restart_required_changes.push("output sample rate");
            }
            if previous_config.output.bits_per_sample != next_config.output.bits_per_sample {
                restart_required_changes.push("output bit depth");
            }

            if let Some(ui) = ui_handle_clone.upgrade() {
                apply_config_to_ui(&ui, &next_config, output_options_clone.as_ref());
            }

            {
                let mut state = config_state_clone
                    .lock()
                    .expect("config state lock poisoned");
                *state = next_config.clone();
            }

            match toml::to_string(&next_config) {
                Ok(config_text) => {
                    if let Err(err) = std::fs::write(&config_file_clone, config_text) {
                        log::error!(
                            "Failed to persist config to {}: {}",
                            config_file_clone.display(),
                            err
                        );
                    }
                }
                Err(err) => {
                    log::error!("Failed to serialize config: {}", err);
                }
            }

            let mut runtime_config = next_config.clone();
            if !restart_required_changes.is_empty() {
                runtime_config.output = previous_config.output;
            }
            let _ =
                bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(runtime_config)));

            if !restart_required_changes.is_empty() {
                let formatted = restart_required_changes.join(", ");
                let message = format!(
                    "Saved changes to {}. These changes will apply after restarting the application.",
                    formatted
                );
                if let Some(ui) = ui_handle_clone.upgrade() {
                    ui.set_settings_restart_notice_message(message.into());
                    ui.set_show_settings_restart_notice(true);
                }
            }
        },
    );

    // Wire up playlist management
    let bus_sender_clone = bus_sender.clone();
    ui.on_create_playlist(move || {
        debug!("Create playlist requested");
        // For now let's just create one with a default name.
        // We could add a dialog later if the framework supports it easily or just prompt in CLI.
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CreatePlaylist {
            name: "New Playlist".to_string(),
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_switch_playlist(move |index| {
        debug!("Switch playlist requested: {}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::SwitchPlaylistByIndex(
            index as usize,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_rename_playlist(move |index, name| {
        debug!("Rename playlist requested: index={}, name={}", index, name);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::RenamePlaylistByIndex(
            index as usize,
            name.to_string(),
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_delete_playlist(move |index| {
        debug!("Delete playlist requested: index={}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeletePlaylistByIndex(
            index as usize,
        )));
    });

    // Setup playlist manager
    let playlist_manager_bus_receiver = bus_sender.subscribe();
    let playlist_manager_bus_sender = bus_sender.clone();
    let db_manager = DbManager::new().expect("Failed to initialize database");
    thread::spawn(move || {
        let mut playlist_manager = PlaylistManager::new(
            Playlist::new(),
            playlist_manager_bus_receiver,
            playlist_manager_bus_sender,
            db_manager,
        );
        playlist_manager.run();
    });

    // Setup UI manager
    let ui_manager_bus_sender = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    thread::spawn(move || {
        let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ui_manager = UiManager::new(
                ui_handle_clone,
                ui_manager_bus_sender.subscribe(),
                ui_manager_bus_sender.clone(),
            );
            ui_manager.run();
        }));
        if let Err(payload) = run_result {
            log::error!(
                "UiManager thread terminated due to panic: {}",
                panic_payload_to_string(payload.as_ref())
            );
        }
    });

    // Setup AudioDecoder
    let decoder_bus_sender = bus_sender.clone();
    let decoder_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_decoder = AudioDecoder::new(decoder_bus_receiver, decoder_bus_sender);
        audio_decoder.run();
    });

    // Setup AudioPlayer
    let player_bus_sender = bus_sender.clone();
    let player_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_player = AudioPlayer::new(player_bus_receiver, player_bus_sender);
        audio_player.run();
    });

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(config)));

    ui.run()?;

    info!("Application exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_focus_touch_areas_are_not_full_pane_overlays() {
        let slint_ui = include_str!("music_player.slint");

        assert!(
            !slint_ui.contains("sidebar-ta := TouchArea"),
            "Sidebar-wide focus TouchArea should not exist"
        );
        assert!(
            !slint_ui.contains("Rectangle {\n                horizontal-stretch: 4;\n                background: #282828;\n                \n                TouchArea {"),
            "Main-pane-wide focus TouchArea should not exist"
        );
        assert!(
            slint_ui.contains("root.sidebar_has_focus = false;"),
            "Track list interactions should clear sidebar focus"
        );
    }

    #[test]
    fn test_playlist_null_column_and_empty_space_trigger_deselect() {
        let slint_ui = include_str!("music_player.slint");

        assert!(
            slint_ui.contains("property <length> null-column-width: 120px;"),
            "App window should define a null column width"
        );
        assert!(
            slint_ui.contains("property <bool> in-null-column: self.mouse-x >= (self.width - root.null-column-width);"),
            "Track overlay should detect clicks in null column"
        );
        assert!(
            slint_ui.contains(
                "property <bool> is-in-rows: raw-row >= 0 && raw-row < root.track_model.length;"
            ),
            "Track overlay should detect when click is outside rows"
        );
        assert!(
            slint_ui.contains("root.deselect_all();"),
            "Track overlay should deselect on null-column or empty-space clicks"
        );
    }
}
