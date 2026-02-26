//! Callback registration for settings dialog, tooltip, and window-size UI events.

use std::{
    rc::Rc,
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};

use log::debug;
use slint::{Model, ModelRc, VecModel};

use crate::{
    app_context::AppSharedState,
    config::{
        CastConfig, Config, OutputConfig, ResamplerQuality, UiConfig, UiPlaybackOrder, UiRepeatMode,
    },
    config_persistence::persist_state_files_with_config_path,
    protocol::{self, Message, PlaybackMessage, PlaylistMessage},
    runtime_config::{
        audio_settings_changed, output_preferences_changed, publish_runtime_config_delta,
        runtime_output_override_snapshot, OutputRuntimeSignature, StagedAudioSettings,
    },
    AppWindow,
};

fn shared_string_model_to_vec(model: ModelRc<slint::SharedString>) -> Vec<String> {
    (0..model.row_count())
        .filter_map(|index| model.row_data(index))
        .map(|value| value.to_string())
        .collect()
}

fn set_custom_color_draft_preview_colors(ui: &AppWindow) {
    let draft_values = shared_string_model_to_vec(ui.get_settings_custom_color_draft_values());
    let saved_values = shared_string_model_to_vec(ui.get_settings_custom_color_values());
    let preview_len = draft_values.len().max(saved_values.len());
    let fallback_color = slint::Color::from_rgb_u8(42, 109, 239);
    let previews: Vec<slint::Color> = (0..preview_len)
        .map(|index| {
            draft_values
                .get(index)
                .and_then(|value| crate::theme::parse_slint_color(value))
                .or_else(|| {
                    saved_values
                        .get(index)
                        .and_then(|value| crate::theme::parse_slint_color(value))
                })
                .unwrap_or(fallback_color)
        })
        .collect();
    ui.set_settings_custom_color_draft_preview_colors(ModelRc::from(Rc::new(VecModel::from(
        previews,
    ))));
}

fn set_custom_color_draft_value(ui: &AppWindow, index: usize, value: String) {
    let mut draft_values = shared_string_model_to_vec(ui.get_settings_custom_color_draft_values());
    if index >= draft_values.len() {
        return;
    }
    draft_values[index] = value;
    let next_values: Vec<slint::SharedString> = draft_values
        .into_iter()
        .map(slint::SharedString::from)
        .collect();
    ui.set_settings_custom_color_draft_values(ModelRc::from(Rc::new(VecModel::from(next_values))));
    set_custom_color_draft_preview_colors(ui);
}

fn set_custom_color_draft_values(ui: &AppWindow, values: &[String]) {
    let values_shared: Vec<slint::SharedString> = values
        .iter()
        .map(|value| slint::SharedString::from(value.as_str()))
        .collect();
    ui.set_settings_custom_color_draft_values(ModelRc::from(Rc::new(VecModel::from(
        values_shared,
    ))));
    set_custom_color_draft_preview_colors(ui);
}

/// Registers callbacks that mutate persisted settings and runtime audio/UI state.
pub(crate) fn register_settings_ui_callbacks(ui: &AppWindow, shared_state: &AppSharedState) {
    let bus_sender_clone = shared_state.bus_sender.clone();
    let config_state_clone = shared_state.config_state.clone();
    let config_file_clone = shared_state.persistence_paths.config_file.clone();
    ui.on_playback_order_changed(move |index| {
        debug!("Playback order changed: {}", index);
        let next_order = match index {
            0 => Some(UiPlaybackOrder::Default),
            1 => Some(UiPlaybackOrder::Shuffle),
            2 => Some(UiPlaybackOrder::Random),
            _ => None,
        };
        let Some(next_order) = next_order else {
            debug!("Invalid playback order index: {}", index);
            return;
        };
        match next_order {
            UiPlaybackOrder::Default => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Default),
                ));
            }
            UiPlaybackOrder::Shuffle => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Shuffle),
                ));
            }
            UiPlaybackOrder::Random => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Random),
                ));
            }
        }

        let should_persist = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if state.ui.playback_order == next_order {
                false
            } else {
                state.ui.playback_order = next_order;
                true
            }
        };
        if should_persist {
            let snapshot = {
                let state = config_state_clone
                    .lock()
                    .expect("config state lock poisoned");
                state.clone()
            };
            persist_state_files_with_config_path(&snapshot, &config_file_clone);
        }
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let config_state_clone = shared_state.config_state.clone();
    let config_file_clone = shared_state.persistence_paths.config_file.clone();
    ui.on_toggle_repeat(move || {
        debug!("Repeat toggle clicked");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ToggleRepeat));

        let should_persist = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let next_repeat_mode = match state.ui.repeat_mode {
                UiRepeatMode::Off => UiRepeatMode::Playlist,
                UiRepeatMode::Playlist => UiRepeatMode::Track,
                UiRepeatMode::Track => UiRepeatMode::Off,
            };
            if state.ui.repeat_mode == next_repeat_mode {
                false
            } else {
                state.ui.repeat_mode = next_repeat_mode;
                true
            }
        };
        if should_persist {
            let snapshot = {
                let state = config_state_clone
                    .lock()
                    .expect("config state lock poisoned");
                state.clone()
            };
            persist_state_files_with_config_path(&snapshot, &config_file_clone);
        }
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let config_state_clone = shared_state.config_state.clone();
    ui.on_volume_changed(move |volume| {
        let clamped = volume.clamp(0.0, 1.0);
        debug!("Volume changed to {}%", clamped * 100.0);
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if (state.ui.volume - clamped).abs() > f32::EPSILON {
                state.ui.volume = clamped;
            }
        }
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::SetVolume(clamped)));
    });

    let config_state_clone = shared_state.config_state.clone();
    let output_options_clone = shared_state.runtime_handles.output_options.clone();
    let layout_workspace_size_clone = shared_state.ui_handles.layout_workspace_size.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_open_settings(move || {
        let current_config = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            crate::apply_config_to_ui(
                &ui,
                &current_config,
                &options_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
            ui.set_settings_dialog_tab_index(0);
            ui.set_show_settings_dialog(true);
        }
    });

    let tooltip_hover_generation = Arc::new(Mutex::new(0u64));
    let tooltip_hover_generation_clone = Arc::clone(&tooltip_hover_generation);
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_tooltip_hover_changed(move |is_hovered, tooltip, anchor_x_px, anchor_y_px| {
        let generation = {
            let mut value = tooltip_hover_generation_clone
                .lock()
                .expect("tooltip generation lock poisoned");
            *value = value.saturating_add(1);
            *value
        };

        let trimmed_tooltip = tooltip.trim().to_string();
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_show_tooltip(false);
            if !is_hovered || trimmed_tooltip.is_empty() || !ui.get_show_tooltips_enabled() {
                ui.set_tooltip_text("".into());
                return;
            }
            ui.set_tooltip_anchor_x_px(anchor_x_px);
            ui.set_tooltip_anchor_y_px(anchor_y_px);
        } else {
            return;
        }

        let tooltip_hover_generation = Arc::clone(&tooltip_hover_generation_clone);
        let ui_weak = ui_handle_clone.clone();
        slint::Timer::single_shot(
            Duration::from_millis(crate::TOOLTIP_HOVER_DELAY_MS),
            move || {
                let should_show = {
                    let current_generation = tooltip_hover_generation
                        .lock()
                        .expect("tooltip generation lock poisoned");
                    *current_generation == generation
                };
                if !should_show {
                    return;
                }
                if let Some(ui) = ui_weak.upgrade() {
                    if ui.get_show_tooltips_enabled() {
                        ui.set_tooltip_text(trimmed_tooltip.clone().into());
                        ui.set_tooltip_anchor_x_px(anchor_x_px);
                        ui.set_tooltip_anchor_y_px(anchor_y_px);
                        ui.set_show_tooltip(true);
                    } else {
                        ui.set_show_tooltip(false);
                        ui.set_tooltip_text("".into());
                    }
                }
            },
        );
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_settings_custom_theme_color_draft_edited(move |component_index, value| {
        let index = component_index.max(0) as usize;
        if let Some(ui) = ui_handle_clone.upgrade() {
            set_custom_color_draft_value(&ui, index, value.to_string());
        }
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_settings_refresh_custom_color_previews(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            set_custom_color_draft_preview_colors(&ui);
        }
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_settings_open_custom_color_picker(move |component_index, label, value| {
        let index = component_index.max(0) as usize;
        if let Some(ui) = ui_handle_clone.upgrade() {
            let draft_values =
                shared_string_model_to_vec(ui.get_settings_custom_color_draft_values());
            if index >= draft_values.len() {
                return;
            }
            let active_value = if value.trim().is_empty() {
                draft_values[index].clone()
            } else {
                value.to_string()
            };
            let parsed = crate::theme::parse_slint_color(&active_value)
                .or_else(|| crate::theme::parse_slint_color(&draft_values[index]))
                .unwrap_or_else(|| slint::Color::from_rgb_u8(42, 109, 239));
            let rgba = parsed.to_argb_u8();
            ui.set_settings_custom_color_picker_target_index(index as i32);
            ui.set_settings_custom_color_picker_target_label(label);
            ui.set_settings_custom_color_picker_r(f32::from(rgba.red));
            ui.set_settings_custom_color_picker_g(f32::from(rgba.green));
            ui.set_settings_custom_color_picker_b(f32::from(rgba.blue));
            ui.set_show_custom_color_picker_dialog(true);
        }
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_settings_apply_custom_color_picker(move |component_index, red, green, blue| {
        let index = component_index.max(0) as usize;
        let red = red.clamp(0, 255) as u8;
        let green = green.clamp(0, 255) as u8;
        let blue = blue.clamp(0, 255) as u8;
        let next_value = format!("#{red:02X}{green:02X}{blue:02X}");
        if let Some(ui) = ui_handle_clone.upgrade() {
            set_custom_color_draft_value(&ui, index, next_value);
            ui.set_show_custom_color_picker_dialog(false);
        }
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_settings_reset_custom_colors(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            let default_colors = crate::theme::default_custom_theme_colors();
            let default_values = crate::theme::custom_color_values_for_ui(&default_colors);
            set_custom_color_draft_values(&ui, &default_values);
            ui.set_show_custom_color_picker_dialog(false);
        }
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let config_state_clone = shared_state.config_state.clone();
    let output_options_clone = shared_state.runtime_handles.output_options.clone();
    let runtime_output_override_clone =
        shared_state.runtime_handles.runtime_output_override.clone();
    let runtime_audio_state_clone = shared_state.runtime_handles.runtime_audio_state.clone();
    let staged_audio_settings_clone = shared_state.runtime_handles.staged_audio_settings.clone();
    let playback_session_active_clone = shared_state.playback_session_active.clone();
    let last_runtime_signature_clone = shared_state.runtime_handles.last_runtime_signature.clone();
    let last_runtime_config_clone = shared_state.runtime_handles.last_runtime_config.clone();
    let config_file_clone = shared_state.persistence_paths.config_file.clone();
    let layout_workspace_size_clone = shared_state.ui_handles.layout_workspace_size.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_apply_settings(
        move |output_device_index,
              channel_index,
              sample_rate_index,
              bits_per_sample_index,
              output_device_custom_value,
              channel_custom_value,
              sample_rate_custom_value,
              bits_custom_value,
              show_layout_edit_tutorial,
              show_tooltips_enabled,
              auto_scroll_to_playing_track,
              sample_rate_mode_index,
              resampler_quality_index,
              dither_on_bitdepth_reduce,
              downmix_higher_channel_tracks,
              cast_allow_transcode_fallback,
              color_scheme_index,
              custom_color_values| {
            let previous_config = {
                let state = config_state_clone
                    .lock()
                    .expect("config state lock poisoned");
                state.clone()
            };

            let output_device_idx = output_device_index.max(0) as usize;
            let channel_idx = channel_index.max(0) as usize;
            let sample_rate_idx = sample_rate_index.max(0) as usize;
            let bits_idx = bits_per_sample_index.max(0) as usize;
            let sample_rate_mode_idx = sample_rate_mode_index.max(0) as usize;
            let resampler_idx = resampler_quality_index.max(0) as usize;
            let options_snapshot = {
                let options = output_options_clone
                    .lock()
                    .expect("output options lock poisoned");
                options.clone()
            };

            let output_device_auto = output_device_idx == 0;
            let output_device_name = if output_device_auto {
                options_snapshot.auto_device_name.clone()
            } else if output_device_idx <= options_snapshot.device_names.len() {
                options_snapshot.device_names[output_device_idx - 1].clone()
            } else {
                let custom_name = output_device_custom_value.trim();
                if custom_name.is_empty() {
                    previous_config.output.output_device_name.clone()
                } else {
                    custom_name.to_string()
                }
            };
            let channel_count_auto = channel_idx == 0;
            let channel_count = if channel_count_auto {
                options_snapshot.auto_channel_value
            } else if channel_idx <= options_snapshot.channel_values.len() {
                options_snapshot.channel_values[channel_idx - 1]
            } else {
                channel_custom_value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(previous_config.output.channel_count)
            };
            let sample_rate_auto = sample_rate_mode_idx == 0;
            let sample_rate_hz = if sample_rate_idx < options_snapshot.sample_rate_values.len() {
                options_snapshot.sample_rate_values[sample_rate_idx]
            } else {
                sample_rate_custom_value
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(previous_config.output.sample_rate_khz)
            };
            let bits_per_sample_auto = bits_idx == 0;
            let bits_per_sample = if bits_per_sample_auto {
                options_snapshot.auto_bits_per_sample_value
            } else if bits_idx <= options_snapshot.bits_per_sample_values.len() {
                options_snapshot.bits_per_sample_values[bits_idx - 1]
            } else {
                bits_custom_value
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|value| *value > 0)
                    .unwrap_or(previous_config.output.bits_per_sample)
            };
            let resampler_quality = match resampler_idx {
                1 => ResamplerQuality::Highest,
                _ => ResamplerQuality::High,
            };
            let selected_color_scheme = crate::theme::scheme_id_for_index(color_scheme_index);
            let custom_color_values = shared_string_model_to_vec(custom_color_values);
            let fallback_colors =
                crate::theme::resolve_persisted_custom_colors(&previous_config.ui.layout);
            let custom_colors =
                crate::theme::custom_colors_from_ui_values(&custom_color_values, &fallback_colors);
            let mut next_layout = previous_config.ui.layout.clone();
            next_layout.color_scheme = selected_color_scheme;
            next_layout.custom_colors = Some(custom_colors);

            let next_config = crate::sanitize_config(Config {
                output: OutputConfig {
                    output_device_name: if output_device_auto {
                        "default".to_string()
                    } else {
                        output_device_name
                    },
                    output_device_auto,
                    channel_count,
                    sample_rate_khz: sample_rate_hz,
                    bits_per_sample,
                    channel_count_auto,
                    sample_rate_auto,
                    bits_per_sample_auto,
                    resampler_quality,
                    dither_on_bitdepth_reduce,
                    downmix_higher_channel_tracks,
                },
                cast: CastConfig {
                    allow_transcode_fallback: cast_allow_transcode_fallback,
                },
                ui: UiConfig {
                    show_layout_edit_intro: show_layout_edit_tutorial,
                    show_tooltips: show_tooltips_enabled,
                    auto_scroll_to_playing_track,
                    legacy_dark_mode: previous_config.ui.legacy_dark_mode,
                    playlist_album_art_column_min_width_px: previous_config
                        .ui
                        .playlist_album_art_column_min_width_px,
                    playlist_album_art_column_max_width_px: previous_config
                        .ui
                        .playlist_album_art_column_max_width_px,
                    layout: next_layout,
                    playlist_columns: previous_config.ui.playlist_columns.clone(),
                    window_width: previous_config.ui.window_width,
                    window_height: previous_config.ui.window_height,
                    volume: previous_config.ui.volume,
                    playback_order: previous_config.ui.playback_order,
                    repeat_mode: previous_config.ui.repeat_mode,
                },
                library: previous_config.library.clone(),
                buffering: previous_config.buffering.clone(),
                integrations: previous_config.integrations.clone(),
            });

            let (workspace_width_px, workspace_height_px) =
                crate::workspace_size_snapshot(&layout_workspace_size_clone);
            if let Some(ui) = ui_handle_clone.upgrade() {
                crate::apply_config_to_ui(
                    &ui,
                    &next_config,
                    &options_snapshot,
                    workspace_width_px,
                    workspace_height_px,
                );
            }

            {
                let mut state = config_state_clone
                    .lock()
                    .expect("config state lock poisoned");
                *state = next_config.clone();
            }

            persist_state_files_with_config_path(&next_config, &config_file_clone);

            let output_changed =
                output_preferences_changed(&previous_config.output, &next_config.output);
            let audio_changed = audio_settings_changed(&previous_config, &next_config);
            let playback_session_active = playback_session_active_clone.load(Ordering::Relaxed);
            if audio_changed && playback_session_active {
                {
                    let mut staged = staged_audio_settings_clone
                        .lock()
                        .expect("staged audio settings lock poisoned");
                    *staged = Some(StagedAudioSettings {
                        output: next_config.output.clone(),
                        cast: next_config.cast.clone(),
                    });
                }
                if let Some(ui) = ui_handle_clone.upgrade() {
                    ui.set_settings_restart_notice_message(
                        "Audio settings were saved and staged. They apply after playback is stopped and started again."
                            .into(),
                    );
                    ui.set_show_settings_restart_notice(true);
                }
            } else if audio_changed {
                let mut staged = staged_audio_settings_clone
                    .lock()
                    .expect("staged audio settings lock poisoned");
                *staged = None;
                let mut runtime_audio_state = runtime_audio_state_clone
                    .lock()
                    .expect("runtime audio state lock poisoned");
                runtime_audio_state.output = next_config.output.clone();
                runtime_audio_state.cast = next_config.cast.clone();
            }

            if output_changed && !(audio_changed && playback_session_active) {
                let mut runtime_override = runtime_output_override_clone
                    .lock()
                    .expect("runtime output override lock poisoned");
                runtime_override.sample_rate_hz = None;
            }
            let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
            let runtime_config = crate::resolve_effective_runtime_config(
                &next_config,
                &options_snapshot,
                Some(&runtime_override),
                &runtime_audio_state_clone,
            );
            {
                let mut last_runtime = last_runtime_signature_clone
                    .lock()
                    .expect("runtime signature lock poisoned");
                *last_runtime = OutputRuntimeSignature::from_output(&runtime_config.output);
            }
            let _ = publish_runtime_config_delta(
                &bus_sender_clone,
                &last_runtime_config_clone,
                runtime_config,
            );
        },
    );

    let config_state_clone = shared_state.config_state.clone();
    let layout_workspace_size_clone = shared_state.ui_handles.layout_workspace_size.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_window_resized(move |width_px, height_px| {
        let width = (width_px.max(600) as u32).min(10_000);
        let height = (height_px.max(400) as u32).min(10_000);
        let workspace_width_px = width.max(1);
        let workspace_height_px = height.max(1);
        {
            let mut workspace_size = layout_workspace_size_clone
                .lock()
                .expect("layout workspace size lock poisoned");
            *workspace_size = (workspace_width_px, workspace_height_px);
        }
        let mut window_size_changed = false;
        let config_snapshot = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if state.ui.window_width != width || state.ui.window_height != height {
                state.ui.window_width = width;
                state.ui.window_height = height;
                window_size_changed = true;
            }
            state.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_sidebar_width_px(crate::sidebar_width_from_window(width));
            crate::apply_layout_to_ui(
                &ui,
                &config_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
        if window_size_changed {
            let _ = bus_sender_clone.send(Message::Config(protocol::ConfigMessage::ConfigChanged(
                vec![protocol::ConfigDeltaEntry::Ui(protocol::UiConfigDelta {
                    window_width: Some(width),
                    window_height: Some(height),
                    ..Default::default()
                })],
            )));
        }
    });

    let config_state_clone = shared_state.config_state.clone();
    let layout_workspace_size_clone = shared_state.ui_handles.layout_workspace_size.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_layout_workspace_resized(move |width_px, height_px| {
        let workspace_width_px = width_px.max(1) as u32;
        let workspace_height_px = height_px.max(1) as u32;
        {
            let mut workspace_size = layout_workspace_size_clone
                .lock()
                .expect("layout workspace size lock poisoned");
            *workspace_size = (workspace_width_px, workspace_height_px);
        }
        let config_snapshot = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            crate::apply_layout_to_ui(
                &ui,
                &config_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
        let _ = bus_sender_clone.send(Message::Config(protocol::ConfigMessage::ConfigChanged(
            vec![protocol::ConfigDeltaEntry::Ui(protocol::UiConfigDelta {
                window_width: Some(config_snapshot.ui.window_width),
                window_height: Some(config_snapshot.ui.window_height),
                ..Default::default()
            })],
        )));
    });
}
