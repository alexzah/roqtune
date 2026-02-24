//! Shared application context types used by callback and runtime modules.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, mpsc::SyncSender, Arc, Mutex},
};

use tokio::sync::broadcast;

use crate::{
    config::Config,
    layout::LayoutConfig,
    protocol::{self, Message},
    runtime_config::{
        OutputRuntimeSignature, RuntimeAudioState, RuntimeOutputOverride, StagedAudioSettings,
    },
    AppWindow, OutputSettingsOptions,
};

#[derive(Clone)]
/// Handles related to the Slint UI instance and workspace sizing.
pub(crate) struct UiHandles {
    /// Weak reference to the root Slint component.
    pub(crate) ui_handle: slint::Weak<AppWindow>,
    /// Last known workspace size used for layout rendering.
    pub(crate) layout_workspace_size: Arc<Mutex<(u32, u32)>>,
}

#[derive(Clone)]
/// File-system paths used for persistence.
pub(crate) struct PersistencePaths {
    /// Path to the persisted `config.toml`.
    pub(crate) config_file: PathBuf,
}

#[derive(Clone)]
/// Runtime-state handles used to compute and publish effective config changes.
pub(crate) struct RuntimeConfigHandles {
    /// Current output option inventory for settings resolution.
    pub(crate) output_options: Arc<Mutex<OutputSettingsOptions>>,
    /// Optional runtime output overrides (for adaptive sample-rate routing).
    pub(crate) runtime_output_override: Arc<Mutex<RuntimeOutputOverride>>,
    /// Runtime copy of audio/cast settings acknowledged by worker components.
    pub(crate) runtime_audio_state: Arc<Mutex<RuntimeAudioState>>,
    /// Pending settings awaiting runtime apply/acknowledgement.
    pub(crate) staged_audio_settings: Arc<Mutex<Option<StagedAudioSettings>>>,
    /// Last runtime output signature used for lightweight change detection.
    pub(crate) last_runtime_signature: Arc<Mutex<OutputRuntimeSignature>>,
    /// Last full runtime config snapshot used for delta emission.
    pub(crate) last_runtime_config: Arc<Mutex<Config>>,
}

#[derive(Clone)]
/// Shared state passed to callback registration and coordinator modules.
pub(crate) struct AppSharedState {
    /// Shared producer side of the application bus.
    pub(crate) bus_sender: broadcast::Sender<Message>,
    /// In-memory mutable persisted config state.
    pub(crate) config_state: Arc<Mutex<Config>>,
    /// UI-related weak handles and sizing data.
    pub(crate) ui_handles: UiHandles,
    /// Runtime config synchronization handles.
    pub(crate) runtime_handles: RuntimeConfigHandles,
    /// Config and layout persistence paths.
    pub(crate) persistence_paths: PersistencePaths,
    /// Whether playback is currently active.
    pub(crate) playback_session_active: Arc<AtomicBool>,
    /// Session-scoped OpenSubsonic passwords keyed by profile ID.
    pub(crate) opensubsonic_session_passwords: Arc<Mutex<HashMap<String, String>>>,
    /// Undo stack for layout edits.
    pub(crate) layout_undo_stack: Arc<Mutex<Vec<LayoutConfig>>>,
    /// Redo stack for layout edits.
    pub(crate) layout_redo_stack: Arc<Mutex<Vec<LayoutConfig>>>,
    /// Producer for bulk playlist import requests.
    pub(crate) playlist_bulk_import_tx: SyncSender<protocol::PlaylistBulkImportRequest>,
}
