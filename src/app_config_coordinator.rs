//! Helpers for coordinated config persistence, UI refresh, and runtime publish.

use crate::{
    app_context::AppSharedState,
    config::Config,
    config_persistence::persist_state_files_with_config_path,
    runtime_config::{
        publish_runtime_config_delta, runtime_output_override_snapshot, OutputRuntimeSignature,
    },
};

fn output_options_snapshot(shared: &AppSharedState) -> crate::OutputSettingsOptions {
    let options = shared
        .runtime_handles
        .output_options
        .lock()
        .expect("output options lock poisoned");
    options.clone()
}

/// Applies the current config state snapshot to the active UI, if available.
pub(crate) fn apply_ui_from_state(shared: &AppSharedState) {
    let config_snapshot = {
        let state = shared
            .config_state
            .lock()
            .expect("config state lock poisoned");
        state.clone()
    };
    let options_snapshot = output_options_snapshot(shared);
    let (workspace_width_px, workspace_height_px) =
        crate::workspace_size_snapshot(&shared.ui_handles.layout_workspace_size);
    if let Some(ui) = shared.ui_handles.ui_handle.upgrade() {
        crate::apply_config_to_ui(
            &ui,
            &config_snapshot,
            &options_snapshot,
            workspace_width_px,
            workspace_height_px,
        );
    }
}

/// Publishes runtime-facing config updates derived from persisted state.
pub(crate) fn publish_runtime_from_state(shared: &AppSharedState, next_config: &Config) {
    let options_snapshot = output_options_snapshot(shared);
    let runtime_override =
        runtime_output_override_snapshot(&shared.runtime_handles.runtime_output_override);
    let runtime_config = crate::resolve_effective_runtime_config(
        next_config,
        &options_snapshot,
        Some(&runtime_override),
        &shared.runtime_handles.runtime_audio_state,
    );
    {
        let mut last_runtime = shared
            .runtime_handles
            .last_runtime_signature
            .lock()
            .expect("runtime signature lock poisoned");
        *last_runtime = OutputRuntimeSignature::from_output(&runtime_config.output);
    }
    let _ = publish_runtime_config_delta(
        &shared.bus_sender,
        &shared.runtime_handles.last_runtime_config,
        runtime_config,
    );
}

/// Persists and applies a config update, then publishes runtime deltas.
pub(crate) fn apply_config_update(
    shared: &AppSharedState,
    next_config: Config,
    refresh_ui: bool,
) -> Config {
    {
        let mut state = shared
            .config_state
            .lock()
            .expect("config state lock poisoned");
        *state = next_config.clone();
    }
    persist_state_files_with_config_path(&next_config, &shared.persistence_paths.config_file);
    if refresh_ui {
        apply_ui_from_state(shared);
    }
    publish_runtime_from_state(shared, &next_config);
    next_config
}
