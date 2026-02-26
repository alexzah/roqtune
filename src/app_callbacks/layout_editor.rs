//! Callback registration for interactive layout-editing workflows.

use crate::{
    app_config_coordinator::publish_runtime_from_state,
    app_context::AppSharedState,
    config_persistence::{load_system_layout_template, persist_state_files_with_config_path},
    layout::{
        delete_leaf, first_leaf_id, replace_leaf_panel, sanitize_layout_config, set_split_ratio,
        split_leaf, LayoutNode, LayoutPanelKind, SplitAxis, ViewerPanelDisplayPriority,
        ViewerPanelImageSource, ViewerPanelMetadataSource,
    },
    AppWindow,
};
fn panel_kind_for_leaf(node: &LayoutNode, leaf_id: &str) -> Option<LayoutPanelKind> {
    match node {
        LayoutNode::Leaf { id, panel } => (id == leaf_id).then_some(*panel),
        LayoutNode::Split { first, second, .. } => {
            panel_kind_for_leaf(first, leaf_id).or_else(|| panel_kind_for_leaf(second, leaf_id))
        }
        LayoutNode::Empty => None,
    }
}

/// Registers layout-editor callbacks for panel/menu/splitter editing actions.
pub(crate) fn register_layout_editor_callbacks(ui: &AppWindow, shared_state: &AppSharedState) {
    let config_state_clone = shared_state.config_state.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_open_control_cluster_menu_for_leaf(move |leaf_id, x_px, y_px| {
        if leaf_id.trim().is_empty() {
            return;
        }
        let config_snapshot = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_control_cluster_menu_target_leaf_id(leaf_id.clone());
            ui.set_control_cluster_menu_x(x_px.max(8) as f32);
            ui.set_control_cluster_menu_y(y_px.max(8) as f32);
            crate::apply_control_cluster_menu_actions_for_leaf(&ui, &config_snapshot, &leaf_id);
            ui.set_show_viewer_panel_settings_menu(false);
            ui.set_show_control_cluster_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_add_control_cluster_button(move |leaf_id, action_id| {
        if leaf_id.trim().is_empty() || !(1..=12).contains(&action_id) {
            return;
        }
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            let mut actions = crate::button_cluster_actions_for_leaf(
                &next_config.ui.layout.button_cluster_instances,
                &leaf_id,
            );
            actions.push(action_id);
            crate::upsert_button_cluster_actions_for_leaf(
                &mut next_config.ui.layout.button_cluster_instances,
                &leaf_id,
                actions,
            );
            next_config = crate::sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                &shared_state_clone.ui_handles.layout_workspace_size,
            );
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_control_cluster_menu_target_leaf_id(leaf_id.clone());
            crate::apply_control_cluster_menu_actions_for_leaf(&ui, &next_config, &leaf_id);
            ui.set_show_viewer_panel_settings_menu(false);
            ui.set_show_control_cluster_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_remove_control_cluster_button(move |leaf_id, button_index| {
        if leaf_id.trim().is_empty() || button_index < 0 {
            return;
        }
        let index = button_index as usize;
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            let mut actions = crate::button_cluster_actions_for_leaf(
                &next_config.ui.layout.button_cluster_instances,
                &leaf_id,
            );
            if index >= actions.len() {
                return;
            }
            actions.remove(index);
            crate::upsert_button_cluster_actions_for_leaf(
                &mut next_config.ui.layout.button_cluster_instances,
                &leaf_id,
                actions,
            );
            next_config = crate::sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                &shared_state_clone.ui_handles.layout_workspace_size,
            );
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_control_cluster_menu_target_leaf_id(leaf_id.clone());
            crate::apply_control_cluster_menu_actions_for_leaf(&ui, &next_config, &leaf_id);
            ui.set_show_viewer_panel_settings_menu(false);
            ui.set_show_control_cluster_menu(true);
        }
    });

    let config_state_clone = shared_state.config_state.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_open_viewer_panel_settings_for_leaf(move |leaf_id, panel_kind, x_px, y_px| {
        if leaf_id.trim().is_empty() {
            return;
        }
        if panel_kind != LayoutPanelKind::MetadataViewer.to_code()
            && panel_kind != LayoutPanelKind::AlbumArtViewer.to_code()
        {
            return;
        }
        let (priority_index, metadata_source_index, image_source_index, metadata_text_format) = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if panel_kind == LayoutPanelKind::MetadataViewer.to_code() {
                (
                    crate::viewer_panel_display_priority_to_code(
                        crate::metadata_viewer_panel_display_priority_for_leaf(
                            &state.ui.layout.metadata_viewer_panel_instances,
                            &leaf_id,
                        ),
                    ),
                    crate::viewer_panel_metadata_source_to_code(
                        crate::metadata_viewer_panel_metadata_source_for_leaf(
                            &state.ui.layout.metadata_viewer_panel_instances,
                            &leaf_id,
                        ),
                    ),
                    crate::viewer_panel_image_source_to_code(ViewerPanelImageSource::default()),
                    crate::metadata_viewer_panel_text_format_for_leaf(
                        &state.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id,
                    ),
                )
            } else {
                (
                    crate::viewer_panel_display_priority_to_code(
                        crate::album_art_viewer_panel_display_priority_for_leaf(
                            &state.ui.layout.album_art_viewer_panel_instances,
                            &leaf_id,
                        ),
                    ),
                    crate::viewer_panel_metadata_source_to_code(
                        ViewerPanelMetadataSource::default(),
                    ),
                    crate::viewer_panel_image_source_to_code(
                        crate::album_art_viewer_panel_image_source_for_leaf(
                            &state.ui.layout.album_art_viewer_panel_instances,
                            &leaf_id,
                        ),
                    ),
                    crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE.to_string(),
                )
            }
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_control_cluster_menu_target_leaf_id("".into());
            ui.set_show_control_cluster_menu(false);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id.clone());
            ui.set_viewer_panel_settings_target_panel_kind(panel_kind);
            ui.set_viewer_panel_settings_display_priority_index(priority_index);
            ui.set_viewer_panel_settings_metadata_source_index(metadata_source_index);
            ui.set_viewer_panel_settings_image_source_index(image_source_index);
            ui.set_viewer_panel_settings_metadata_text_format(metadata_text_format.into());
            ui.set_viewer_panel_settings_x(x_px.max(8) as f32);
            ui.set_viewer_panel_settings_y(y_px.max(8) as f32);
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_set_viewer_panel_display_priority(move |leaf_id, priority_code| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let display_priority = crate::viewer_panel_display_priority_from_code(priority_code);
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            match panel_kind_for_leaf(&next_config.ui.layout.root, &leaf_id_text) {
                Some(LayoutPanelKind::MetadataViewer) => {
                    crate::upsert_metadata_viewer_panel_display_priority_for_leaf(
                        &mut next_config.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id_text,
                        display_priority,
                    );
                }
                Some(LayoutPanelKind::AlbumArtViewer) => {
                    crate::upsert_album_art_viewer_panel_display_priority_for_leaf(
                        &mut next_config.ui.layout.album_art_viewer_panel_instances,
                        &leaf_id_text,
                        display_priority,
                    );
                }
                _ => return,
            }
            next_config = crate::sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                &shared_state_clone.ui_handles.layout_workspace_size,
            );
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            let panel_kind = panel_kind_for_leaf(&next_config.ui.layout.root, &leaf_id_text);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                crate::viewer_panel_display_priority_to_code(match panel_kind {
                    Some(LayoutPanelKind::MetadataViewer) => {
                        crate::metadata_viewer_panel_display_priority_for_leaf(
                            &next_config.ui.layout.metadata_viewer_panel_instances,
                            &leaf_id_text,
                        )
                    }
                    Some(LayoutPanelKind::AlbumArtViewer) => {
                        crate::album_art_viewer_panel_display_priority_for_leaf(
                            &next_config.ui.layout.album_art_viewer_panel_instances,
                            &leaf_id_text,
                        )
                    }
                    _ => ViewerPanelDisplayPriority::default(),
                }),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                crate::viewer_panel_metadata_source_to_code(
                    crate::metadata_viewer_panel_metadata_source_for_leaf(
                        &next_config.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_image_source_index(
                crate::viewer_panel_image_source_to_code(
                    crate::album_art_viewer_panel_image_source_for_leaf(
                        &next_config.ui.layout.album_art_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_metadata_text_format(
                crate::metadata_viewer_panel_text_format_for_leaf(
                    &next_config.ui.layout.metadata_viewer_panel_instances,
                    &leaf_id_text,
                )
                .into(),
            );
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_set_viewer_panel_metadata_source(move |leaf_id, source_code| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let metadata_source = crate::viewer_panel_metadata_source_from_code(source_code);
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            crate::upsert_metadata_viewer_panel_metadata_source_for_leaf(
                &mut next_config.ui.layout.metadata_viewer_panel_instances,
                &leaf_id_text,
                metadata_source,
            );
            next_config = crate::sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                &shared_state_clone.ui_handles.layout_workspace_size,
            );
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                crate::viewer_panel_display_priority_to_code(
                    crate::metadata_viewer_panel_display_priority_for_leaf(
                        &next_config.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                crate::viewer_panel_metadata_source_to_code(metadata_source),
            );
            ui.set_viewer_panel_settings_image_source_index(
                crate::viewer_panel_image_source_to_code(
                    crate::album_art_viewer_panel_image_source_for_leaf(
                        &next_config.ui.layout.album_art_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_metadata_text_format(
                crate::metadata_viewer_panel_text_format_for_leaf(
                    &next_config.ui.layout.metadata_viewer_panel_instances,
                    &leaf_id_text,
                )
                .into(),
            );
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_set_viewer_panel_metadata_text_format(move |leaf_id, metadata_text_format| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            crate::upsert_metadata_viewer_panel_text_format_for_leaf(
                &mut next_config.ui.layout.metadata_viewer_panel_instances,
                &leaf_id_text,
                metadata_text_format.to_string(),
            );
            next_config = crate::sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                &shared_state_clone.ui_handles.layout_workspace_size,
            );
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                crate::viewer_panel_display_priority_to_code(
                    crate::metadata_viewer_panel_display_priority_for_leaf(
                        &next_config.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                crate::viewer_panel_metadata_source_to_code(
                    crate::metadata_viewer_panel_metadata_source_for_leaf(
                        &next_config.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_image_source_index(
                crate::viewer_panel_image_source_to_code(
                    crate::album_art_viewer_panel_image_source_for_leaf(
                        &next_config.ui.layout.album_art_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_metadata_text_format(
                crate::metadata_viewer_panel_text_format_for_leaf(
                    &next_config.ui.layout.metadata_viewer_panel_instances,
                    &leaf_id_text,
                )
                .into(),
            );
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_set_viewer_panel_image_source(move |leaf_id, source_code| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let image_source = crate::viewer_panel_image_source_from_code(source_code);
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            crate::upsert_album_art_viewer_panel_image_source_for_leaf(
                &mut next_config.ui.layout.album_art_viewer_panel_instances,
                &leaf_id_text,
                image_source,
            );
            next_config = crate::sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                &shared_state_clone.ui_handles.layout_workspace_size,
            );
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(
            &next_config,
            &shared_state_clone.persistence_paths.config_file,
        );
        publish_runtime_from_state(&shared_state_clone, &next_config);
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                crate::viewer_panel_display_priority_to_code(
                    crate::album_art_viewer_panel_display_priority_for_leaf(
                        &next_config.ui.layout.album_art_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                crate::viewer_panel_metadata_source_to_code(
                    crate::metadata_viewer_panel_metadata_source_for_leaf(
                        &next_config.ui.layout.metadata_viewer_panel_instances,
                        &leaf_id_text,
                    ),
                ),
            );
            ui.set_viewer_panel_settings_image_source_index(
                crate::viewer_panel_image_source_to_code(image_source),
            );
            ui.set_viewer_panel_settings_metadata_text_format(
                crate::metadata_viewer_panel_text_format_for_leaf(
                    &next_config.ui.layout.metadata_viewer_panel_instances,
                    &leaf_id_text,
                )
                .into(),
            );
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_open_layout_editor(move || {
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            if ui.get_layout_edit_mode() {
                ui.set_show_layout_editor_dialog(false);
                ui.set_show_layout_leaf_context_menu(false);
                ui.set_show_layout_splitter_context_menu(false);
                ui.set_layout_edit_mode(false);
                let (workspace_width_px, workspace_height_px) = crate::workspace_size_snapshot(
                    &shared_state_clone.ui_handles.layout_workspace_size,
                );
                let config_snapshot = {
                    let state = shared_state_clone
                        .config_state
                        .lock()
                        .expect("config state lock poisoned");
                    state.clone()
                };
                crate::apply_layout_to_ui(
                    &ui,
                    &config_snapshot,
                    workspace_width_px,
                    workspace_height_px,
                );
                return;
            }
        }

        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let next = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut layout =
                sanitize_layout_config(&state.ui.layout, workspace_width_px, workspace_height_px);
            if layout.selected_leaf_id.is_none() {
                layout.selected_leaf_id = first_leaf_id(&layout.root);
            }
            state.ui.layout = layout;
            state.clone()
        };
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            ui.set_layout_editor_replace_panel_kind(LayoutPanelKind::Spacer.to_code());
            ui.set_layout_editor_split_panel_kind(LayoutPanelKind::Spacer.to_code());
            ui.set_layout_editor_split_axis(SplitAxis::Vertical.to_code());
            ui.set_layout_editor_new_root_panel_kind(LayoutPanelKind::TrackList.to_code());
            ui.set_layout_active_splitter_index(-1);
            ui.set_show_layout_leaf_context_menu(false);
            ui.set_show_layout_splitter_context_menu(false);
            ui.set_layout_intro_dont_show_again(false);
            ui.set_layout_edit_mode(true);
            ui.set_show_layout_editor_dialog(next.ui.show_layout_edit_intro);
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_intro_proceed(move |dont_show_again| {
        if !dont_show_again {
            return;
        }
        let next = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            if state.ui.show_layout_edit_intro {
                state.ui.show_layout_edit_intro = false;
            }
            state.clone()
        };
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_intro_cancel(move |dont_show_again| {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let next = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            if dont_show_again && state.ui.show_layout_edit_intro {
                state.ui.show_layout_edit_intro = false;
            }
            state.clone()
        };
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            ui.set_layout_edit_mode(false);
            ui.set_show_layout_leaf_context_menu(false);
            ui.set_show_layout_splitter_context_menu(false);
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_select_leaf(move |leaf_id| {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let next = {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut layout =
                sanitize_layout_config(&state.ui.layout, workspace_width_px, workspace_height_px);
            layout.selected_leaf_id = if leaf_id.trim().is_empty() {
                None
            } else {
                Some(leaf_id.to_string())
            };
            state.ui.layout = layout;
            state.clone()
        };
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_replace_selected_leaf(move |panel_code| {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let previous = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let selected_leaf_id = previous.ui.layout.selected_leaf_id.clone();
        let Some(selected_leaf_id) = selected_leaf_id else {
            return;
        };
        let (panel, preset_actions) = crate::resolve_layout_panel_selection(panel_code);
        let Some(mut updated_layout) =
            replace_leaf_panel(&previous.ui.layout, &selected_leaf_id, panel)
        else {
            return;
        };
        updated_layout.selected_leaf_id = if panel == LayoutPanelKind::None {
            first_leaf_id(&updated_layout.root)
        } else {
            Some(selected_leaf_id.clone())
        };
        let mut next = crate::with_updated_layout(&previous, updated_layout);
        crate::apply_layout_panel_selection_preset_to_leaf(
            &mut next.ui.layout,
            &selected_leaf_id,
            panel,
            panel_code,
            preset_actions,
        );
        next = crate::sanitize_config(next);
        crate::push_layout_undo_snapshot(
            &shared_state_clone.layout_undo_stack,
            &shared_state_clone.layout_redo_stack,
            &previous.ui.layout,
        );
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_split_selected_leaf(move |axis_code, panel_code| {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let previous = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(selected_leaf_id) = previous.ui.layout.selected_leaf_id.clone() else {
            return;
        };
        let axis = if axis_code == SplitAxis::Horizontal.to_code() {
            SplitAxis::Horizontal
        } else {
            SplitAxis::Vertical
        };
        let (panel, preset_actions) = crate::resolve_layout_panel_selection(panel_code);
        let Some(mut updated_layout) =
            split_leaf(&previous.ui.layout, &selected_leaf_id, axis, panel)
        else {
            return;
        };
        let added_leaf_ids = crate::newly_added_leaf_ids(&previous.ui.layout, &updated_layout);
        let added_leaf_id = added_leaf_ids.first().cloned();
        updated_layout.selected_leaf_id = if panel == LayoutPanelKind::None {
            Some(selected_leaf_id.clone())
        } else {
            added_leaf_id
                .clone()
                .or_else(|| first_leaf_id(&updated_layout.root))
        };
        let mut next = crate::with_updated_layout(&previous, updated_layout);
        if let Some(new_leaf_id) = added_leaf_id {
            crate::apply_layout_panel_selection_preset_to_leaf(
                &mut next.ui.layout,
                &new_leaf_id,
                panel,
                panel_code,
                preset_actions,
            );
        }
        next = crate::sanitize_config(next);
        crate::push_layout_undo_snapshot(
            &shared_state_clone.layout_undo_stack,
            &shared_state_clone.layout_redo_stack,
            &previous.ui.layout,
        );
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_delete_selected_leaf(move || {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let previous = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(selected_leaf_id) = previous.ui.layout.selected_leaf_id.clone() else {
            return;
        };
        let Some(mut updated_layout) = delete_leaf(&previous.ui.layout, &selected_leaf_id) else {
            return;
        };
        updated_layout.selected_leaf_id = first_leaf_id(&updated_layout.root);
        let next = crate::with_updated_layout(&previous, updated_layout);
        crate::push_layout_undo_snapshot(
            &shared_state_clone.layout_undo_stack,
            &shared_state_clone.layout_redo_stack,
            &previous.ui.layout,
        );
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_add_root_leaf(move |panel_code| {
        let (panel, preset_actions) = crate::resolve_layout_panel_selection(panel_code);
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let previous = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let mut updated_layout = crate::add_root_leaf_if_empty(&previous.ui.layout, panel);
        let added_leaf_ids = crate::newly_added_leaf_ids(&previous.ui.layout, &updated_layout);
        let added_leaf_id = added_leaf_ids.first().cloned();
        updated_layout.selected_leaf_id = first_leaf_id(&updated_layout.root);
        let mut next = crate::with_updated_layout(&previous, updated_layout);
        if let Some(new_leaf_id) = added_leaf_id {
            crate::apply_layout_panel_selection_preset_to_leaf(
                &mut next.ui.layout,
                &new_leaf_id,
                panel,
                panel_code,
                preset_actions,
            );
        }
        next = crate::sanitize_config(next);
        crate::push_layout_undo_snapshot(
            &shared_state_clone.layout_undo_stack,
            &shared_state_clone.layout_redo_stack,
            &previous.ui.layout,
        );
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_preview_layout_splitter_ratio(move |split_id, ratio| {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let snapshot = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(updated_layout) = set_split_ratio(&snapshot.ui.layout, &split_id, ratio) else {
            return;
        };
        let preview_config = crate::with_updated_layout(&snapshot, updated_layout);
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(
                &ui,
                &preview_config,
                workspace_width_px,
                workspace_height_px,
            );
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_commit_layout_splitter_ratio(move |split_id, ratio| {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let quantized_ratio = crate::quantize_splitter_ratio_to_precision(ratio);
        let previous = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(updated_layout) = set_split_ratio(&previous.ui.layout, &split_id, quantized_ratio)
        else {
            return;
        };
        let next = crate::with_updated_layout(&previous, updated_layout);
        crate::push_layout_undo_snapshot(
            &shared_state_clone.layout_undo_stack,
            &shared_state_clone.layout_redo_stack,
            &previous.ui.layout,
        );
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_undo_last_action(move || {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let current = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(previous_layout) =
            crate::pop_layout_snapshot(&shared_state_clone.layout_undo_stack)
        else {
            return;
        };
        crate::push_layout_snapshot(&shared_state_clone.layout_redo_stack, &current.ui.layout);
        let next = crate::with_updated_layout(&current, previous_layout);
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_layout_redo_last_action(move || {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let current = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(next_layout) = crate::pop_layout_snapshot(&shared_state_clone.layout_redo_stack)
        else {
            return;
        };
        crate::push_layout_snapshot(&shared_state_clone.layout_undo_stack, &current.ui.layout);
        let next = crate::with_updated_layout(&current, next_layout);
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let shared_state_clone = shared_state.clone();
    ui.on_reset_layout_default(move || {
        let (workspace_width_px, workspace_height_px) =
            crate::workspace_size_snapshot(&shared_state_clone.ui_handles.layout_workspace_size);
        let previous = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let mut reset_layout = load_system_layout_template();
        reset_layout.selected_leaf_id = first_leaf_id(&reset_layout.root);
        let next = crate::with_updated_layout(&previous, reset_layout);
        crate::push_layout_undo_snapshot(
            &shared_state_clone.layout_undo_stack,
            &shared_state_clone.layout_redo_stack,
            &previous.ui.layout,
        );
        {
            let mut state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(
            &next,
            &shared_state_clone.persistence_paths.config_file,
        );
        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            crate::apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });
}
