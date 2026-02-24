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
pub(crate) struct UiHandles {
    pub(crate) ui_handle: slint::Weak<AppWindow>,
    pub(crate) layout_workspace_size: Arc<Mutex<(u32, u32)>>,
}

#[derive(Clone)]
pub(crate) struct PersistencePaths {
    pub(crate) config_file: PathBuf,
}

#[derive(Clone)]
pub(crate) struct RuntimeConfigHandles {
    pub(crate) output_options: Arc<Mutex<OutputSettingsOptions>>,
    pub(crate) runtime_output_override: Arc<Mutex<RuntimeOutputOverride>>,
    pub(crate) runtime_audio_state: Arc<Mutex<RuntimeAudioState>>,
    pub(crate) staged_audio_settings: Arc<Mutex<Option<StagedAudioSettings>>>,
    pub(crate) last_runtime_signature: Arc<Mutex<OutputRuntimeSignature>>,
    pub(crate) last_runtime_config: Arc<Mutex<Config>>,
}

#[derive(Clone)]
pub(crate) struct AppSharedState {
    pub(crate) bus_sender: broadcast::Sender<Message>,
    pub(crate) config_state: Arc<Mutex<Config>>,
    pub(crate) ui_handles: UiHandles,
    pub(crate) runtime_handles: RuntimeConfigHandles,
    pub(crate) persistence_paths: PersistencePaths,
    pub(crate) playback_session_active: Arc<AtomicBool>,
    pub(crate) opensubsonic_session_passwords: Arc<Mutex<HashMap<String, String>>>,
    pub(crate) layout_undo_stack: Arc<Mutex<Vec<LayoutConfig>>>,
    pub(crate) layout_redo_stack: Arc<Mutex<Vec<LayoutConfig>>>,
    pub(crate) playlist_bulk_import_tx: SyncSender<protocol::PlaylistBulkImportRequest>,
}
