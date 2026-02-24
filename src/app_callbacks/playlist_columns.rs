use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use log::debug;

use crate::{
    app_config_coordinator::publish_runtime_from_state,
    app_context::AppSharedState,
    config::{Config, PlaylistColumnConfig, UiConfig},
    config_persistence::persist_state_files_with_config_path,
    protocol::{Message, PlaylistMessage},
    AppWindow,
};

pub(crate) fn should_apply_custom_column_delete(
    confirm_dialog_visible: bool,
    pending_index: i32,
    requested_index: usize,
) -> bool {
    if !confirm_dialog_visible {
        return false;
    }
    pending_index >= 0 && pending_index as usize == requested_index
}

pub(crate) fn register_playlist_column_callbacks(ui: &AppWindow, shared_state: &AppSharedState) {
    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_playlist_columns_viewport_resized(move |width_px| {
        let clamped_width = width_px.max(0) as u32;
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::PlaylistViewportWidthChanged(clamped_width),
        ));
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_playlist_header_column_at(move |mouse_x_px| {
        ui_handle_clone
            .upgrade()
            .map(|ui| {
                crate::resolve_playlist_header_column_from_x(
                    mouse_x_px,
                    &crate::playlist_column_widths_from_model(ui.get_playlist_column_widths_px()),
                )
            })
            .unwrap_or(-1)
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_playlist_header_gap_at(move |mouse_x_px| {
        ui_handle_clone
            .upgrade()
            .map(|ui| {
                crate::resolve_playlist_header_gap_from_x(
                    mouse_x_px,
                    &crate::playlist_column_widths_from_model(ui.get_playlist_column_widths_px()),
                )
            })
            .unwrap_or(-1)
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_playlist_header_divider_at(move |mouse_x_px| {
        ui_handle_clone
            .upgrade()
            .map(|ui| {
                crate::resolve_playlist_header_divider_from_x(
                    mouse_x_px,
                    &crate::playlist_column_widths_from_model(ui.get_playlist_column_widths_px()),
                    4,
                )
            })
            .unwrap_or(-1)
    });

    let config_state_clone = shared_state.config_state.clone();
    ui.on_playlist_header_column_min_width(move |visible_index| {
        let state = config_state_clone
            .lock()
            .expect("config state lock poisoned");
        let album_art_bounds = crate::album_art_column_width_bounds(&state.ui);
        crate::playlist_column_bounds_at_visible_index(
            &state.ui.playlist_columns,
            visible_index.max(0) as usize,
            album_art_bounds,
        )
        .map(|bounds| bounds.min_px)
        .unwrap_or(1)
    });

    let config_state_clone = shared_state.config_state.clone();
    ui.on_playlist_header_column_max_width(move |visible_index| {
        let state = config_state_clone
            .lock()
            .expect("config state lock poisoned");
        let album_art_bounds = crate::album_art_column_width_bounds(&state.ui);
        crate::playlist_column_bounds_at_visible_index(
            &state.ui.playlist_columns,
            visible_index.max(0) as usize,
            album_art_bounds,
        )
        .map(|bounds| bounds.max_px.max(bounds.min_px))
        .unwrap_or(1024)
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let config_state_clone = shared_state.config_state.clone();
    let preview_throttle_state = Arc::new(Mutex::new(
        crate::PlaylistColumnPreviewThrottleState::default(),
    ));
    ui.on_preview_playlist_column_width(move |visible_index, width_px| {
        let (column_key, clamped_width_px) = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let visible_index = visible_index.max(0) as usize;
            let album_art_bounds = crate::album_art_column_width_bounds(&state.ui);
            let column_key = crate::playlist_column_key_at_visible_index(
                &state.ui.playlist_columns,
                visible_index,
            );
            let clamped = crate::clamp_width_for_visible_column(
                &state.ui.playlist_columns,
                visible_index,
                width_px,
                album_art_bounds,
            );
            (column_key, clamped)
        };
        if let (Some(column_key), Some(clamped_width_px)) = (column_key, clamped_width_px) {
            let should_send = {
                let mut throttle = preview_throttle_state
                    .lock()
                    .expect("playlist column preview throttle lock poisoned");
                if throttle.last_column_key.as_deref() != Some(column_key.as_str()) {
                    throttle.last_column_key = Some(column_key.clone());
                    throttle.last_sent_at = None;
                }
                let now = Instant::now();
                match throttle.last_sent_at {
                    Some(last_sent_at)
                        if now.duration_since(last_sent_at)
                            < crate::PLAYLIST_COLUMN_PREVIEW_THROTTLE_INTERVAL =>
                    {
                        false
                    }
                    _ => {
                        throttle.last_sent_at = Some(now);
                        true
                    }
                }
            };
            if !should_send {
                return;
            }
            let _ = bus_sender_clone.send(Message::Playlist(
                PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                    column_key,
                    width_px: clamped_width_px as u32,
                },
            ));
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_commit_playlist_column_width(move |visible_index, width_px| {
        let (column_key, clamped_width_px) = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let visible_index = visible_index.max(0) as usize;
            let album_art_bounds = crate::album_art_column_width_bounds(&state.ui);
            let column_key = crate::playlist_column_key_at_visible_index(
                &state.ui.playlist_columns,
                visible_index,
            );
            let clamped = crate::clamp_width_for_visible_column(
                &state.ui.playlist_columns,
                visible_index,
                width_px,
                album_art_bounds,
            );
            (column_key, clamped)
        };
        if let (Some(column_key), Some(clamped_width_px)) = (column_key, clamped_width_px) {
            let next_config = {
                let mut state = shared_state_clone
                    .config_state
                    .lock()
                    .expect("config state lock poisoned");
                let mut next = state.clone();
                crate::upsert_layout_column_width_override(
                    &mut next.ui.layout,
                    &column_key,
                    clamped_width_px as u32,
                );
                next = crate::sanitize_config(next);
                *state = next.clone();
                next
            };
            persist_state_files_with_config_path(
                &next_config,
                &shared_state_clone.persistence_paths.config_file,
            );
            publish_runtime_from_state(&shared_state_clone, &next_config);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_reset_playlist_column_width(move |visible_index| {
        let column_key = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            crate::playlist_column_key_at_visible_index(
                &state.ui.playlist_columns,
                visible_index.max(0) as usize,
            )
        };
        if let Some(column_key) = column_key {
            let next_config = {
                let mut state = shared_state_clone
                    .config_state
                    .lock()
                    .expect("config state lock poisoned");
                let mut next = state.clone();
                crate::clear_layout_column_width_override(&mut next.ui.layout, &column_key);
                next = crate::sanitize_config(next);
                *state = next.clone();
                next
            };
            persist_state_files_with_config_path(
                &next_config,
                &shared_state_clone.persistence_paths.config_file,
            );
            publish_runtime_from_state(&shared_state_clone, &next_config);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_toggle_playlist_column(move |column_index| {
        let column_idx = column_index.max(0) as usize;
        let previous_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        if column_idx >= previous_config.ui.playlist_columns.len() {
            return;
        }

        let mut updated_columns = previous_config.ui.playlist_columns.clone();
        if let Some(column) = updated_columns.get_mut(column_idx) {
            column.enabled = !column.enabled;
        }
        if updated_columns.iter().all(|column| !column.enabled) {
            if let Some(first_column) = updated_columns.first_mut() {
                first_column.enabled = true;
            }
        }

        let next_config = crate::sanitize_config(Config {
            output: previous_config.output.clone(),
            cast: previous_config.cast.clone(),
            ui: UiConfig {
                show_layout_edit_intro: previous_config.ui.show_layout_edit_intro,
                show_tooltips: previous_config.ui.show_tooltips,
                auto_scroll_to_playing_track: previous_config.ui.auto_scroll_to_playing_track,
                dark_mode: previous_config.ui.dark_mode,
                playlist_album_art_column_min_width_px: previous_config
                    .ui
                    .playlist_album_art_column_min_width_px,
                playlist_album_art_column_max_width_px: previous_config
                    .ui
                    .playlist_album_art_column_max_width_px,
                layout: previous_config.ui.layout.clone(),
                playlist_columns: updated_columns,
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

        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        let _ = shared_state_clone.bus_sender.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(crate::playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

    let shared_state_clone = shared_state.clone();
    ui.on_add_custom_playlist_column(move |name, format| {
        let trimmed_name = name.trim();
        let trimmed_format = format.trim();
        if trimmed_name.is_empty() || trimmed_format.is_empty() {
            return;
        }

        let previous_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };

        let mut updated_columns = previous_config.ui.playlist_columns.clone();
        if let Some(existing) = updated_columns.iter_mut().find(|column| {
            column.custom && column.name == trimmed_name && column.format == trimmed_format
        }) {
            existing.enabled = true;
        } else {
            updated_columns.push(PlaylistColumnConfig {
                name: trimmed_name.to_string(),
                format: trimmed_format.to_string(),
                enabled: true,
                custom: true,
            });
        }

        let next_config = crate::sanitize_config(Config {
            output: previous_config.output.clone(),
            cast: previous_config.cast.clone(),
            ui: UiConfig {
                show_layout_edit_intro: previous_config.ui.show_layout_edit_intro,
                show_tooltips: previous_config.ui.show_tooltips,
                auto_scroll_to_playing_track: previous_config.ui.auto_scroll_to_playing_track,
                dark_mode: previous_config.ui.dark_mode,
                playlist_album_art_column_min_width_px: previous_config
                    .ui
                    .playlist_album_art_column_min_width_px,
                playlist_album_art_column_max_width_px: previous_config
                    .ui
                    .playlist_album_art_column_max_width_px,
                layout: previous_config.ui.layout.clone(),
                playlist_columns: updated_columns,
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

        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        let _ = shared_state_clone.bus_sender.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(crate::playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

    let shared_state_clone = shared_state.clone();
    ui.on_delete_custom_playlist_column(move |column_index| {
        let column_idx = column_index.max(0) as usize;
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            if !should_apply_custom_column_delete(
                ui.get_show_delete_custom_column_confirm(),
                ui.get_delete_custom_column_index(),
                column_idx,
            ) {
                return;
            }
        } else {
            return;
        }

        let previous_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        if column_idx >= previous_config.ui.playlist_columns.len() {
            return;
        }
        if !previous_config.ui.playlist_columns[column_idx].custom {
            return;
        }

        let mut updated_columns = previous_config.ui.playlist_columns.clone();
        updated_columns.remove(column_idx);

        let next_config = crate::sanitize_config(Config {
            output: previous_config.output.clone(),
            cast: previous_config.cast.clone(),
            ui: UiConfig {
                show_layout_edit_intro: previous_config.ui.show_layout_edit_intro,
                show_tooltips: previous_config.ui.show_tooltips,
                auto_scroll_to_playing_track: previous_config.ui.auto_scroll_to_playing_track,
                dark_mode: previous_config.ui.dark_mode,
                playlist_album_art_column_min_width_px: previous_config
                    .ui
                    .playlist_album_art_column_min_width_px,
                playlist_album_art_column_max_width_px: previous_config
                    .ui
                    .playlist_album_art_column_max_width_px,
                layout: previous_config.ui.layout.clone(),
                playlist_columns: updated_columns,
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

        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        let _ = shared_state_clone.bus_sender.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(crate::playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

    let shared_state_clone = shared_state.clone();
    ui.on_reorder_playlist_columns(move |from_visible_index, to_visible_index| {
        let from_visible_index = from_visible_index.max(0) as usize;
        let to_visible_index = to_visible_index.max(0) as usize;
        debug!(
            "Reorder playlist columns requested: from_visible_index={} to_visible_index={}",
            from_visible_index, to_visible_index
        );
        let previous_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };

        let updated_columns = crate::reorder_visible_playlist_columns(
            &previous_config.ui.playlist_columns,
            from_visible_index,
            to_visible_index,
        );
        if updated_columns == previous_config.ui.playlist_columns {
            debug!("Reorder playlist columns: no effective change");
            return;
        }

        let next_config = crate::sanitize_config(Config {
            output: previous_config.output.clone(),
            cast: previous_config.cast.clone(),
            ui: UiConfig {
                show_layout_edit_intro: previous_config.ui.show_layout_edit_intro,
                show_tooltips: previous_config.ui.show_tooltips,
                auto_scroll_to_playing_track: previous_config.ui.auto_scroll_to_playing_track,
                dark_mode: previous_config.ui.dark_mode,
                playlist_album_art_column_min_width_px: previous_config
                    .ui
                    .playlist_album_art_column_min_width_px,
                playlist_album_art_column_max_width_px: previous_config
                    .ui
                    .playlist_album_art_column_max_width_px,
                layout: previous_config.ui.layout.clone(),
                playlist_columns: updated_columns,
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

        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        let _ = shared_state_clone.bus_sender.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(crate::playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::should_apply_custom_column_delete;

    #[test]
    fn test_should_apply_custom_column_delete_requires_matching_confirmation_state() {
        assert!(!should_apply_custom_column_delete(false, 3, 3));
        assert!(!should_apply_custom_column_delete(true, -1, 0));
        assert!(!should_apply_custom_column_delete(true, 1, 0));
        assert!(should_apply_custom_column_delete(true, 2, 2));
    }
}
