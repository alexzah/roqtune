mod audio_decoder;
mod audio_player;
mod audio_probe;
mod backends;
mod cast_manager;
mod config;
mod db_manager;
mod integration_keyring;
mod integration_manager;
mod integration_uri;
mod layout;
mod library_enrichment_manager;
mod library_manager;
mod media_controls_manager;
mod metadata_manager;
mod metadata_tags;
mod playlist;
mod playlist_manager;
mod protocol;
mod ui_manager;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use audio_decoder::AudioDecoder;
use audio_player::AudioPlayer;
use audio_probe::get_or_probe_output_device;
use cast_manager::CastManager;
use config::{
    BackendProfileConfig, BufferingConfig, ButtonClusterInstanceConfig, CastConfig, Config,
    IntegrationBackendKind, IntegrationsConfig, LibraryConfig, OutputConfig, PlaylistColumnConfig,
    ResamplerQuality, UiConfig, UiPlaybackOrder, UiRepeatMode,
};
use cpal::traits::{DeviceTrait, HostTrait};
use db_manager::DbManager;
use integration_keyring::{get_opensubsonic_password, set_opensubsonic_password};
use integration_manager::IntegrationManager;
use layout::{
    add_root_leaf_if_empty, compute_tree_layout_metrics, delete_leaf, first_leaf_id,
    replace_leaf_panel, sanitize_layout_config, set_split_ratio, split_leaf, LayoutConfig,
    LayoutNode, LayoutPanelKind, SplitAxis, ViewerPanelDisplayPriority, ViewerPanelImageSource,
    ViewerPanelInstanceConfig, ViewerPanelMetadataSource, SPLITTER_THICKNESS_PX,
};
use library_enrichment_manager::LibraryEnrichmentManager;
use library_manager::LibraryManager;
use log::{debug, info, warn};
use media_controls_manager::MediaControlsManager;
use metadata_manager::MetadataManager;
use playlist::Playlist;
use playlist_manager::PlaylistManager;
use protocol::{
    CastMessage, ConfigMessage, IntegrationMessage, Message, MetadataMessage, PlaybackMessage,
    PlaylistMessage,
};
use slint::winit_030::{winit, EventResult as WinitEventResult, WinitWindowAccessor};
use slint::{language::ColorScheme, ComponentHandle, LogicalSize, Model, ModelRc, VecModel};
use tokio::sync::broadcast;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};
use ui_manager::{UiManager, UiState};

slint::include_modules!();

fn setup_app_state_associations(ui: &AppWindow, ui_state: &UiState) {
    ui.set_track_model(ModelRc::from(ui_state.track_model.clone()));
    ui.set_library_model(ModelRc::from(Rc::new(VecModel::from(
        Vec::<LibraryRowData>::new(),
    ))));
    ui.set_settings_library_folders(ModelRc::from(Rc::new(VecModel::from(Vec::<
        slint::SharedString,
    >::new()))));
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

fn flash_read_only_view_indicator(ui_handle: slint::Weak<AppWindow>) {
    if let Some(ui) = ui_handle.upgrade() {
        // Reset first so repeated blocked actions retrigger the highlight reliably.
        ui.set_playlist_filter_blocked_feedback(false);
        ui.set_playlist_filter_blocked_feedback(true);
    }
    slint::Timer::single_shot(Duration::from_millis(380), move || {
        if let Some(ui) = ui_handle.upgrade() {
            ui.set_playlist_filter_blocked_feedback(false);
        }
    });
}

fn spawn_debounced_query_dispatcher(
    debounce_delay: Duration,
    mut dispatch: impl FnMut(String) + Send + 'static,
) -> mpsc::Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        while let Ok(initial_query) = rx.recv() {
            let mut pending_query = initial_query;
            loop {
                match rx.recv_timeout(debounce_delay) {
                    Ok(next_query) => pending_query = next_query,
                    Err(RecvTimeoutError::Timeout) => {
                        dispatch(pending_query);
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        dispatch(pending_query);
                        return;
                    }
                }
            }
        }
    });
    tx
}

/// Detected and filtered output-setting choices presented in the settings dialog.
#[derive(Debug, Clone)]
struct OutputSettingsOptions {
    device_names: Vec<String>,
    auto_device_name: String,
    channel_values: Vec<u16>,
    sample_rate_values: Vec<u32>,
    bits_per_sample_values: Vec<u16>,
    verified_sample_rate_values: Vec<u32>,
    verified_sample_rates_summary: String,
    auto_channel_value: u16,
    auto_sample_rate_value: u32,
    auto_bits_per_sample_value: u16,
}

#[derive(Debug, Clone, Default)]
struct RuntimeOutputOverride {
    sample_rate_hz: Option<u32>,
}

#[derive(Debug, Clone)]
struct RuntimeAudioState {
    output: OutputConfig,
    cast: CastConfig,
}

#[derive(Debug, Clone)]
struct StagedAudioSettings {
    output: OutputConfig,
    cast: CastConfig,
}

/// Lightweight signature used to detect meaningful runtime-output changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputRuntimeSignature {
    output_device_name: String,
    channel_count: u16,
    sample_rate_hz: u32,
    bits_per_sample: u16,
}

impl OutputRuntimeSignature {
    fn from_output(output: &OutputConfig) -> Self {
        Self {
            output_device_name: output.output_device_name.clone(),
            channel_count: output.channel_count,
            sample_rate_hz: output.sample_rate_khz,
            bits_per_sample: output.bits_per_sample,
        }
    }
}

fn resolve_effective_runtime_config(
    persisted_config: &Config,
    output_options: &OutputSettingsOptions,
    runtime_output_override: Option<&RuntimeOutputOverride>,
    runtime_audio_state: &Arc<Mutex<RuntimeAudioState>>,
) -> Config {
    let runtime_audio = runtime_audio_state
        .lock()
        .expect("runtime audio state lock poisoned")
        .clone();
    let mut effective_config = persisted_config.clone();
    effective_config.output = runtime_audio.output;
    effective_config.cast = runtime_audio.cast;
    resolve_runtime_config(&effective_config, output_options, runtime_output_override)
}

const IMPORT_CLUSTER_PRESET: [i32; 1] = [1];
const TRANSPORT_CLUSTER_PRESET: [i32; 5] = [2, 3, 4, 5, 6];
const UTILITY_CLUSTER_PRESET: [i32; 4] = [7, 8, 11, 10];
const SUPPORTED_AUDIO_EXTENSIONS: [&str; 7] = ["mp3", "wav", "ogg", "flac", "aac", "m4a", "mp4"];
const PLAYLIST_COLUMN_KIND_TEXT: i32 = 0;
const PLAYLIST_COLUMN_KIND_ALBUM_ART: i32 = 1;
const TOOLTIP_HOVER_DELAY_MS: u64 = 650;
const PLAYLIST_IMPORT_CHUNK_SIZE: usize = 512;
const DROP_IMPORT_BATCH_DELAY_MS: u64 = 80;
const COLLECTION_MODE_PLAYLIST: i32 = 0;
const COLLECTION_MODE_LIBRARY: i32 = 1;
const OPENSUBSONIC_PROFILE_ID: &str = "opensubsonic-default";

fn is_supported_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            SUPPORTED_AUDIO_EXTENSIONS
                .iter()
                .any(|supported| ext.eq_ignore_ascii_case(supported))
        })
        .unwrap_or(false)
}

fn find_opensubsonic_backend(config: &Config) -> Option<&BackendProfileConfig> {
    config
        .integrations
        .backends
        .iter()
        .find(|backend| backend.profile_id == OPENSUBSONIC_PROFILE_ID)
}

fn upsert_opensubsonic_backend_config(
    config: &mut Config,
    endpoint: &str,
    username: &str,
    enabled: bool,
) {
    let endpoint = endpoint.trim().trim_end_matches('/').to_string();
    let username = username.trim().to_string();
    if let Some(existing) = config
        .integrations
        .backends
        .iter_mut()
        .find(|backend| backend.profile_id == OPENSUBSONIC_PROFILE_ID)
    {
        existing.backend_kind = IntegrationBackendKind::OpenSubsonic;
        existing.display_name = "OpenSubsonic".to_string();
        existing.endpoint = endpoint;
        existing.username = username;
        existing.enabled = enabled;
        return;
    }
    config.integrations.backends.push(BackendProfileConfig {
        profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
        backend_kind: IntegrationBackendKind::OpenSubsonic,
        display_name: "OpenSubsonic".to_string(),
        endpoint,
        username,
        enabled,
    });
}

fn opensubsonic_profile_snapshot(
    config_backend: &BackendProfileConfig,
    status_text: Option<String>,
) -> protocol::BackendProfileSnapshot {
    protocol::BackendProfileSnapshot {
        profile_id: config_backend.profile_id.clone(),
        backend_kind: protocol::BackendKind::OpenSubsonic,
        display_name: config_backend.display_name.clone(),
        endpoint: config_backend.endpoint.clone(),
        username: config_backend.username.clone(),
        configured: !config_backend.endpoint.trim().is_empty()
            && !config_backend.username.trim().is_empty(),
        connection_state: protocol::BackendConnectionState::Disconnected,
        status_text,
    }
}

fn collect_audio_files_from_folder(folder_path: &Path) -> Vec<PathBuf> {
    let mut pending_directories = vec![folder_path.to_path_buf()];
    let mut tracks = Vec::new();

    while let Some(directory) = pending_directories.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(err) => {
                debug!("Failed to read directory {}: {}", directory.display(), err);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    debug!(
                        "Failed to read a directory entry in {}: {}",
                        directory.display(),
                        err
                    );
                    continue;
                }
            };

            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    debug!("Failed to inspect {}: {}", path.display(), err);
                    continue;
                }
            };

            if file_type.is_dir() {
                pending_directories.push(path);
                continue;
            }

            if file_type.is_file() && is_supported_audio_file(&path) {
                tracks.push(path);
            }
        }
    }

    tracks.sort_unstable();
    tracks
}

fn collect_audio_files_from_dropped_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut tracks = BTreeSet::new();
    for path in paths {
        if path.is_file() {
            if is_supported_audio_file(path) {
                tracks.insert(path.clone());
            }
            continue;
        }
        if path.is_dir() {
            for track in collect_audio_files_from_folder(path) {
                tracks.insert(track);
            }
        }
    }
    tracks.into_iter().collect()
}

fn collect_library_folders_from_dropped_paths(paths: &[PathBuf]) -> Vec<String> {
    let mut folders = BTreeSet::new();
    for path in paths {
        if path.is_dir() {
            folders.insert(path.to_string_lossy().to_string());
            continue;
        }
        if path.is_file() && is_supported_audio_file(path) {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    folders.insert(parent.to_string_lossy().to_string());
                }
            }
        }
    }
    folders.into_iter().collect()
}

fn enqueue_playlist_bulk_import(
    playlist_bulk_import_tx: &mpsc::SyncSender<protocol::PlaylistBulkImportRequest>,
    bus_sender: &broadcast::Sender<Message>,
    paths: &[PathBuf],
    source: protocol::ImportSource,
) -> usize {
    let mut queued = 0usize;
    for chunk in paths.chunks(PLAYLIST_IMPORT_CHUNK_SIZE) {
        if let Err(err) = playlist_bulk_import_tx.send(protocol::PlaylistBulkImportRequest {
            paths: chunk.to_vec(),
            source,
        }) {
            warn!(
                "Failed to enqueue import batch ({} track(s)): {}",
                chunk.len(),
                err
            );
            break;
        }
        queued += chunk.len();
        let _ = bus_sender.send(protocol::Message::Playlist(
            protocol::PlaylistMessage::DrainBulkImportQueue,
        ));
    }
    queued
}

#[derive(Clone)]
struct LibraryFolderImportContext {
    config_state: Arc<Mutex<Config>>,
    output_options: Arc<Mutex<OutputSettingsOptions>>,
    runtime_output_override: Arc<Mutex<RuntimeOutputOverride>>,
    runtime_audio_state: Arc<Mutex<RuntimeAudioState>>,
    last_runtime_signature: Arc<Mutex<OutputRuntimeSignature>>,
    config_file: PathBuf,
    layout_workspace_size: Arc<Mutex<(u32, u32)>>,
    ui_handle: slint::Weak<AppWindow>,
    bus_sender: broadcast::Sender<Message>,
}

fn add_library_folders_to_config_and_scan(
    context: &LibraryFolderImportContext,
    folders_to_add: Vec<String>,
) -> usize {
    let normalized_folders: Vec<String> = folders_to_add
        .into_iter()
        .map(|folder| folder.trim().to_string())
        .filter(|folder| !folder.is_empty())
        .collect();
    if normalized_folders.is_empty() {
        return 0;
    }

    let (next_config, added_count) = {
        let mut state = context
            .config_state
            .lock()
            .expect("config state lock poisoned");
        let mut next = state.clone();
        let mut added = 0usize;
        for folder in normalized_folders {
            if !next
                .library
                .folders
                .iter()
                .any(|existing| existing == &folder)
            {
                next.library.folders.push(folder);
                added += 1;
            }
        }
        if added == 0 {
            return 0;
        }
        next = sanitize_config(next);
        *state = next.clone();
        (next, added)
    };

    persist_state_files_with_config_path(&next_config, &context.config_file);
    let options_snapshot = {
        let options = context
            .output_options
            .lock()
            .expect("output options lock poisoned");
        options.clone()
    };
    let (workspace_width_px, workspace_height_px) =
        workspace_size_snapshot(&context.layout_workspace_size);
    if let Some(ui) = context.ui_handle.upgrade() {
        apply_config_to_ui(
            &ui,
            &next_config,
            &options_snapshot,
            workspace_width_px,
            workspace_height_px,
        );
    }

    let runtime_override = runtime_output_override_snapshot(&context.runtime_output_override);
    let runtime_config = resolve_effective_runtime_config(
        &next_config,
        &options_snapshot,
        Some(&runtime_override),
        &context.runtime_audio_state,
    );
    {
        let mut last_runtime = context
            .last_runtime_signature
            .lock()
            .expect("runtime signature lock poisoned");
        *last_runtime = OutputRuntimeSignature::from_output(&runtime_config.output);
    }
    let _ = context
        .bus_sender
        .send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
    let _ = context
        .bus_sender
        .send(Message::Library(protocol::LibraryMessage::RequestScan));

    added_count
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

fn choose_preferred_u16(detected: &BTreeSet<u16>, preferred_values: &[u16], fallback: u16) -> u16 {
    preferred_values
        .iter()
        .copied()
        .find(|value| detected.contains(value))
        .or_else(|| detected.get(&fallback).copied())
        .or_else(|| detected.iter().next().copied())
        .unwrap_or(fallback)
}

fn choose_preferred_u32(detected: &BTreeSet<u32>, preferred_values: &[u32], fallback: u32) -> u32 {
    preferred_values
        .iter()
        .copied()
        .find(|value| detected.contains(value))
        .or_else(|| detected.get(&fallback).copied())
        .or_else(|| detected.iter().next().copied())
        .unwrap_or(fallback)
}

fn resolve_runtime_config(
    config: &Config,
    output_options: &OutputSettingsOptions,
    runtime_output_override: Option<&RuntimeOutputOverride>,
) -> Config {
    let mut runtime = config.clone();
    if runtime.output.output_device_auto {
        runtime.output.output_device_name = output_options.auto_device_name.clone();
    }
    if runtime.output.channel_count_auto {
        runtime.output.channel_count = output_options.auto_channel_value;
    }
    if runtime.output.sample_rate_auto {
        runtime.output.sample_rate_khz = output_options.auto_sample_rate_value;
    }
    if runtime.output.bits_per_sample_auto {
        runtime.output.bits_per_sample = output_options.auto_bits_per_sample_value;
    }
    if let Some(sample_rate_hz) = runtime_output_override.and_then(|value| value.sample_rate_hz) {
        runtime.output.sample_rate_khz = sample_rate_hz.clamp(8_000, 192_000);
    }
    runtime
}

fn select_output_option_index_string(
    is_auto: bool,
    value: &str,
    values: &[String],
    custom_index: usize,
) -> usize {
    if is_auto {
        return 0;
    }
    values
        .iter()
        .position(|candidate| candidate == value)
        .map(|idx| idx + 1)
        .unwrap_or(custom_index)
}

fn select_output_option_index_u16(
    is_auto: bool,
    value: u16,
    values: &[u16],
    custom_index: usize,
) -> usize {
    if is_auto {
        return 0;
    }
    values
        .iter()
        .position(|candidate| *candidate == value)
        .map(|idx| idx + 1)
        .unwrap_or(custom_index)
}

fn select_manual_output_option_index_u32(value: u32, values: &[u32], custom_index: usize) -> usize {
    values
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(custom_index)
}

fn resampler_quality_to_str(value: ResamplerQuality) -> &'static str {
    match value {
        ResamplerQuality::High => "high",
        ResamplerQuality::Highest => "highest",
    }
}

fn is_likely_technical_device_name(name: &str) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let normalized = name.trim().to_ascii_lowercase();
    normalized == "default"
        || normalized.starts_with("sysdefault:")
        || normalized.starts_with("hw:")
        || normalized.starts_with("plughw:")
        || normalized.starts_with("front:")
        || normalized.starts_with("surround")
        || normalized.starts_with("dmix")
        || normalized.starts_with("dsnoop")
        || normalized.starts_with("iec958")
        || normalized.starts_with("pulse")
        || normalized.starts_with("jack")
        || normalized.contains("monitor of")
}

fn format_verified_rates_summary(rates: &[u32]) -> String {
    if rates.is_empty() {
        return "Verified rates: (none)".to_string();
    }
    let values = rates
        .iter()
        .map(|rate| {
            if rate % 1_000 == 0 {
                format!("{}k", rate / 1_000)
            } else {
                format!("{:.1}k", *rate as f64 / 1_000.0)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("Verified rates: {}", values)
}

fn runtime_output_override_snapshot(
    runtime_output_override: &Arc<Mutex<RuntimeOutputOverride>>,
) -> RuntimeOutputOverride {
    let state = runtime_output_override
        .lock()
        .expect("runtime output override lock poisoned");
    state.clone()
}

fn output_preferences_changed(previous: &OutputConfig, next: &OutputConfig) -> bool {
    previous.output_device_name != next.output_device_name
        || previous.output_device_auto != next.output_device_auto
        || previous.channel_count != next.channel_count
        || previous.sample_rate_khz != next.sample_rate_khz
        || previous.bits_per_sample != next.bits_per_sample
        || previous.channel_count_auto != next.channel_count_auto
        || previous.sample_rate_auto != next.sample_rate_auto
        || previous.bits_per_sample_auto != next.bits_per_sample_auto
        || previous.resampler_quality != next.resampler_quality
        || previous.dither_on_bitdepth_reduce != next.dither_on_bitdepth_reduce
        || previous.downmix_higher_channel_tracks != next.downmix_higher_channel_tracks
}

fn audio_settings_changed(previous: &Config, next: &Config) -> bool {
    output_preferences_changed(&previous.output, &next.output)
        || previous.cast.allow_transcode_fallback != next.cast.allow_transcode_fallback
}

fn snapshot_output_device_names(host: &cpal::Host) -> Vec<String> {
    let mut device_names = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                if !name.is_empty() {
                    device_names.push(name);
                }
            }
        }
    }
    device_names.sort();
    device_names.dedup();
    device_names
}

fn detect_output_settings_options(config: &Config) -> OutputSettingsOptions {
    const COMMON_SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
    const COMMON_CHANNEL_COUNTS: [u16; 4] = [2, 1, 6, 8];
    const COMMON_BIT_DEPTHS: [u16; 3] = [16, 24, 32];
    const PREFERRED_AUTO_SAMPLE_RATES: [u32; 6] =
        [96_000, 48_000, 44_100, 88_200, 192_000, 176_400];
    const PREFERRED_AUTO_CHANNEL_COUNTS: [u16; 4] = [2, 6, 1, 8];
    const PREFERRED_AUTO_BIT_DEPTHS: [u16; 3] = [24, 32, 16];

    let mut channels = BTreeSet::new();
    let mut sample_rates = BTreeSet::new();
    let mut bits_per_sample = BTreeSet::new();
    let mut verified_sample_rates = BTreeSet::new();
    let mut device_names;
    let mut auto_device_name = String::new();

    let host = cpal::default_host();
    if let Some(default_device) = host.default_output_device() {
        auto_device_name = default_device
            .name()
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_default();
    }
    device_names = snapshot_output_device_names(&host);
    if !config.output.output_device_name.trim().is_empty() {
        device_names.push(config.output.output_device_name.trim().to_string());
    }
    device_names.sort();
    device_names.dedup();
    let mut user_facing_device_names: Vec<String> = device_names
        .iter()
        .filter(|name| !is_likely_technical_device_name(name))
        .cloned()
        .collect();
    if user_facing_device_names.is_empty() {
        user_facing_device_names = device_names.clone();
    }
    if !config.output.output_device_name.trim().is_empty()
        && !user_facing_device_names
            .iter()
            .any(|name| name == config.output.output_device_name.trim())
    {
        user_facing_device_names.push(config.output.output_device_name.trim().to_string());
    }
    user_facing_device_names.sort();
    user_facing_device_names.dedup();

    let selected_device = if config.output.output_device_auto {
        host.default_output_device()
    } else {
        let requested_name = config.output.output_device_name.trim().to_string();
        let matching_device = if requested_name.is_empty() {
            None
        } else {
            host.output_devices().ok().and_then(|devices| {
                devices
                    .filter_map(|device| {
                        let name = device.name().ok()?;
                        if name == requested_name {
                            Some(device)
                        } else {
                            None
                        }
                    })
                    .next()
            })
        };
        matching_device.or_else(|| host.default_output_device())
    };

    if let Some(device) = selected_device {
        let probe_result =
            get_or_probe_output_device(&host, &device, config.output.sample_rate_khz.max(8_000));
        for verified_rate in probe_result.verified_sample_rates {
            verified_sample_rates.insert(verified_rate);
            sample_rates.insert(verified_rate);
        }
        if let Ok(configs) = device.supported_output_configs() {
            for output_config in configs {
                channels.insert(output_config.channels().max(1));
                bits_per_sample.insert((output_config.sample_format().sample_size() * 8) as u16);

                let min_rate = output_config.min_sample_rate().0;
                let max_rate = output_config.max_sample_rate().0;
                for common_rate in COMMON_SAMPLE_RATES {
                    if common_rate >= min_rate && common_rate <= max_rate {
                        sample_rates.insert(common_rate);
                    }
                }
                let configured_rate = config.output.sample_rate_khz.max(8_000);
                if configured_rate >= min_rate && configured_rate <= max_rate {
                    sample_rates.insert(configured_rate);
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
    let auto_channel_value = choose_preferred_u16(
        &channels,
        &PREFERRED_AUTO_CHANNEL_COUNTS,
        config.output.channel_count.max(1),
    );
    let auto_sample_rate_value = verified_sample_rates
        .iter()
        .next_back()
        .copied()
        .or_else(|| {
            Some(choose_preferred_u32(
                &sample_rates,
                &PREFERRED_AUTO_SAMPLE_RATES,
                config.output.sample_rate_khz.max(8_000),
            ))
        })
        .unwrap_or(config.output.sample_rate_khz.max(8_000));
    let auto_bits_per_sample_value = choose_preferred_u16(
        &bits_per_sample,
        &PREFERRED_AUTO_BIT_DEPTHS,
        config.output.bits_per_sample.max(8),
    );

    let verified_sample_rate_values: Vec<u32> = verified_sample_rates.iter().copied().collect();
    let verified_sample_rates_summary = format_verified_rates_summary(&verified_sample_rate_values);

    OutputSettingsOptions {
        device_names: user_facing_device_names,
        auto_device_name,
        channel_values,
        sample_rate_values,
        bits_per_sample_values,
        verified_sample_rate_values,
        verified_sample_rates_summary,
        auto_channel_value,
        auto_sample_rate_value,
        auto_bits_per_sample_value,
    }
}

fn sanitize_config(config: Config) -> Config {
    let sanitized_playlist_columns = sanitize_playlist_columns(&config.ui.playlist_columns);
    let output_device_name = config.output.output_device_name.trim().to_string();
    let clamped_channels = config.output.channel_count.clamp(1, 8);
    let clamped_sample_rate_hz = config.output.sample_rate_khz.clamp(8_000, 192_000);
    let clamped_bits = config.output.bits_per_sample.clamp(8, 32);
    let clamped_window_width = config.ui.window_width.clamp(600, 10_000);
    let clamped_window_height = config.ui.window_height.clamp(400, 10_000);
    let mut sanitized_layout = sanitize_layout_config(
        &config.ui.layout,
        clamped_window_width,
        clamped_window_height,
    );
    let sanitized_button_cluster_instances = sanitize_button_cluster_instances(
        &sanitized_layout,
        &sanitized_layout.button_cluster_instances,
    );
    let sanitized_viewer_panel_instances = sanitize_viewer_panel_instances(
        &sanitized_layout,
        &sanitized_layout.viewer_panel_instances,
    );
    let clamped_volume = config.ui.volume.clamp(0.0, 1.0);
    let mut clamped_album_art_column_min_width_px = config
        .ui
        .playlist_album_art_column_min_width_px
        .clamp(12, 512);
    let mut clamped_album_art_column_max_width_px = config
        .ui
        .playlist_album_art_column_max_width_px
        .clamp(24, 1024);
    if clamped_album_art_column_min_width_px > clamped_album_art_column_max_width_px {
        std::mem::swap(
            &mut clamped_album_art_column_min_width_px,
            &mut clamped_album_art_column_max_width_px,
        );
    }
    sanitized_layout.playlist_album_art_column_min_width_px = clamped_album_art_column_min_width_px;
    sanitized_layout.playlist_album_art_column_max_width_px = clamped_album_art_column_max_width_px;
    sanitized_layout.playlist_columns = sanitized_playlist_columns.clone();
    sanitized_layout.button_cluster_instances = sanitized_button_cluster_instances;
    sanitized_layout.viewer_panel_instances = sanitized_viewer_panel_instances;
    let clamped_low_watermark = config.buffering.player_low_watermark_ms.max(500);
    let clamped_target = config
        .buffering
        .player_target_buffer_ms
        .max(clamped_low_watermark.saturating_add(500))
        .clamp(1_000, 120_000);
    let clamped_interval = config.buffering.player_request_interval_ms.max(20);
    let clamped_decoder_chunk = config.buffering.decoder_request_chunk_ms.max(100);
    let mut sanitized_library_folders = Vec::new();
    let mut seen_folders = HashSet::new();
    for folder in config.library.folders {
        let trimmed = folder.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_ascii_lowercase();
        if seen_folders.insert(key) {
            sanitized_library_folders.push(trimmed.to_string());
        }
    }
    let clamped_artist_image_cache_ttl_days =
        config.library.artist_image_cache_ttl_days.clamp(1, 3650);
    let clamped_artist_image_cache_max_size_mb = config
        .library
        .artist_image_cache_max_size_mb
        .clamp(16, 16_384);
    let mut sanitized_backends = Vec::new();
    let mut seen_backend_ids = HashSet::new();
    for backend in config.integrations.backends {
        let trimmed_profile_id = backend.profile_id.trim();
        if trimmed_profile_id.is_empty() {
            continue;
        }
        if !seen_backend_ids.insert(trimmed_profile_id.to_ascii_lowercase()) {
            continue;
        }
        sanitized_backends.push(BackendProfileConfig {
            profile_id: trimmed_profile_id.to_string(),
            backend_kind: backend.backend_kind,
            display_name: backend.display_name.trim().to_string(),
            endpoint: backend.endpoint.trim().trim_end_matches('/').to_string(),
            username: backend.username.trim().to_string(),
            enabled: backend.enabled,
        });
    }

    Config {
        output: OutputConfig {
            output_device_name,
            output_device_auto: config.output.output_device_auto,
            channel_count: clamped_channels,
            sample_rate_khz: clamped_sample_rate_hz,
            bits_per_sample: clamped_bits,
            channel_count_auto: config.output.channel_count_auto,
            sample_rate_auto: config.output.sample_rate_auto,
            bits_per_sample_auto: config.output.bits_per_sample_auto,
            resampler_quality: config.output.resampler_quality,
            dither_on_bitdepth_reduce: config.output.dither_on_bitdepth_reduce,
            downmix_higher_channel_tracks: config.output.downmix_higher_channel_tracks,
        },
        cast: config.cast.clone(),
        ui: UiConfig {
            show_layout_edit_intro: config.ui.show_layout_edit_intro,
            show_tooltips: config.ui.show_tooltips,
            auto_scroll_to_playing_track: config.ui.auto_scroll_to_playing_track,
            dark_mode: config.ui.dark_mode,
            playlist_album_art_column_min_width_px: clamped_album_art_column_min_width_px,
            playlist_album_art_column_max_width_px: clamped_album_art_column_max_width_px,
            layout: sanitized_layout,
            playlist_columns: sanitized_playlist_columns,
            window_width: clamped_window_width,
            window_height: clamped_window_height,
            volume: clamped_volume,
            playback_order: config.ui.playback_order,
            repeat_mode: config.ui.repeat_mode,
        },
        library: LibraryConfig {
            folders: sanitized_library_folders,
            online_metadata_enabled: config.library.online_metadata_enabled,
            online_metadata_prompt_pending: config.library.online_metadata_prompt_pending,
            artist_image_cache_ttl_days: clamped_artist_image_cache_ttl_days,
            artist_image_cache_max_size_mb: clamped_artist_image_cache_max_size_mb,
        },
        buffering: BufferingConfig {
            player_low_watermark_ms: clamped_low_watermark,
            player_target_buffer_ms: clamped_target,
            player_request_interval_ms: clamped_interval,
            decoder_request_chunk_ms: clamped_decoder_chunk,
        },
        integrations: IntegrationsConfig {
            backends: sanitized_backends,
        },
    }
}

fn collect_button_cluster_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, panel } => {
            if *panel == LayoutPanelKind::ButtonCluster {
                out.push(id.clone());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_button_cluster_leaf_ids(first, out);
            collect_button_cluster_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn collect_viewer_panel_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, panel } => {
            if *panel == LayoutPanelKind::MetadataViewer
                || *panel == LayoutPanelKind::AlbumArtViewer
            {
                out.push(id.clone());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_viewer_panel_leaf_ids(first, out);
            collect_viewer_panel_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn button_cluster_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_button_cluster_leaf_ids(&layout.root, &mut ids);
    ids
}

fn viewer_panel_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_viewer_panel_leaf_ids(&layout.root, &mut ids);
    ids
}

fn collect_all_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, .. } => out.push(id.clone()),
        LayoutNode::Split { first, second, .. } => {
            collect_all_leaf_ids(first, out);
            collect_all_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn layout_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_all_leaf_ids(&layout.root, &mut ids);
    ids
}

fn newly_added_leaf_ids(previous_layout: &LayoutConfig, next_layout: &LayoutConfig) -> Vec<String> {
    let previous_ids: HashSet<String> = layout_leaf_ids(previous_layout).into_iter().collect();
    layout_leaf_ids(next_layout)
        .into_iter()
        .filter(|id| !previous_ids.contains(id))
        .collect()
}

fn sanitize_button_actions(actions: &[i32]) -> Vec<i32> {
    actions
        .iter()
        .copied()
        .filter(|action| (1..=11).contains(action))
        .collect()
}

fn default_button_cluster_actions_by_index(index: usize) -> Vec<i32> {
    match index {
        0 => IMPORT_CLUSTER_PRESET.to_vec(),
        1 => TRANSPORT_CLUSTER_PRESET.to_vec(),
        2 => UTILITY_CLUSTER_PRESET.to_vec(),
        _ => Vec::new(),
    }
}

fn sanitize_button_cluster_instances(
    layout: &LayoutConfig,
    instances: &[ButtonClusterInstanceConfig],
) -> Vec<ButtonClusterInstanceConfig> {
    let leaf_ids = button_cluster_leaf_ids(layout);
    let had_existing_instances = !instances.is_empty();
    let mut actions_by_leaf: HashMap<&str, Vec<i32>> = HashMap::new();
    for instance in instances {
        let leaf_id = instance.leaf_id.trim();
        if leaf_id.is_empty() {
            continue;
        }
        actions_by_leaf.insert(leaf_id, sanitize_button_actions(&instance.actions));
    }

    leaf_ids
        .iter()
        .enumerate()
        .map(|(index, leaf_id)| ButtonClusterInstanceConfig {
            leaf_id: leaf_id.clone(),
            actions: actions_by_leaf
                .get(leaf_id.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    if had_existing_instances {
                        Vec::new()
                    } else {
                        default_button_cluster_actions_by_index(index)
                    }
                }),
        })
        .collect()
}

fn sanitize_viewer_panel_instances(
    layout: &LayoutConfig,
    instances: &[ViewerPanelInstanceConfig],
) -> Vec<ViewerPanelInstanceConfig> {
    let leaf_ids = viewer_panel_leaf_ids(layout);
    let mut priority_by_leaf: HashMap<&str, ViewerPanelDisplayPriority> = HashMap::new();
    let mut metadata_source_by_leaf: HashMap<&str, ViewerPanelMetadataSource> = HashMap::new();
    let mut image_source_by_leaf: HashMap<&str, ViewerPanelImageSource> = HashMap::new();
    for instance in instances {
        let leaf_id = instance.leaf_id.trim();
        if leaf_id.is_empty() {
            continue;
        }
        priority_by_leaf.insert(leaf_id, instance.display_priority);
        metadata_source_by_leaf.insert(leaf_id, instance.metadata_source);
        image_source_by_leaf.insert(leaf_id, instance.image_source);
    }

    leaf_ids
        .into_iter()
        .map(|leaf_id| ViewerPanelInstanceConfig {
            display_priority: priority_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            metadata_source: metadata_source_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            image_source: image_source_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            leaf_id,
        })
        .collect()
}

fn normalize_column_format(format: &str) -> String {
    format.trim().to_ascii_lowercase()
}

fn is_album_art_builtin_column(column: &PlaylistColumnConfig) -> bool {
    !column.custom && normalize_column_format(&column.format) == "{album_art}"
}

fn playlist_column_kind(column: &PlaylistColumnConfig) -> i32 {
    if is_album_art_builtin_column(column) {
        PLAYLIST_COLUMN_KIND_ALBUM_ART
    } else {
        PLAYLIST_COLUMN_KIND_TEXT
    }
}

fn visible_playlist_column_kinds(columns: &[PlaylistColumnConfig]) -> Vec<i32> {
    columns
        .iter()
        .filter(|column| column.enabled)
        .map(playlist_column_kind)
        .collect()
}

fn sanitize_playlist_columns(columns: &[PlaylistColumnConfig]) -> Vec<PlaylistColumnConfig> {
    let default_columns = config::default_playlist_columns();
    let mut seen_builtin_keys: HashSet<String> = HashSet::new();
    let mut seen_custom_keys: HashSet<String> = HashSet::new();
    let mut merged_columns: Vec<PlaylistColumnConfig> = Vec::new();

    let mut default_name_by_key: HashMap<String, String> = HashMap::new();
    for default_column in &default_columns {
        default_name_by_key.insert(
            normalize_column_format(&default_column.format),
            default_column.name.clone(),
        );
    }

    for column in columns {
        let trimmed_name = column.name.trim();
        let trimmed_format = column.format.trim();
        if trimmed_name.is_empty() || trimmed_format.is_empty() {
            continue;
        }

        if column.custom {
            let custom_key = format!("{}|{}", trimmed_name, trimmed_format);
            if seen_custom_keys.insert(custom_key) {
                merged_columns.push(PlaylistColumnConfig {
                    name: trimmed_name.to_string(),
                    format: trimmed_format.to_string(),
                    enabled: column.enabled,
                    custom: true,
                });
            }
        } else {
            let builtin_key = normalize_column_format(trimmed_format);
            if default_name_by_key.contains_key(&builtin_key)
                && seen_builtin_keys.insert(builtin_key.clone())
            {
                let builtin_name = default_name_by_key
                    .get(&builtin_key)
                    .cloned()
                    .unwrap_or_else(|| trimmed_name.to_string());
                merged_columns.push(PlaylistColumnConfig {
                    name: builtin_name,
                    format: trimmed_format.to_string(),
                    enabled: column.enabled,
                    custom: false,
                });
            }
        }
    }

    for default_column in default_columns {
        let key = normalize_column_format(&default_column.format);
        if !seen_builtin_keys.contains(&key) {
            merged_columns.push(default_column);
        }
    }

    if merged_columns.iter().all(|column| !column.enabled) {
        if let Some(first_column) = merged_columns.first_mut() {
            first_column.enabled = true;
        }
    }

    merged_columns
}

fn playlist_column_key(column: &PlaylistColumnConfig) -> String {
    if column.custom {
        format!("custom:{}|{}", column.name.trim(), column.format.trim())
    } else {
        normalize_column_format(&column.format)
    }
}

fn playlist_column_order_keys(columns: &[PlaylistColumnConfig]) -> Vec<String> {
    columns.iter().map(playlist_column_key).collect()
}

/// Min/max width constraints for one visible playlist column.
#[derive(Clone, Copy)]
struct ColumnWidthBounds {
    min_px: i32,
    max_px: i32,
}

fn album_art_column_width_bounds(ui: &UiConfig) -> ColumnWidthBounds {
    ColumnWidthBounds {
        min_px: ui.playlist_album_art_column_min_width_px as i32,
        max_px: ui.playlist_album_art_column_max_width_px as i32,
    }
}

#[cfg(test)]
fn default_album_art_column_width_bounds() -> ColumnWidthBounds {
    ColumnWidthBounds {
        min_px: config::default_playlist_album_art_column_min_width_px() as i32,
        max_px: config::default_playlist_album_art_column_max_width_px() as i32,
    }
}

fn playlist_column_width_bounds_with_album_art(
    column: &PlaylistColumnConfig,
    album_art_bounds: ColumnWidthBounds,
) -> ColumnWidthBounds {
    if is_album_art_builtin_column(column) {
        return album_art_bounds;
    }

    let normalized_format = column.format.trim().to_ascii_lowercase();
    let normalized_name = column.name.trim().to_ascii_lowercase();

    if normalized_format == "{track}"
        || normalized_format == "{track_number}"
        || normalized_format == "{tracknumber}"
        || normalized_name == "track #"
        || normalized_name == "track"
    {
        return ColumnWidthBounds {
            min_px: 52,
            max_px: 90,
        };
    }

    if normalized_format == "{disc}" || normalized_format == "{disc_number}" {
        return ColumnWidthBounds {
            min_px: 50,
            max_px: 84,
        };
    }

    if normalized_format == "{year}" || normalized_format == "{date}" || normalized_name == "year" {
        return ColumnWidthBounds {
            min_px: 64,
            max_px: 104,
        };
    }

    if normalized_format == "{title}" || normalized_name == "title" {
        return ColumnWidthBounds {
            min_px: 140,
            max_px: 440,
        };
    }

    if normalized_format == "{artist}"
        || normalized_format == "{album_artist}"
        || normalized_format == "{albumartist}"
        || normalized_name == "artist"
        || normalized_name == "album artist"
    {
        return ColumnWidthBounds {
            min_px: 120,
            max_px: 320,
        };
    }

    if normalized_format == "{album}" || normalized_name == "album" {
        return ColumnWidthBounds {
            min_px: 140,
            max_px: 360,
        };
    }

    if normalized_format == "{genre}" || normalized_name == "genre" {
        return ColumnWidthBounds {
            min_px: 100,
            max_px: 210,
        };
    }

    if normalized_name == "duration"
        || normalized_name == "time"
        || normalized_format == "{duration}"
    {
        return ColumnWidthBounds {
            min_px: 78,
            max_px: 120,
        };
    }

    if column.custom {
        return ColumnWidthBounds {
            min_px: 110,
            max_px: 340,
        };
    }

    ColumnWidthBounds {
        min_px: 100,
        max_px: 300,
    }
}

#[cfg(test)]
fn playlist_column_width_bounds(column: &PlaylistColumnConfig) -> ColumnWidthBounds {
    playlist_column_width_bounds_with_album_art(column, default_album_art_column_width_bounds())
}

fn playlist_column_bounds_at_visible_index(
    columns: &[PlaylistColumnConfig],
    visible_index: usize,
    album_art_bounds: ColumnWidthBounds,
) -> Option<ColumnWidthBounds> {
    columns
        .iter()
        .filter(|column| column.enabled)
        .nth(visible_index)
        .map(|column| playlist_column_width_bounds_with_album_art(column, album_art_bounds))
}

fn playlist_column_key_at_visible_index(
    columns: &[PlaylistColumnConfig],
    visible_index: usize,
) -> Option<String> {
    columns
        .iter()
        .filter(|column| column.enabled)
        .nth(visible_index)
        .map(playlist_column_key)
}

fn clamp_width_for_visible_column(
    columns: &[PlaylistColumnConfig],
    visible_index: usize,
    width_px: i32,
    album_art_bounds: ColumnWidthBounds,
) -> Option<i32> {
    let bounds = playlist_column_bounds_at_visible_index(columns, visible_index, album_art_bounds)?;
    Some(width_px.clamp(bounds.min_px, bounds.max_px.max(bounds.min_px)))
}

fn reorder_visible_playlist_columns(
    columns: &[PlaylistColumnConfig],
    from_visible_index: usize,
    to_visible_index: usize,
) -> Vec<PlaylistColumnConfig> {
    let visible_count = columns.iter().filter(|column| column.enabled).count();
    if visible_count < 2
        || from_visible_index >= visible_count
        || to_visible_index >= visible_count
        || from_visible_index == to_visible_index
    {
        return columns.to_vec();
    }

    let mut visible_columns: Vec<PlaylistColumnConfig> = columns
        .iter()
        .filter(|column| column.enabled)
        .cloned()
        .collect();
    let moved_column = visible_columns.remove(from_visible_index);
    visible_columns.insert(to_visible_index, moved_column);

    let mut visible_iter = visible_columns.into_iter();
    let mut reordered = Vec::with_capacity(columns.len());
    for column in columns {
        if column.enabled {
            if let Some(next_visible) = visible_iter.next() {
                reordered.push(next_visible);
            }
        } else {
            reordered.push(column.clone());
        }
    }
    reordered
}

fn apply_column_order_keys(
    columns: &[PlaylistColumnConfig],
    column_order_keys: &[String],
) -> Vec<PlaylistColumnConfig> {
    if column_order_keys.is_empty() {
        return columns.to_vec();
    }

    let mut remaining: Vec<PlaylistColumnConfig> = columns.to_vec();
    let mut reordered: Vec<PlaylistColumnConfig> = Vec::with_capacity(columns.len());

    for key in column_order_keys {
        if let Some(index) = remaining
            .iter()
            .position(|column| playlist_column_key(column) == *key)
        {
            reordered.push(remaining.remove(index));
        }
    }

    reordered.extend(remaining);
    reordered
}

fn playlist_column_widths_from_model(widths_model: ModelRc<i32>) -> Vec<i32> {
    (0..widths_model.row_count())
        .filter_map(|index| widths_model.row_data(index))
        .collect()
}

fn resolve_playlist_header_column_from_x(mouse_x_px: i32, widths_px: &[i32]) -> i32 {
    if mouse_x_px < 0 {
        return -1;
    }
    let mut start_px: i32 = 0;
    for (index, width_px) in widths_px.iter().enumerate() {
        let column_width = (*width_px).max(0);
        let end_px = start_px.saturating_add(column_width);
        if mouse_x_px >= start_px && mouse_x_px < end_px {
            return index as i32;
        }
        let next_start = end_px.saturating_add(10);
        if mouse_x_px < next_start {
            return -1;
        }
        start_px = next_start;
    }
    -1
}

fn resolve_playlist_header_gap_from_x(mouse_x_px: i32, widths_px: &[i32]) -> i32 {
    if widths_px.is_empty() {
        return -1;
    }
    if mouse_x_px < 0 {
        return 0;
    }

    let mut start_px: i32 = 0;
    for (index, width_px) in widths_px.iter().enumerate() {
        let column_width = (*width_px).max(0);
        let end_px = start_px.saturating_add(column_width);
        let midpoint_px = start_px.saturating_add(column_width / 2);

        if mouse_x_px < start_px {
            return index as i32;
        }
        if mouse_x_px < end_px {
            return if mouse_x_px < midpoint_px {
                index as i32
            } else {
                index as i32 + 1
            };
        }

        let next_start = end_px.saturating_add(10);
        if mouse_x_px < next_start {
            return index as i32 + 1;
        }
        start_px = next_start;
    }
    widths_px.len() as i32
}

fn resolve_playlist_header_divider_from_x(
    mouse_x_px: i32,
    widths_px: &[i32],
    hit_tolerance_px: i32,
) -> i32 {
    if mouse_x_px < 0 || widths_px.is_empty() {
        return -1;
    }
    let tolerance = hit_tolerance_px.max(1);
    let mut start_px: i32 = 0;
    for (index, width_px) in widths_px.iter().enumerate() {
        let column_width = (*width_px).max(0);
        let end_px = start_px.saturating_add(column_width);
        if (mouse_x_px - end_px).abs() <= tolerance {
            return index as i32;
        }
        start_px = end_px.saturating_add(10);
    }
    -1
}

fn sidebar_width_from_window(window_width_px: u32) -> i32 {
    let proportional = ((window_width_px as f32) * 0.24).round() as u32;
    proportional.clamp(180, 300) as i32
}

fn workspace_size_snapshot(workspace_size: &Arc<Mutex<(u32, u32)>>) -> (u32, u32) {
    let (width, height) = *workspace_size
        .lock()
        .expect("layout workspace size lock poisoned");
    (width.max(1), height.max(1))
}

fn quantize_splitter_ratio_to_precision(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return ratio;
    }
    (ratio * 100.0).round() / 100.0
}

fn layout_panel_options() -> Vec<slint::SharedString> {
    vec![
        "None".into(),
        "Button Cluster".into(),
        "Transport Button Cluster".into(),
        "Utility Button Cluster".into(),
        "Volume Slider".into(),
        "Seek Bar".into(),
        "Playlist Switcher".into(),
        "Track List".into(),
        "Metadata Viewer".into(),
        "Image Metadata Viewer".into(),
        "Spacer".into(),
        "Status Bar".into(),
        "Import Button Cluster".into(),
    ]
}

fn resolve_layout_panel_selection(panel_code: i32) -> (LayoutPanelKind, Option<Vec<i32>>) {
    if panel_code == LayoutPanelKind::TransportButtonCluster.to_code() {
        return (
            LayoutPanelKind::ButtonCluster,
            Some(TRANSPORT_CLUSTER_PRESET.to_vec()),
        );
    }
    if panel_code == LayoutPanelKind::UtilityButtonCluster.to_code() {
        return (
            LayoutPanelKind::ButtonCluster,
            Some(UTILITY_CLUSTER_PRESET.to_vec()),
        );
    }
    if panel_code == LayoutPanelKind::ImportButtonCluster.to_code() {
        return (
            LayoutPanelKind::ButtonCluster,
            Some(IMPORT_CLUSTER_PRESET.to_vec()),
        );
    }
    (LayoutPanelKind::from_code(panel_code), None)
}

fn button_cluster_actions_for_leaf(
    instances: &[ButtonClusterInstanceConfig],
    leaf_id: &str,
) -> Vec<i32> {
    instances
        .iter()
        .find(|instance| instance.leaf_id == leaf_id)
        .map(|instance| instance.actions.clone())
        .unwrap_or_default()
}

fn upsert_button_cluster_actions_for_leaf(
    instances: &mut Vec<ButtonClusterInstanceConfig>,
    leaf_id: &str,
    actions: Vec<i32>,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.actions = sanitize_button_actions(&actions);
        return;
    }
    instances.push(ButtonClusterInstanceConfig {
        leaf_id: leaf_id.to_string(),
        actions: sanitize_button_actions(&actions),
    });
}

fn remove_button_cluster_instance_for_leaf(
    instances: &mut Vec<ButtonClusterInstanceConfig>,
    leaf_id: &str,
) {
    instances.retain(|instance| instance.leaf_id != leaf_id);
}

fn upsert_viewer_panel_display_priority_for_leaf(
    instances: &mut Vec<ViewerPanelInstanceConfig>,
    leaf_id: &str,
    display_priority: ViewerPanelDisplayPriority,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.display_priority = display_priority;
        return;
    }
    instances.push(ViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority,
        metadata_source: ViewerPanelMetadataSource::default(),
        image_source: ViewerPanelImageSource::default(),
    });
}

fn upsert_viewer_panel_metadata_source_for_leaf(
    instances: &mut Vec<ViewerPanelInstanceConfig>,
    leaf_id: &str,
    metadata_source: ViewerPanelMetadataSource,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.metadata_source = metadata_source;
        return;
    }
    instances.push(ViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority: ViewerPanelDisplayPriority::default(),
        metadata_source,
        image_source: ViewerPanelImageSource::default(),
    });
}

fn upsert_viewer_panel_image_source_for_leaf(
    instances: &mut Vec<ViewerPanelInstanceConfig>,
    leaf_id: &str,
    image_source: ViewerPanelImageSource,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.image_source = image_source;
        return;
    }
    instances.push(ViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority: ViewerPanelDisplayPriority::default(),
        metadata_source: ViewerPanelMetadataSource::default(),
        image_source,
    });
}

fn apply_control_cluster_menu_actions_for_leaf(ui: &AppWindow, config: &Config, leaf_id: &str) {
    let actions =
        button_cluster_actions_for_leaf(&config.ui.layout.button_cluster_instances, leaf_id);
    ui.set_control_cluster_menu_actions(ModelRc::from(Rc::new(VecModel::from(actions))));
}

fn apply_button_cluster_views_to_ui(
    ui: &AppWindow,
    metrics: &layout::TreeLayoutMetrics,
    config: &Config,
) {
    let views: Vec<LayoutButtonClusterPanelModel> = metrics
        .leaves
        .iter()
        .filter(|leaf| leaf.panel == LayoutPanelKind::ButtonCluster)
        .map(|leaf| LayoutButtonClusterPanelModel {
            leaf_id: leaf.id.clone().into(),
            x_px: leaf.x,
            y_px: leaf.y,
            width_px: leaf.width,
            height_px: leaf.height,
            visible: true,
            actions: ModelRc::from(Rc::new(VecModel::from(button_cluster_actions_for_leaf(
                &config.ui.layout.button_cluster_instances,
                &leaf.id,
            )))),
        })
        .collect();
    ui.set_layout_button_cluster_panels(ModelRc::from(Rc::new(VecModel::from(views))));
}

fn viewer_panel_display_priority_to_code(priority: ViewerPanelDisplayPriority) -> i32 {
    match priority {
        ViewerPanelDisplayPriority::Default => 0,
        ViewerPanelDisplayPriority::PreferSelection => 1,
        ViewerPanelDisplayPriority::PreferNowPlaying => 2,
        ViewerPanelDisplayPriority::SelectionOnly => 3,
        ViewerPanelDisplayPriority::NowPlayingOnly => 4,
    }
}

fn viewer_panel_display_priority_from_code(priority: i32) -> ViewerPanelDisplayPriority {
    match priority {
        1 => ViewerPanelDisplayPriority::PreferSelection,
        2 => ViewerPanelDisplayPriority::PreferNowPlaying,
        3 => ViewerPanelDisplayPriority::SelectionOnly,
        4 => ViewerPanelDisplayPriority::NowPlayingOnly,
        _ => ViewerPanelDisplayPriority::Default,
    }
}

fn viewer_panel_metadata_source_to_code(source: ViewerPanelMetadataSource) -> i32 {
    match source {
        ViewerPanelMetadataSource::Track => 0,
        ViewerPanelMetadataSource::AlbumDescription => 1,
        ViewerPanelMetadataSource::ArtistBio => 2,
    }
}

fn viewer_panel_metadata_source_from_code(source: i32) -> ViewerPanelMetadataSource {
    match source {
        1 => ViewerPanelMetadataSource::AlbumDescription,
        2 => ViewerPanelMetadataSource::ArtistBio,
        _ => ViewerPanelMetadataSource::Track,
    }
}

fn viewer_panel_image_source_to_code(source: ViewerPanelImageSource) -> i32 {
    match source {
        ViewerPanelImageSource::AlbumArt => 0,
        ViewerPanelImageSource::ArtistImage => 1,
    }
}

fn viewer_panel_image_source_from_code(source: i32) -> ViewerPanelImageSource {
    match source {
        1 => ViewerPanelImageSource::ArtistImage,
        _ => ViewerPanelImageSource::AlbumArt,
    }
}

fn viewer_panel_instance_for_leaf<'a>(
    instances: &'a [ViewerPanelInstanceConfig],
    leaf_id: &str,
) -> Option<&'a ViewerPanelInstanceConfig> {
    instances
        .iter()
        .find(|instance| instance.leaf_id == leaf_id)
}

fn viewer_panel_display_priority_for_leaf(
    instances: &[ViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelDisplayPriority {
    viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.display_priority)
        .unwrap_or_default()
}

fn viewer_panel_metadata_source_for_leaf(
    instances: &[ViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelMetadataSource {
    viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.metadata_source)
        .unwrap_or_default()
}

fn viewer_panel_image_source_for_leaf(
    instances: &[ViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelImageSource {
    viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.image_source)
        .unwrap_or_default()
}

fn apply_viewer_panel_views_to_ui(
    ui: &AppWindow,
    metrics: &layout::TreeLayoutMetrics,
    config: &Config,
) {
    let default_display_title = ui.get_display_title();
    let default_display_artist = ui.get_display_artist();
    let default_display_album = ui.get_display_album();
    let default_display_date = ui.get_display_date();
    let default_display_genre = ui.get_display_genre();
    let default_art_source = ui.get_current_cover_art();
    let default_has_art = ui.get_current_cover_art_available();
    let existing_metadata_by_leaf: HashMap<String, LayoutMetadataViewerPanelModel> = ui
        .get_layout_metadata_viewer_panels()
        .iter()
        .map(|panel| (panel.leaf_id.to_string(), panel))
        .collect();
    let existing_album_art_by_leaf: HashMap<String, LayoutAlbumArtViewerPanelModel> = ui
        .get_layout_album_art_viewer_panels()
        .iter()
        .map(|panel| (panel.leaf_id.to_string(), panel))
        .collect();

    let metadata_views: Vec<LayoutMetadataViewerPanelModel> = metrics
        .leaves
        .iter()
        .filter(|leaf| leaf.panel == LayoutPanelKind::MetadataViewer)
        .map(|leaf| {
            let display_priority =
                viewer_panel_display_priority_to_code(viewer_panel_display_priority_for_leaf(
                    &config.ui.layout.viewer_panel_instances,
                    &leaf.id,
                ));
            let metadata_source =
                viewer_panel_metadata_source_to_code(viewer_panel_metadata_source_for_leaf(
                    &config.ui.layout.viewer_panel_instances,
                    &leaf.id,
                ));
            if let Some(existing) = existing_metadata_by_leaf.get(&leaf.id) {
                return LayoutMetadataViewerPanelModel {
                    leaf_id: leaf.id.clone().into(),
                    x_px: leaf.x,
                    y_px: leaf.y,
                    width_px: leaf.width,
                    height_px: leaf.height,
                    visible: true,
                    display_priority,
                    metadata_source,
                    display_title: existing.display_title.clone(),
                    display_artist: existing.display_artist.clone(),
                    display_album: existing.display_album.clone(),
                    display_date: existing.display_date.clone(),
                    display_genre: existing.display_genre.clone(),
                };
            }
            LayoutMetadataViewerPanelModel {
                leaf_id: leaf.id.clone().into(),
                x_px: leaf.x,
                y_px: leaf.y,
                width_px: leaf.width,
                height_px: leaf.height,
                visible: true,
                display_priority,
                metadata_source,
                display_title: default_display_title.clone(),
                display_artist: default_display_artist.clone(),
                display_album: default_display_album.clone(),
                display_date: default_display_date.clone(),
                display_genre: default_display_genre.clone(),
            }
        })
        .collect();

    let album_art_views: Vec<LayoutAlbumArtViewerPanelModel> = metrics
        .leaves
        .iter()
        .filter(|leaf| leaf.panel == LayoutPanelKind::AlbumArtViewer)
        .map(|leaf| {
            let display_priority =
                viewer_panel_display_priority_to_code(viewer_panel_display_priority_for_leaf(
                    &config.ui.layout.viewer_panel_instances,
                    &leaf.id,
                ));
            let image_source =
                viewer_panel_image_source_to_code(viewer_panel_image_source_for_leaf(
                    &config.ui.layout.viewer_panel_instances,
                    &leaf.id,
                ));
            if let Some(existing) = existing_album_art_by_leaf.get(&leaf.id) {
                return LayoutAlbumArtViewerPanelModel {
                    leaf_id: leaf.id.clone().into(),
                    x_px: leaf.x,
                    y_px: leaf.y,
                    width_px: leaf.width,
                    height_px: leaf.height,
                    visible: true,
                    display_priority,
                    image_source,
                    art_source: existing.art_source.clone(),
                    has_art: existing.has_art,
                };
            }
            LayoutAlbumArtViewerPanelModel {
                leaf_id: leaf.id.clone().into(),
                x_px: leaf.x,
                y_px: leaf.y,
                width_px: leaf.width,
                height_px: leaf.height,
                visible: true,
                display_priority,
                image_source,
                art_source: default_art_source.clone(),
                has_art: default_has_art,
            }
        })
        .collect();

    ui.set_layout_metadata_viewer_panels(ModelRc::from(Rc::new(VecModel::from(metadata_views))));
    ui.set_layout_album_art_viewer_panels(ModelRc::from(Rc::new(VecModel::from(album_art_views))));
}

fn set_table_value_preserving_decor(table: &mut Table, key: &str, item: Item) {
    let replacing_scalar_with_aot = item.is_array_of_tables()
        && table
            .get(key)
            .is_some_and(|current| !current.is_array_of_tables());
    if replacing_scalar_with_aot {
        table.remove(key);
        table[key] = item;
        return;
    }

    let existing_value_decor = table
        .get(key)
        .and_then(|current| current.as_value().map(|value| value.decor().clone()));
    table[key] = item;
    if let Some(existing_value_decor) = existing_value_decor {
        if let Some(next_value) = table[key].as_value_mut() {
            *next_value.decor_mut() = existing_value_decor;
        }
    }
}

fn set_table_scalar_if_changed<T, F>(
    table: &mut Table,
    key: &str,
    previous_value: T,
    next_value: T,
    to_item: F,
) where
    T: PartialEq + Copy,
    F: FnOnce(T) -> Item,
{
    if table.contains_key(key) && previous_value == next_value {
        return;
    }
    set_table_value_preserving_decor(table, key, to_item(next_value));
}

fn ensure_section_table(document: &mut DocumentMut, key: &str) {
    let root = document.as_table_mut();
    let should_replace = !matches!(root.get(key), Some(item) if item.is_table());
    if should_replace {
        root.insert(key, Item::Table(Table::new()));
    }
}

fn write_config_to_document(document: &mut DocumentMut, previous: &Config, config: &Config) {
    ensure_section_table(document, "output");
    ensure_section_table(document, "cast");
    ensure_section_table(document, "ui");
    ensure_section_table(document, "library");
    ensure_section_table(document, "buffering");
    ensure_section_table(document, "integrations");

    {
        let output = document["output"]
            .as_table_mut()
            .expect("output should be a table");
        if !output.contains_key("output_device_name")
            || previous.output.output_device_name != config.output.output_device_name
        {
            set_table_value_preserving_decor(
                output,
                "output_device_name",
                value(config.output.output_device_name.clone()),
            );
        }
        set_table_scalar_if_changed(
            output,
            "output_device_auto",
            previous.output.output_device_auto,
            config.output.output_device_auto,
            value,
        );
        set_table_scalar_if_changed(
            output,
            "channel_count",
            i64::from(previous.output.channel_count),
            i64::from(config.output.channel_count),
            value,
        );
        set_table_scalar_if_changed(
            output,
            "sample_rate_khz",
            i64::from(previous.output.sample_rate_khz),
            i64::from(config.output.sample_rate_khz),
            value,
        );
        set_table_scalar_if_changed(
            output,
            "bits_per_sample",
            i64::from(previous.output.bits_per_sample),
            i64::from(config.output.bits_per_sample),
            value,
        );
        set_table_scalar_if_changed(
            output,
            "channel_count_auto",
            previous.output.channel_count_auto,
            config.output.channel_count_auto,
            value,
        );
        set_table_scalar_if_changed(
            output,
            "sample_rate_auto",
            previous.output.sample_rate_auto,
            config.output.sample_rate_auto,
            value,
        );
        set_table_scalar_if_changed(
            output,
            "bits_per_sample_auto",
            previous.output.bits_per_sample_auto,
            config.output.bits_per_sample_auto,
            value,
        );
        if !output.contains_key("resampler_quality")
            || previous.output.resampler_quality != config.output.resampler_quality
        {
            set_table_value_preserving_decor(
                output,
                "resampler_quality",
                value(resampler_quality_to_str(config.output.resampler_quality)),
            );
        }
        if !output.contains_key("dither_on_bitdepth_reduce")
            || previous.output.dither_on_bitdepth_reduce != config.output.dither_on_bitdepth_reduce
        {
            set_table_value_preserving_decor(
                output,
                "dither_on_bitdepth_reduce",
                value(config.output.dither_on_bitdepth_reduce),
            );
        }
        if !output.contains_key("downmix_higher_channel_tracks")
            || previous.output.downmix_higher_channel_tracks
                != config.output.downmix_higher_channel_tracks
        {
            set_table_value_preserving_decor(
                output,
                "downmix_higher_channel_tracks",
                value(config.output.downmix_higher_channel_tracks),
            );
        }
    }

    {
        let cast = document["cast"]
            .as_table_mut()
            .expect("cast should be a table");
        set_table_scalar_if_changed(
            cast,
            "allow_transcode_fallback",
            previous.cast.allow_transcode_fallback,
            config.cast.allow_transcode_fallback,
            value,
        );
    }

    {
        let ui = document["ui"].as_table_mut().expect("ui should be a table");
        set_table_scalar_if_changed(
            ui,
            "show_layout_edit_intro",
            previous.ui.show_layout_edit_intro,
            config.ui.show_layout_edit_intro,
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "show_tooltips",
            previous.ui.show_tooltips,
            config.ui.show_tooltips,
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "auto_scroll_to_playing_track",
            previous.ui.auto_scroll_to_playing_track,
            config.ui.auto_scroll_to_playing_track,
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "dark_mode",
            previous.ui.dark_mode,
            config.ui.dark_mode,
            value,
        );
        // Layout-owned state is persisted in layout.toml only.
        ui.remove("button_cluster_instances");
        ui.remove("playlist_columns");
        ui.remove("playlist_album_art_column_min_width_px");
        ui.remove("playlist_album_art_column_max_width_px");
        ui.remove("layout");
        ui.remove("viewer_panel_instances");
        set_table_scalar_if_changed(
            ui,
            "window_width",
            i64::from(previous.ui.window_width),
            i64::from(config.ui.window_width),
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "window_height",
            i64::from(previous.ui.window_height),
            i64::from(config.ui.window_height),
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "volume",
            f64::from(previous.ui.volume),
            f64::from(config.ui.volume),
            value,
        );
        if !ui.contains_key("playback_order")
            || previous.ui.playback_order != config.ui.playback_order
        {
            let playback_order = match config.ui.playback_order {
                UiPlaybackOrder::Default => "default",
                UiPlaybackOrder::Shuffle => "shuffle",
                UiPlaybackOrder::Random => "random",
            };
            set_table_value_preserving_decor(ui, "playback_order", value(playback_order));
        }
        if !ui.contains_key("repeat_mode") || previous.ui.repeat_mode != config.ui.repeat_mode {
            let repeat_mode = match config.ui.repeat_mode {
                UiRepeatMode::Off => "off",
                UiRepeatMode::Playlist => "playlist",
                UiRepeatMode::Track => "track",
            };
            set_table_value_preserving_decor(ui, "repeat_mode", value(repeat_mode));
        }
    }

    {
        let library = document["library"]
            .as_table_mut()
            .expect("library should be a table");
        library.remove("show_first_start_prompt");
        set_table_scalar_if_changed(
            library,
            "online_metadata_enabled",
            previous.library.online_metadata_enabled,
            config.library.online_metadata_enabled,
            value,
        );
        set_table_scalar_if_changed(
            library,
            "online_metadata_prompt_pending",
            previous.library.online_metadata_prompt_pending,
            config.library.online_metadata_prompt_pending,
            value,
        );
        set_table_scalar_if_changed(
            library,
            "artist_image_cache_ttl_days",
            i64::from(previous.library.artist_image_cache_ttl_days),
            i64::from(config.library.artist_image_cache_ttl_days),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "artist_image_cache_max_size_mb",
            i64::from(previous.library.artist_image_cache_max_size_mb),
            i64::from(config.library.artist_image_cache_max_size_mb),
            value,
        );
        if !library.contains_key("folders") || previous.library.folders != config.library.folders {
            let mut folders = Array::new();
            for folder in &config.library.folders {
                folders.push(folder.as_str());
            }
            set_table_value_preserving_decor(library, "folders", value(folders));
        }
    }

    {
        let buffering = document["buffering"]
            .as_table_mut()
            .expect("buffering should be a table");
        set_table_scalar_if_changed(
            buffering,
            "player_low_watermark_ms",
            i64::from(previous.buffering.player_low_watermark_ms),
            i64::from(config.buffering.player_low_watermark_ms),
            value,
        );
        set_table_scalar_if_changed(
            buffering,
            "player_target_buffer_ms",
            i64::from(previous.buffering.player_target_buffer_ms),
            i64::from(config.buffering.player_target_buffer_ms),
            value,
        );
        set_table_scalar_if_changed(
            buffering,
            "player_request_interval_ms",
            i64::from(previous.buffering.player_request_interval_ms),
            i64::from(config.buffering.player_request_interval_ms),
            value,
        );
        set_table_scalar_if_changed(
            buffering,
            "decoder_request_chunk_ms",
            i64::from(previous.buffering.decoder_request_chunk_ms),
            i64::from(config.buffering.decoder_request_chunk_ms),
            value,
        );
    }

    {
        let integrations = document["integrations"]
            .as_table_mut()
            .expect("integrations should be a table");
        if !integrations.contains_key("backends")
            || previous.integrations.backends != config.integrations.backends
        {
            let mut backends = ArrayOfTables::new();
            for backend in &config.integrations.backends {
                let mut row = Table::new();
                row.insert("profile_id", value(backend.profile_id.clone()));
                row.insert(
                    "backend_kind",
                    value(match backend.backend_kind {
                        IntegrationBackendKind::OpenSubsonic => "open_subsonic",
                    }),
                );
                row.insert("display_name", value(backend.display_name.clone()));
                row.insert("endpoint", value(backend.endpoint.clone()));
                row.insert("username", value(backend.username.clone()));
                row.insert("enabled", value(backend.enabled));
                backends.push(row);
            }
            set_table_value_preserving_decor(
                integrations,
                "backends",
                Item::ArrayOfTables(backends),
            );
        }
    }
}

fn serialize_config_with_preserved_comments(
    existing_text: &str,
    config: &Config,
) -> Result<String, String> {
    let previous = toml::from_str::<Config>(existing_text)
        .map_err(|err| format!("failed to parse existing config as Config: {}", err))?;
    let mut document = existing_text
        .parse::<DocumentMut>()
        .map_err(|err| format!("failed to parse existing config as TOML document: {}", err))?;
    write_config_to_document(&mut document, &previous, config);
    Ok(document.to_string())
}

fn merge_table_with_targeted_updates(destination: &mut Table, source: &Table) {
    for (key, source_item) in source.iter() {
        match source_item {
            Item::Table(source_table) => {
                if !destination.get(key).is_some_and(Item::is_table) {
                    destination.insert(key, Item::Table(Table::new()));
                }
                let destination_table = destination
                    .get_mut(key)
                    .and_then(Item::as_table_mut)
                    .expect("table inserted above");
                merge_table_with_targeted_updates(destination_table, source_table);
            }
            Item::ArrayOfTables(source_array) => {
                if !destination.get(key).is_some_and(Item::is_array_of_tables) {
                    set_table_value_preserving_decor(destination, key, source_item.clone());
                    continue;
                }
                let destination_array = destination
                    .get_mut(key)
                    .and_then(Item::as_array_of_tables_mut)
                    .expect("array-of-tables verified above");
                let shared_len = destination_array.len().min(source_array.len());
                for index in 0..shared_len {
                    let destination_table = destination_array
                        .get_mut(index)
                        .expect("index should exist in destination array");
                    let source_table = source_array
                        .get(index)
                        .expect("index should exist in source array");
                    merge_table_with_targeted_updates(destination_table, source_table);
                }
                if source_array.len() > destination_array.len() {
                    for index in destination_array.len()..source_array.len() {
                        let source_table = source_array
                            .get(index)
                            .expect("index should exist in source array");
                        destination_array.push(source_table.clone());
                    }
                } else if source_array.len() < destination_array.len() {
                    for _ in source_array.len()..destination_array.len() {
                        destination_array.remove(source_array.len());
                    }
                }
            }
            _ => {
                set_table_value_preserving_decor(destination, key, source_item.clone());
            }
        }
    }
}

fn serialize_layout_with_preserved_comments(
    existing_text: &str,
    layout: &LayoutConfig,
) -> Result<String, String> {
    let next_layout_text = toml::to_string(layout)
        .map_err(|err| format!("failed to serialize layout to TOML: {}", err))?;
    let next_document = next_layout_text
        .parse::<DocumentMut>()
        .map_err(|err| format!("failed to parse serialized layout TOML document: {}", err))?;
    let mut existing_document = existing_text
        .parse::<DocumentMut>()
        .map_err(|err| format!("failed to parse existing layout as TOML document: {}", err))?;

    merge_table_with_targeted_updates(existing_document.as_table_mut(), next_document.as_table());
    Ok(existing_document.to_string())
}

fn persist_config_file(config: &Config, path: &std::path::Path) {
    let existing_text = std::fs::read_to_string(path).ok();
    let config_text = if let Some(existing_text) = existing_text {
        match serialize_config_with_preserved_comments(&existing_text, config) {
            Ok(updated_text) => Some(updated_text),
            Err(err) => {
                warn!(
                    "Failed to preserve config comments for {} ({}). Falling back to plain serialization.",
                    path.display(),
                    err
                );
                toml::to_string(config).ok()
            }
        }
    } else {
        toml::to_string(config).ok()
    };

    let Some(config_text) = config_text else {
        log::error!("Failed to serialize config for {}", path.display());
        return;
    };

    if let Err(err) = std::fs::write(path, config_text) {
        log::error!("Failed to persist config to {}: {}", path.display(), err);
    }
}

fn persist_layout_file(layout: &LayoutConfig, path: &std::path::Path) {
    let existing_text = std::fs::read_to_string(path).ok();
    let layout_text = if let Some(existing_text) = existing_text {
        match serialize_layout_with_preserved_comments(&existing_text, layout) {
            Ok(updated_text) => Some(updated_text),
            Err(err) => {
                warn!(
                    "Failed to preserve layout comments for {} ({}). Falling back to plain serialization.",
                    path.display(),
                    err
                );
                toml::to_string(layout).ok()
            }
        }
    } else {
        toml::to_string(layout).ok()
    };

    let Some(layout_text) = layout_text else {
        log::error!("Failed to serialize layout for {}", path.display());
        return;
    };

    if let Err(err) = std::fs::write(path, layout_text) {
        log::error!("Failed to persist layout to {}: {}", path.display(), err);
    }
}

fn persist_state_files(config: &Config, config_path: &Path, layout_path: &Path) {
    persist_config_file(config, config_path);
    persist_layout_file(&config.ui.layout, layout_path);
}

fn persist_state_files_with_config_path(config: &Config, config_path: &Path) {
    let layout_path = config_path
        .parent()
        .map(|parent| parent.join("layout.toml"))
        .unwrap_or_else(|| PathBuf::from("layout.toml"));
    persist_state_files(config, config_path, &layout_path);
}

fn system_layout_template_text() -> &'static str {
    include_str!("../config/layout.system.toml")
}

fn load_system_layout_template() -> LayoutConfig {
    toml::from_str(system_layout_template_text())
        .expect("layout system template should parse into LayoutConfig")
}

fn load_layout_file(path: &Path) -> LayoutConfig {
    let layout_content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            warn!(
                "Failed to read layout file {}. Using layout system template. error={}",
                path.display(),
                err
            );
            return load_system_layout_template();
        }
    };

    match toml::from_str::<LayoutConfig>(&layout_content) {
        Ok(layout) => layout,
        Err(err) => {
            warn!(
                "Failed to parse layout file {}. Using layout system template. error={}",
                path.display(),
                err
            );
            load_system_layout_template()
        }
    }
}

fn hydrate_ui_columns_from_layout(config: &mut Config) {
    config.ui.playlist_album_art_column_min_width_px =
        config.ui.layout.playlist_album_art_column_min_width_px;
    config.ui.playlist_album_art_column_max_width_px =
        config.ui.layout.playlist_album_art_column_max_width_px;
    config.ui.playlist_columns = config.ui.layout.playlist_columns.clone();
}

fn with_updated_layout(previous: &Config, layout: LayoutConfig) -> Config {
    sanitize_config(Config {
        output: previous.output.clone(),
        cast: previous.cast.clone(),
        ui: UiConfig {
            show_layout_edit_intro: previous.ui.show_layout_edit_intro,
            show_tooltips: previous.ui.show_tooltips,
            auto_scroll_to_playing_track: previous.ui.auto_scroll_to_playing_track,
            dark_mode: previous.ui.dark_mode,
            playlist_album_art_column_min_width_px: previous
                .ui
                .playlist_album_art_column_min_width_px,
            playlist_album_art_column_max_width_px: previous
                .ui
                .playlist_album_art_column_max_width_px,
            layout,
            playlist_columns: previous.ui.playlist_columns.clone(),
            window_width: previous.ui.window_width,
            window_height: previous.ui.window_height,
            volume: previous.ui.volume,
            playback_order: previous.ui.playback_order,
            repeat_mode: previous.ui.repeat_mode,
        },
        library: previous.library.clone(),
        buffering: previous.buffering.clone(),
        integrations: previous.integrations.clone(),
    })
}

fn push_layout_snapshot(stack: &Arc<Mutex<Vec<LayoutConfig>>>, layout: &LayoutConfig) {
    let mut stack = stack.lock().expect("layout history stack lock poisoned");
    stack.push(layout.clone());
    if stack.len() > 128 {
        let overflow = stack.len() - 128;
        stack.drain(0..overflow);
    }
}

fn clear_layout_snapshot_stack(stack: &Arc<Mutex<Vec<LayoutConfig>>>) {
    stack
        .lock()
        .expect("layout history stack lock poisoned")
        .clear();
}

fn push_layout_undo_snapshot(
    undo_stack: &Arc<Mutex<Vec<LayoutConfig>>>,
    redo_stack: &Arc<Mutex<Vec<LayoutConfig>>>,
    layout: &LayoutConfig,
) {
    push_layout_snapshot(undo_stack, layout);
    clear_layout_snapshot_stack(redo_stack);
}

fn pop_layout_snapshot(stack: &Arc<Mutex<Vec<LayoutConfig>>>) -> Option<LayoutConfig> {
    stack
        .lock()
        .expect("layout history stack lock poisoned")
        .pop()
}

fn update_or_replace_vec_model<T: Clone + 'static>(
    current_model: ModelRc<T>,
    next_values: Vec<T>,
) -> ModelRc<T> {
    if let Some(vec_model) = current_model.as_any().downcast_ref::<VecModel<T>>() {
        if vec_model.row_count() == next_values.len() {
            for (index, value) in next_values.into_iter().enumerate() {
                vec_model.set_row_data(index, value);
            }
        } else {
            vec_model.set_vec(next_values);
        }
        current_model
    } else {
        ModelRc::from(Rc::new(VecModel::from(next_values)))
    }
}

fn apply_layout_to_ui(
    ui: &AppWindow,
    config: &Config,
    workspace_width_px: u32,
    workspace_height_px: u32,
) {
    let sanitized_layout =
        sanitize_layout_config(&config.ui.layout, workspace_width_px, workspace_height_px);
    let splitter_thickness_px = if ui.get_layout_edit_mode() {
        SPLITTER_THICKNESS_PX
    } else {
        0
    };
    let metrics = compute_tree_layout_metrics(
        &sanitized_layout,
        workspace_width_px,
        workspace_height_px,
        splitter_thickness_px,
    );

    let leaf_ids: Vec<slint::SharedString> = metrics
        .leaves
        .iter()
        .map(|leaf| leaf.id.clone().into())
        .collect();
    let region_panel_codes: Vec<i32> = metrics
        .leaves
        .iter()
        .map(|leaf| leaf.panel.to_code())
        .collect();
    let region_x: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.x).collect();
    let region_y: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.y).collect();
    let region_widths: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.width).collect();
    let region_heights: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.height).collect();
    let region_visible: Vec<bool> = vec![true; metrics.leaves.len()];

    let selected_leaf_index = sanitized_layout
        .selected_leaf_id
        .as_ref()
        .and_then(|selected| metrics.leaves.iter().position(|leaf| &leaf.id == selected))
        .map(|index| index as i32)
        .unwrap_or(-1);

    let splitter_model: Vec<LayoutSplitterModel> = metrics
        .splitters
        .iter()
        .map(|splitter| LayoutSplitterModel {
            id: splitter.id.clone().into(),
            axis: splitter.axis.to_code(),
            x_px: splitter.x,
            y_px: splitter.y,
            width_px: splitter.width,
            height_px: splitter.height,
            ratio: splitter.ratio,
            min_ratio: splitter.min_ratio,
            max_ratio: splitter.max_ratio,
            track_start_px: splitter.track_start_px,
            track_length_px: splitter.track_length_px,
        })
        .collect();

    ui.set_layout_leaf_ids(update_or_replace_vec_model(
        ui.get_layout_leaf_ids(),
        leaf_ids,
    ));
    ui.set_layout_region_panel_kinds(update_or_replace_vec_model(
        ui.get_layout_region_panel_kinds(),
        region_panel_codes,
    ));
    ui.set_layout_region_x_px(update_or_replace_vec_model(
        ui.get_layout_region_x_px(),
        region_x,
    ));
    ui.set_layout_region_y_px(update_or_replace_vec_model(
        ui.get_layout_region_y_px(),
        region_y,
    ));
    ui.set_layout_region_width_px(update_or_replace_vec_model(
        ui.get_layout_region_width_px(),
        region_widths,
    ));
    ui.set_layout_region_height_px(update_or_replace_vec_model(
        ui.get_layout_region_height_px(),
        region_heights,
    ));
    ui.set_layout_region_visible(update_or_replace_vec_model(
        ui.get_layout_region_visible(),
        region_visible,
    ));
    ui.set_layout_splitters(update_or_replace_vec_model(
        ui.get_layout_splitters(),
        splitter_model,
    ));
    ui.set_layout_selected_leaf_index(selected_leaf_index);
    apply_button_cluster_views_to_ui(ui, &metrics, config);
    apply_viewer_panel_views_to_ui(ui, &metrics, config);
}

fn apply_playlist_columns_to_ui(ui: &AppWindow, config: &Config) {
    let visible_headers: Vec<slint::SharedString> = config
        .ui
        .playlist_columns
        .iter()
        .filter(|column| column.enabled)
        .map(|column| column.name.as_str().into())
        .collect();
    let menu_labels: Vec<slint::SharedString> = config
        .ui
        .playlist_columns
        .iter()
        .map(|column| column.name.as_str().into())
        .collect();
    let menu_checked: Vec<bool> = config
        .ui
        .playlist_columns
        .iter()
        .map(|column| column.enabled)
        .collect();
    let menu_is_custom: Vec<bool> = config
        .ui
        .playlist_columns
        .iter()
        .map(|column| column.custom)
        .collect();
    let visible_kinds = visible_playlist_column_kinds(&config.ui.playlist_columns);

    ui.set_playlist_visible_column_headers(ModelRc::from(Rc::new(VecModel::from(visible_headers))));
    ui.set_playlist_visible_column_kinds(ModelRc::from(Rc::new(VecModel::from(visible_kinds))));
    ui.set_playlist_column_menu_labels(ModelRc::from(Rc::new(VecModel::from(menu_labels))));
    ui.set_playlist_column_menu_checked(ModelRc::from(Rc::new(VecModel::from(menu_checked))));
    ui.set_playlist_column_menu_is_custom(ModelRc::from(Rc::new(VecModel::from(menu_is_custom))));
}

fn should_apply_custom_column_delete(
    confirm_dialog_visible: bool,
    pending_index: i32,
    requested_index: usize,
) -> bool {
    if !confirm_dialog_visible {
        return false;
    }
    pending_index >= 0 && pending_index as usize == requested_index
}

fn apply_config_to_ui(
    ui: &AppWindow,
    config: &Config,
    output_options: &OutputSettingsOptions,
    workspace_width_px: u32,
    workspace_height_px: u32,
) {
    const SAMPLE_RATE_MODE_OPTIONS: [&str; 2] = ["Match Content (Recommended)", "Manual"];
    const RESAMPLER_QUALITY_OPTIONS: [&str; 2] = ["High", "Highest"];

    ui.set_volume_level(config.ui.volume);
    let playback_order_index = match config.ui.playback_order {
        UiPlaybackOrder::Default => 0,
        UiPlaybackOrder::Shuffle => 1,
        UiPlaybackOrder::Random => 2,
    };
    let repeat_mode_index = match config.ui.repeat_mode {
        UiRepeatMode::Off => 0,
        UiRepeatMode::Playlist => 1,
        UiRepeatMode::Track => 2,
    };
    ui.set_playback_order_index(playback_order_index);
    ui.set_repeat_mode(repeat_mode_index);
    ui.set_sidebar_width_px(sidebar_width_from_window(config.ui.window_width));
    ui.set_layout_panel_options(ModelRc::from(Rc::new(VecModel::from(
        layout_panel_options(),
    ))));

    let auto_device_label = if output_options.auto_device_name.is_empty() {
        "Unavailable".to_string()
    } else {
        output_options.auto_device_name.clone()
    };
    let mut device_options_with_other: Vec<slint::SharedString> =
        vec![format!("Auto (System Default: {})", auto_device_label).into()];
    device_options_with_other.extend(
        output_options
            .device_names
            .iter()
            .map(|value| value.as_str().into()),
    );
    device_options_with_other.push("Other...".into());

    let mut channel_options_with_other: Vec<slint::SharedString> =
        vec![format!("Auto ({})", output_options.auto_channel_value).into()];
    channel_options_with_other.extend(
        output_options
            .channel_values
            .iter()
            .map(|value| value.to_string().into()),
    );
    channel_options_with_other.push("Other...".into());

    let mut sample_rate_options_with_other: Vec<slint::SharedString> = output_options
        .sample_rate_values
        .iter()
        .map(|value| format!("{} Hz", value).into())
        .collect();
    sample_rate_options_with_other.push("Other...".into());

    let mut bit_depth_options_with_other: Vec<slint::SharedString> =
        vec![format!("Auto ({} bit)", output_options.auto_bits_per_sample_value).into()];
    bit_depth_options_with_other.extend(
        output_options
            .bits_per_sample_values
            .iter()
            .map(|value| format!("{} bit", value).into()),
    );
    bit_depth_options_with_other.push("Other...".into());

    ui.set_settings_output_device_options(ModelRc::from(Rc::new(VecModel::from(
        device_options_with_other,
    ))));
    ui.set_settings_channel_options(ModelRc::from(Rc::new(VecModel::from(
        channel_options_with_other,
    ))));
    ui.set_settings_sample_rate_options(ModelRc::from(Rc::new(VecModel::from(
        sample_rate_options_with_other,
    ))));
    ui.set_settings_bits_per_sample_options(ModelRc::from(Rc::new(VecModel::from(
        bit_depth_options_with_other,
    ))));
    ui.set_settings_sample_rate_mode_options(ModelRc::from(Rc::new(VecModel::from(
        SAMPLE_RATE_MODE_OPTIONS
            .iter()
            .map(|value| (*value).into())
            .collect::<Vec<slint::SharedString>>(),
    ))));
    ui.set_settings_resampler_quality_options(ModelRc::from(Rc::new(VecModel::from(
        RESAMPLER_QUALITY_OPTIONS
            .iter()
            .map(|value| (*value).into())
            .collect::<Vec<slint::SharedString>>(),
    ))));

    let device_custom_index = output_options.device_names.len() + 1;
    let channel_custom_index = output_options.channel_values.len() + 1;
    let sample_rate_custom_index = output_options.sample_rate_values.len();
    let bits_custom_index = output_options.bits_per_sample_values.len() + 1;

    let device_index = select_output_option_index_string(
        config.output.output_device_auto,
        &config.output.output_device_name,
        &output_options.device_names,
        device_custom_index,
    );
    let channel_index = select_output_option_index_u16(
        config.output.channel_count_auto,
        config.output.channel_count,
        &output_options.channel_values,
        channel_custom_index,
    );
    let sample_rate_index = select_manual_output_option_index_u32(
        config.output.sample_rate_khz,
        &output_options.sample_rate_values,
        sample_rate_custom_index,
    );
    let bits_index = select_output_option_index_u16(
        config.output.bits_per_sample_auto,
        config.output.bits_per_sample,
        &output_options.bits_per_sample_values,
        bits_custom_index,
    );
    let sample_rate_mode_index = if config.output.sample_rate_auto { 0 } else { 1 };
    let resampler_quality_index = match config.output.resampler_quality {
        ResamplerQuality::High => 0,
        ResamplerQuality::Highest => 1,
    };

    ui.set_settings_output_device_index(device_index as i32);
    ui.set_settings_channel_index(channel_index as i32);
    ui.set_settings_sample_rate_index(sample_rate_index as i32);
    ui.set_settings_bits_per_sample_index(bits_index as i32);
    ui.set_settings_sample_rate_mode_index(sample_rate_mode_index);
    ui.set_settings_resampler_quality_index(resampler_quality_index);
    ui.set_settings_show_layout_edit_tutorial(config.ui.show_layout_edit_intro);
    ui.set_settings_show_tooltips(config.ui.show_tooltips);
    ui.set_settings_auto_scroll_to_playing_track(config.ui.auto_scroll_to_playing_track);
    ui.set_ui_dark_mode(config.ui.dark_mode);
    ui.set_settings_dark_mode(config.ui.dark_mode);
    let color_scheme = if config.ui.dark_mode {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    ui.global::<AppPalette>().set_color_scheme(color_scheme);
    ui.set_settings_dither_on_bitdepth_reduce(config.output.dither_on_bitdepth_reduce);
    ui.set_settings_downmix_higher_channel_tracks(config.output.downmix_higher_channel_tracks);
    ui.set_settings_cast_allow_transcode_fallback(config.cast.allow_transcode_fallback);
    ui.set_settings_verified_sample_rates_summary(
        output_options.verified_sample_rates_summary.clone().into(),
    );
    ui.set_show_tooltips_enabled(config.ui.show_tooltips);
    ui.set_settings_output_device_custom_value(config.output.output_device_name.to_string().into());
    ui.set_settings_channel_custom_value(config.output.channel_count.to_string().into());
    ui.set_settings_sample_rate_custom_value(config.output.sample_rate_khz.to_string().into());
    ui.set_settings_bits_per_sample_custom_value(config.output.bits_per_sample.to_string().into());
    if !config.ui.show_tooltips {
        ui.set_show_tooltip(false);
        ui.set_tooltip_text("".into());
    }
    let library_folders: Vec<slint::SharedString> = config
        .library
        .folders
        .iter()
        .map(|folder| folder.as_str().into())
        .collect();
    ui.set_settings_library_folders(ModelRc::from(Rc::new(VecModel::from(library_folders))));
    if config.library.folders.is_empty() {
        ui.set_settings_library_selected_folder_index(-1);
    } else {
        let current_selection = ui.get_settings_library_selected_folder_index();
        if current_selection < 0 || current_selection as usize >= config.library.folders.len() {
            ui.set_settings_library_selected_folder_index(0);
        }
    }
    ui.set_settings_library_online_metadata_enabled(config.library.online_metadata_enabled);
    if let Some(backend) = find_opensubsonic_backend(config) {
        ui.set_settings_subsonic_enabled(backend.enabled);
        ui.set_settings_subsonic_endpoint(backend.endpoint.clone().into());
        ui.set_settings_subsonic_username(backend.username.clone().into());
        ui.set_settings_subsonic_password("".into());
        let status = if backend.endpoint.trim().is_empty() || backend.username.trim().is_empty() {
            "Not configured".to_string()
        } else if backend.enabled {
            "Configured (ready to connect)".to_string()
        } else {
            "Configured (disabled)".to_string()
        };
        ui.set_settings_subsonic_status(status.into());
    } else {
        ui.set_settings_subsonic_enabled(false);
        ui.set_settings_subsonic_endpoint("".into());
        ui.set_settings_subsonic_username("".into());
        ui.set_settings_subsonic_password("".into());
        ui.set_settings_subsonic_status("Not configured".into());
    }
    apply_playlist_columns_to_ui(ui, config);
    apply_layout_to_ui(ui, config, workspace_width_px, workspace_height_px);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut clog = colog::default_builder();
    clog.filter(Some("roqtune"), log::LevelFilter::Debug);
    clog.init();

    std::panic::set_hook(Box::new(|panic_info| {
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("unnamed");
        log::error!("panic in thread '{}': {}", thread_name, panic_info);
    }));

    let configured_backend = std::env::var("SLINT_BACKEND").unwrap_or_else(|_| {
        info!("SLINT_BACKEND not set. Defaulting to winit-software");
        "winit-software".to_string()
    });
    let backend_selector = slint::BackendSelector::new().backend_name(configured_backend.clone());
    backend_selector
        .select()
        .map_err(|err| format!("Failed to initialize Slint backend: {}", err))?;

    // Setup ui state
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
        let default_config = sanitize_config(Config::default());

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
    let mut config = sanitize_config(toml::from_str::<Config>(&config_content).unwrap_or_default());
    config.ui.layout = load_layout_file(&layout_file);
    hydrate_ui_columns_from_layout(&mut config);
    let config = sanitize_config(config);
    let initial_output_options = detect_output_settings_options(&config);
    let runtime_output_override = Arc::new(Mutex::new(RuntimeOutputOverride::default()));
    let runtime_override_snapshot = {
        let state = runtime_output_override
            .lock()
            .expect("runtime output override lock poisoned");
        state.clone()
    };
    let runtime_config = resolve_runtime_config(
        &config,
        &initial_output_options,
        Some(&runtime_override_snapshot),
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
    apply_config_to_ui(
        &ui,
        &config,
        &initial_output_options,
        initial_workspace_width_px,
        initial_workspace_height_px,
    );
    let config_state = Arc::new(Mutex::new(config.clone()));
    let output_options = Arc::new(Mutex::new(initial_output_options.clone()));
    let output_device_inventory = Arc::new(Mutex::new(snapshot_output_device_names(
        &cpal::default_host(),
    )));
    let layout_workspace_size = Arc::new(Mutex::new((
        initial_workspace_width_px,
        initial_workspace_height_px,
    )));
    let layout_undo_stack: Arc<Mutex<Vec<LayoutConfig>>> = Arc::new(Mutex::new(Vec::new()));
    let layout_redo_stack: Arc<Mutex<Vec<LayoutConfig>>> = Arc::new(Mutex::new(Vec::new()));
    let last_runtime_signature = Arc::new(Mutex::new(OutputRuntimeSignature::from_output(
        &runtime_config.output,
    )));

    // Bus for communication between components
    let (bus_sender, _) = broadcast::channel(8192);
    let playback_session_active = Arc::new(AtomicBool::new(false));
    let staged_audio_settings: Arc<Mutex<Option<StagedAudioSettings>>> = Arc::new(Mutex::new(None));
    let (playlist_bulk_import_tx, playlist_bulk_import_rx) =
        mpsc::sync_channel::<protocol::PlaylistBulkImportRequest>(64);
    let (library_scan_progress_tx, library_scan_progress_rx) =
        mpsc::sync_channel::<protocol::LibraryMessage>(512);
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
    let library_folder_import_context = LibraryFolderImportContext {
        config_state: Arc::clone(&config_state),
        output_options: Arc::clone(&output_options),
        runtime_output_override: Arc::clone(&runtime_output_override),
        runtime_audio_state: Arc::clone(&runtime_audio_state),
        last_runtime_signature: Arc::clone(&last_runtime_signature),
        config_file: config_file.clone(),
        layout_workspace_size: Arc::clone(&layout_workspace_size),
        ui_handle: ui.as_weak().clone(),
        bus_sender: bus_sender.clone(),
    };

    let ui_handle_for_drop = ui.as_weak().clone();
    let pending_drop_paths: Rc<RefCell<Vec<(i32, PathBuf)>>> = Rc::new(RefCell::new(Vec::new()));
    let drop_flush_scheduled = Rc::new(Cell::new(false));
    let playlist_bulk_import_tx_for_drop = playlist_bulk_import_tx.clone();
    let bus_sender_for_drop = bus_sender.clone();
    let library_context_for_drop = library_folder_import_context.clone();
    ui.window().on_winit_window_event(move |_, event| {
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            debug!("Dropped file: {}", path.display());
            let collection_mode = ui_handle_for_drop
                .upgrade()
                .map(|ui| ui.get_collection_mode())
                .unwrap_or(COLLECTION_MODE_PLAYLIST);
            pending_drop_paths
                .borrow_mut()
                .push((collection_mode, path.clone()));
            if !drop_flush_scheduled.get() {
                drop_flush_scheduled.set(true);
                let pending_drop_paths = pending_drop_paths.clone();
                let drop_flush_scheduled = drop_flush_scheduled.clone();
                let playlist_bulk_import_tx = playlist_bulk_import_tx_for_drop.clone();
                let bus_sender = bus_sender_for_drop.clone();
                let library_context = library_context_for_drop.clone();
                slint::Timer::single_shot(
                    Duration::from_millis(DROP_IMPORT_BATCH_DELAY_MS),
                    move || {
                        drop_flush_scheduled.set(false);
                        let pending = std::mem::take(&mut *pending_drop_paths.borrow_mut());
                        if pending.is_empty() {
                            return;
                        }
                        let mut playlist_paths = Vec::new();
                        let mut library_paths = Vec::new();
                        for (mode, path) in pending {
                            if mode == COLLECTION_MODE_LIBRARY {
                                library_paths.push(path);
                            } else {
                                playlist_paths.push(path);
                            }
                        }
                        if !playlist_paths.is_empty() {
                            let playlist_bulk_import_tx = playlist_bulk_import_tx.clone();
                            let bus_sender = bus_sender.clone();
                            thread::spawn(move || {
                                let tracks =
                                    collect_audio_files_from_dropped_paths(&playlist_paths);
                                if tracks.is_empty() {
                                    debug!("Ignored dropped playlist item(s): no supported tracks");
                                    return;
                                }
                                let source = if playlist_paths.iter().any(|path| path.is_dir()) {
                                    protocol::ImportSource::AddFolderDialog
                                } else {
                                    protocol::ImportSource::AddFilesDialog
                                };
                                let queued = enqueue_playlist_bulk_import(
                                    &playlist_bulk_import_tx,
                                    &bus_sender,
                                    &tracks,
                                    source,
                                );
                                debug!(
                                    "Queued {} track(s) from drag-and-drop into playlist",
                                    queued
                                );
                            });
                        }
                        if !library_paths.is_empty() {
                            let folders =
                                collect_library_folders_from_dropped_paths(&library_paths);
                            if folders.is_empty() {
                                debug!("Ignored dropped library item(s): no usable folders found");
                                return;
                            }
                            let added =
                                add_library_folders_to_config_and_scan(&library_context, folders);
                            debug!("Added {} folder(s) from drag-and-drop into library", added);
                        }
                    },
                );
            }
        }
        WinitEventResult::Propagate
    });

    let playlist_bulk_import_tx_clone = playlist_bulk_import_tx.clone();
    let bus_sender_clone = bus_sender.clone();

    // Setup import dialogs
    ui.on_open_file(move || {
        debug!("Opening file dialog");
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Audio Files", &SUPPORTED_AUDIO_EXTENSIONS)
            .pick_files()
        {
            let filtered_paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|path| {
                    if is_supported_audio_file(path) {
                        true
                    } else {
                        debug!(
                            "Skipping unsupported file from import dialog: {}",
                            path.display()
                        );
                        false
                    }
                })
                .collect();
            if filtered_paths.is_empty() {
                return;
            }
            let queued = enqueue_playlist_bulk_import(
                &playlist_bulk_import_tx_clone,
                &bus_sender_clone,
                &filtered_paths,
                protocol::ImportSource::AddFilesDialog,
            );
            debug!("Queued {} track(s) from Add files", queued);
        }
    });

    let playlist_bulk_import_tx_clone = playlist_bulk_import_tx.clone();
    let bus_sender_clone = bus_sender.clone();
    ui.on_open_folder(move || {
        debug!("Opening folder dialog");
        if let Some(folder_path) = rfd::FileDialog::new().pick_folder() {
            let bus_sender_for_scan = bus_sender_clone.clone();
            let bulk_import_tx = playlist_bulk_import_tx_clone.clone();
            thread::spawn(move || {
                let tracks = collect_audio_files_from_folder(&folder_path);
                debug!(
                    "Found {} track(s) in folder import {}",
                    tracks.len(),
                    folder_path.display()
                );
                let queued = enqueue_playlist_bulk_import(
                    &bulk_import_tx,
                    &bus_sender_for_scan,
                    &tracks,
                    protocol::ImportSource::AddFolderDialog,
                );
                debug!(
                    "Queued {} track(s) from Add folder {}",
                    queued,
                    folder_path.display()
                );
            });
        }
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_switch_collection_mode(move |mode| {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::SetCollectionMode(mode),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_select_root(move |section| {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::SelectRootSection(section),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_global_library_search(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::OpenGlobalSearch));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_back(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::NavigateBack));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_select_list_item(move |index, ctrl, shift, context_click| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::SelectListItem {
            index: index as usize,
            ctrl,
            shift,
            context_click,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_item_activated(move |index| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ActivateListItem(index as usize),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_prepare_add_to_playlists(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::PrepareAddToPlaylists,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_toggle_add_to_playlist(move |index| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ToggleAddToPlaylist(index as usize),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_confirm_add_to_playlists(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ConfirmAddToPlaylists,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_cancel_add_to_playlists(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::CancelAddToPlaylists,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_confirm_remove_selection(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ConfirmRemoveSelection,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_cancel_remove_selection(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::CancelRemoveSelection,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_properties_for_current_selection(move || {
        let _ = bus_sender_clone.send(Message::Metadata(
            MetadataMessage::OpenPropertiesForCurrentSelection,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_properties_field_edited(move |index, value| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Metadata(MetadataMessage::EditPropertiesField {
            index: index as usize,
            value: value.to_string(),
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_properties_save(move || {
        let _ = bus_sender_clone.send(Message::Metadata(MetadataMessage::SaveProperties));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_properties_cancel(move || {
        let _ = bus_sender_clone.send(Message::Metadata(MetadataMessage::CancelProperties));
    });

    let ui_handle_clone = ui.as_weak().clone();
    ui.on_settings_select_library_folder(move |index| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_settings_library_selected_folder_index(index);
        }
    });

    let library_folder_import_context_clone = library_folder_import_context.clone();
    ui.on_library_add_folder(move || {
        let Some(folder_path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let folder = folder_path.to_string_lossy().to_string();
        let added = add_library_folders_to_config_and_scan(
            &library_folder_import_context_clone,
            vec![folder],
        );
        debug!("Added {} folder(s) from library folder picker", added);
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_library_remove_folder(move |index| {
        if index < 0 {
            return;
        }
        let next_config = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if index as usize >= state.library.folders.len() {
                return;
            }
            let mut next = state.clone();
            next.library.folders.remove(index as usize);
            next = sanitize_config(next);
            *state = next.clone();
            next
        };

        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(
                &ui,
                &next_config,
                &options_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }

        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::RequestScan));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_rescan(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::RequestScan));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_clear_library_enrichment_cache(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ClearEnrichmentCache,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_library_online_metadata_prompt_accept(move || {
        let next_config = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            next.library.online_metadata_enabled = true;
            next.library.online_metadata_prompt_pending = false;
            next = sanitize_config(next);
            *state = next.clone();
            next
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(
                &ui,
                &next_config,
                &options_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_library_online_metadata_prompt_deny(move || {
        let next_config = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            next.library.online_metadata_enabled = false;
            next.library.online_metadata_prompt_pending = false;
            next = sanitize_config(next);
            *state = next.clone();
            next
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(
                &ui,
                &next_config,
                &options_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_settings_set_library_online_metadata_enabled(move |enabled| {
        let next_config = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            // Any explicit user choice here counts as handling first-use prompt.
            next.library.online_metadata_prompt_pending = false;
            next.library.online_metadata_enabled = enabled;
            next = sanitize_config(next);
            *state = next.clone();
            next
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(
                &ui,
                &next_config,
                &options_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_settings_save_subsonic_profile(move |enabled, endpoint, username, password| {
        let endpoint_trimmed = endpoint.trim().trim_end_matches('/').to_string();
        let username_trimmed = username.trim().to_string();
        let password_trimmed = password.trim().to_string();

        let mut status_message = "OpenSubsonic profile saved".to_string();
        if !password_trimmed.is_empty() {
            if let Err(error) =
                set_opensubsonic_password(OPENSUBSONIC_PROFILE_ID, password_trimmed.as_str())
            {
                warn!(
                    "Failed to save OpenSubsonic credential for profile '{}': {}",
                    OPENSUBSONIC_PROFILE_ID, error
                );
                status_message = format!("Failed to save password to credential store: {error}");
            }
        }

        let next_config = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            upsert_opensubsonic_backend_config(
                &mut next,
                endpoint_trimmed.as_str(),
                username_trimmed.as_str(),
                enabled,
            );
            next = sanitize_config(next);
            *state = next.clone();
            next
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);

        let password_for_upsert = if !password_trimmed.is_empty() {
            Some(password_trimmed)
        } else {
            match get_opensubsonic_password(OPENSUBSONIC_PROFILE_ID) {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        "Failed to load OpenSubsonic credential for profile '{}': {}",
                        OPENSUBSONIC_PROFILE_ID, error
                    );
                    status_message = format!("Failed to access credential store password: {error}");
                    None
                }
            }
        };

        if let Some(backend) = find_opensubsonic_backend(&next_config) {
            let snapshot = opensubsonic_profile_snapshot(backend, Some(status_message.clone()));
            let connect_now = enabled && password_for_upsert.is_some();
            let _ = bus_sender_clone.send(Message::Integration(
                IntegrationMessage::UpsertBackendProfile {
                    profile: snapshot,
                    password: password_for_upsert,
                    connect_now,
                },
            ));
            if !enabled {
                let _ = bus_sender_clone.send(Message::Integration(
                    IntegrationMessage::DisconnectBackendProfile {
                        profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
                    },
                ));
            }
        }

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(
                &ui,
                &next_config,
                &options_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
            ui.set_settings_subsonic_status(status_message.into());
            ui.set_settings_subsonic_password("".into());
        }
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_settings_test_subsonic_connection(move || {
        let Some(ui) = ui_handle_clone.upgrade() else {
            return;
        };

        let endpoint_trimmed = ui
            .get_settings_subsonic_endpoint()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let username_trimmed = ui.get_settings_subsonic_username().trim().to_string();
        let password_trimmed = ui.get_settings_subsonic_password().trim().to_string();
        if endpoint_trimmed.is_empty() || username_trimmed.is_empty() {
            ui.set_settings_subsonic_status("Enter server URL and username first".into());
            return;
        }

        let password_for_upsert = if password_trimmed.is_empty() {
            match get_opensubsonic_password(OPENSUBSONIC_PROFILE_ID) {
                Ok(value) => value,
                Err(error) => {
                    warn!(
                        "Failed to load OpenSubsonic credential for profile '{}': {}",
                        OPENSUBSONIC_PROFILE_ID, error
                    );
                    ui.set_settings_subsonic_status(
                        format!("Failed to access credential store password: {error}").into(),
                    );
                    return;
                }
            }
        } else {
            Some(password_trimmed)
        };
        let profile = protocol::BackendProfileSnapshot {
            profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            backend_kind: protocol::BackendKind::OpenSubsonic,
            display_name: "OpenSubsonic".to_string(),
            endpoint: endpoint_trimmed,
            username: username_trimmed,
            configured: true,
            connection_state: protocol::BackendConnectionState::Disconnected,
            status_text: Some("Testing connection...".to_string()),
        };
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::UpsertBackendProfile {
                profile,
                password: password_for_upsert,
                connect_now: false,
            },
        ));
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::TestBackendConnection {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
        ui.set_settings_subsonic_status("Testing connection...".into());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_settings_sync_subsonic_now(move || {
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::SyncBackendProfile {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_settings_disconnect_subsonic(move || {
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::DisconnectBackendProfile {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_remote_detach_confirm(move |playlist_id| {
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::ConfirmDetachRemotePlaylist {
                playlist_id: playlist_id.to_string(),
            },
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_remote_detach_cancel(move |playlist_id| {
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::CancelDetachRemotePlaylist {
                playlist_id: playlist_id.to_string(),
            },
        ));
    });

    // Wire up play button
    let bus_sender_clone = bus_sender.clone();
    ui.on_play(move || {
        debug!("Play button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::PlayActiveCollection));
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
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnPointerDown {
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
    ui.on_playlist_item_double_click(move |index| {
        debug!("Playlist item double-clicked: {}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::PlayTrackByViewIndex(
            index as usize,
        )));
    });

    // Wire up delete track handler
    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_delete_selected_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                let _ = bus_sender_clone
                    .send(Message::Library(protocol::LibraryMessage::DeleteSelected));
                return;
            }
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        debug!("Delete selected tracks requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeleteSelected));
    });

    // Wire up reorder track handler
    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_reorder_tracks(move |indices, to| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let indices_vec: Vec<usize> = indices.iter().map(|i| i as usize).collect();
        debug!("Reorder tracks requested: from {:?} to {}", indices_vec, to);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ReorderTracks {
            indices: indices_vec,
            to: to as usize,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_copy_selected_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                let _ =
                    bus_sender_clone.send(Message::Library(protocol::LibraryMessage::CopySelected));
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CopySelectedTracks));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_cut_selected_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                let _ =
                    bus_sender_clone.send(Message::Library(protocol::LibraryMessage::CutSelected));
                return;
            }
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CutSelectedTracks));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_paste_copied_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                return;
            }
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::PasteCopiedTracks));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_undo_last_action(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::UndoTrackListEdit));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_redo_last_action(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::RedoTrackListEdit));
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
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_on_drag_start(move |pressed_index| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        debug!("Drag start at index {:?}", pressed_index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragStart {
            pressed_index: pressed_index as usize,
        }));
    });

    // Wire up drag move handler
    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_on_drag_move(move |drop_gap| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                return;
            }
        }
        debug!("Drag move to gap {:?}", drop_gap);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragMove {
            drop_gap: drop_gap as usize,
        }));
    });

    // Wire up drag end handler
    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_on_drag_end(move |drop_gap| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                return;
            }
        }
        debug!("Drag end at gap {:?}", drop_gap);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragEnd {
            drop_gap: drop_gap as usize,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_playlist_search(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OpenPlaylistSearch));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_close_playlist_search(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ClosePlaylistSearch));
    });

    let playlist_search_bus_sender = bus_sender.clone();
    let playlist_search_query_tx =
        spawn_debounced_query_dispatcher(Duration::from_millis(120), move |query| {
            let _ = playlist_search_bus_sender.send(Message::Playlist(
                PlaylistMessage::SetPlaylistSearchQuery(query),
            ));
        });
    ui.on_set_playlist_search_query(move |query| {
        let _ = playlist_search_query_tx.send(query.to_string());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_library_search(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::OpenSearch));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_close_library_search(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::CloseSearch));
    });

    let library_search_bus_sender = bus_sender.clone();
    let library_search_query_tx =
        spawn_debounced_query_dispatcher(Duration::from_millis(120), move |query| {
            let _ = library_search_bus_sender.send(Message::Library(
                protocol::LibraryMessage::SetSearchQuery(query),
            ));
        });
    ui.on_library_search_query_edited(move |query| {
        let _ = library_search_query_tx.send(query.to_string());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_viewport_changed(move |first_row, row_count| {
        if first_row < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::LibraryViewportChanged {
                first_row: first_row as usize,
                row_count: row_count.max(0) as usize,
            },
        ));
    });

    ui.on_open_external_url(move |url| {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Err(err) = webbrowser::open(trimmed) {
            warn!("Failed to open external URL '{}': {}", trimmed, err);
        }
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_clear_playlist_filter_view(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ClearPlaylistFilterView));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cycle_playlist_sort_by_column(move |column_index| {
        if column_index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::CyclePlaylistSortByColumn(column_index as usize),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_apply_filter_view_to_playlist(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::RequestApplyFilterView));
    });

    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_modification_blocked(move || {
        flash_read_only_view_indicator(ui_handle_clone.clone());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_playlist_columns_viewport_resized(move |width_px| {
        let clamped_width = width_px.max(0) as u32;
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::PlaylistViewportWidthChanged(clamped_width),
        ));
    });

    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_header_column_at(move |mouse_x_px| {
        ui_handle_clone
            .upgrade()
            .map(|ui| {
                resolve_playlist_header_column_from_x(
                    mouse_x_px,
                    &playlist_column_widths_from_model(ui.get_playlist_column_widths_px()),
                )
            })
            .unwrap_or(-1)
    });

    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_header_gap_at(move |mouse_x_px| {
        ui_handle_clone
            .upgrade()
            .map(|ui| {
                resolve_playlist_header_gap_from_x(
                    mouse_x_px,
                    &playlist_column_widths_from_model(ui.get_playlist_column_widths_px()),
                )
            })
            .unwrap_or(-1)
    });

    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_header_divider_at(move |mouse_x_px| {
        ui_handle_clone
            .upgrade()
            .map(|ui| {
                resolve_playlist_header_divider_from_x(
                    mouse_x_px,
                    &playlist_column_widths_from_model(ui.get_playlist_column_widths_px()),
                    4,
                )
            })
            .unwrap_or(-1)
    });

    let config_state_clone = Arc::clone(&config_state);
    ui.on_playlist_header_column_min_width(move |visible_index| {
        let state = config_state_clone
            .lock()
            .expect("config state lock poisoned");
        let album_art_bounds = album_art_column_width_bounds(&state.ui);
        playlist_column_bounds_at_visible_index(
            &state.ui.playlist_columns,
            visible_index.max(0) as usize,
            album_art_bounds,
        )
        .map(|bounds| bounds.min_px)
        .unwrap_or(1)
    });

    let config_state_clone = Arc::clone(&config_state);
    ui.on_playlist_header_column_max_width(move |visible_index| {
        let state = config_state_clone
            .lock()
            .expect("config state lock poisoned");
        let album_art_bounds = album_art_column_width_bounds(&state.ui);
        playlist_column_bounds_at_visible_index(
            &state.ui.playlist_columns,
            visible_index.max(0) as usize,
            album_art_bounds,
        )
        .map(|bounds| bounds.max_px.max(bounds.min_px))
        .unwrap_or(1024)
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    ui.on_preview_playlist_column_width(move |visible_index, width_px| {
        let (column_key, clamped_width_px) = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let visible_index = visible_index.max(0) as usize;
            let album_art_bounds = album_art_column_width_bounds(&state.ui);
            let column_key =
                playlist_column_key_at_visible_index(&state.ui.playlist_columns, visible_index);
            let clamped = clamp_width_for_visible_column(
                &state.ui.playlist_columns,
                visible_index,
                width_px,
                album_art_bounds,
            );
            (column_key, clamped)
        };
        if let (Some(column_key), Some(clamped_width_px)) = (column_key, clamped_width_px) {
            let _ = bus_sender_clone.send(Message::Playlist(
                PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                    column_key,
                    width_px: clamped_width_px as u32,
                    persist: false,
                },
            ));
        }
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    ui.on_commit_playlist_column_width(move |visible_index, width_px| {
        let (column_key, clamped_width_px) = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let visible_index = visible_index.max(0) as usize;
            let album_art_bounds = album_art_column_width_bounds(&state.ui);
            let column_key =
                playlist_column_key_at_visible_index(&state.ui.playlist_columns, visible_index);
            let clamped = clamp_width_for_visible_column(
                &state.ui.playlist_columns,
                visible_index,
                width_px,
                album_art_bounds,
            );
            (column_key, clamped)
        };
        if let (Some(column_key), Some(clamped_width_px)) = (column_key, clamped_width_px) {
            let _ = bus_sender_clone.send(Message::Playlist(
                PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                    column_key,
                    width_px: clamped_width_px as u32,
                    persist: true,
                },
            ));
        }
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    ui.on_reset_playlist_column_width(move |visible_index| {
        let column_key = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            playlist_column_key_at_visible_index(
                &state.ui.playlist_columns,
                visible_index.max(0) as usize,
            )
        };
        if let Some(column_key) = column_key {
            let _ = bus_sender_clone.send(Message::Playlist(
                PlaylistMessage::ClearActivePlaylistColumnWidthOverride {
                    column_key,
                    persist: true,
                },
            ));
        }
    });

    // Wire up sequence selector
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let config_file_clone = config_file.clone();
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

    // Wire up repeat toggle
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let config_file_clone = config_file.clone();
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

    // Wire up seek handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_seek_to(move |percentage| {
        debug!("Seek requested to {}%", percentage * 100.0);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Seek(percentage)));
    });

    // Wire up volume handler
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
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

    let config_state_clone = Arc::clone(&config_state);
    let ui_handle_clone = ui.as_weak().clone();
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
            apply_control_cluster_menu_actions_for_leaf(&ui, &config_snapshot, &leaf_id);
            ui.set_show_viewer_panel_settings_menu(false);
            ui.set_show_control_cluster_menu(true);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_add_control_cluster_button(move |leaf_id, action_id| {
        if leaf_id.trim().is_empty() || !(1..=11).contains(&action_id) {
            return;
        }
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            let mut actions = button_cluster_actions_for_leaf(
                &next_config.ui.layout.button_cluster_instances,
                &leaf_id,
            );
            actions.push(action_id);
            upsert_button_cluster_actions_for_leaf(
                &mut next_config.ui.layout.button_cluster_instances,
                &leaf_id,
                actions,
            );
            next_config = sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) =
                workspace_size_snapshot(&layout_workspace_size_clone);
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_control_cluster_menu_target_leaf_id(leaf_id.clone());
            apply_control_cluster_menu_actions_for_leaf(&ui, &next_config, &leaf_id);
            ui.set_show_viewer_panel_settings_menu(false);
            ui.set_show_control_cluster_menu(true);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_remove_control_cluster_button(move |leaf_id, button_index| {
        if leaf_id.trim().is_empty() || button_index < 0 {
            return;
        }
        let index = button_index as usize;
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            let mut actions = button_cluster_actions_for_leaf(
                &next_config.ui.layout.button_cluster_instances,
                &leaf_id,
            );
            if index >= actions.len() {
                return;
            }
            actions.remove(index);
            upsert_button_cluster_actions_for_leaf(
                &mut next_config.ui.layout.button_cluster_instances,
                &leaf_id,
                actions,
            );
            next_config = sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) =
                workspace_size_snapshot(&layout_workspace_size_clone);
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_control_cluster_menu_target_leaf_id(leaf_id.clone());
            apply_control_cluster_menu_actions_for_leaf(&ui, &next_config, &leaf_id);
            ui.set_show_viewer_panel_settings_menu(false);
            ui.set_show_control_cluster_menu(true);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_open_viewer_panel_settings_for_leaf(move |leaf_id, panel_kind, x_px, y_px| {
        if leaf_id.trim().is_empty() {
            return;
        }
        if panel_kind != LayoutPanelKind::MetadataViewer.to_code()
            && panel_kind != LayoutPanelKind::AlbumArtViewer.to_code()
        {
            return;
        }
        let (priority_index, metadata_source_index, image_source_index) = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            (
                viewer_panel_display_priority_to_code(viewer_panel_display_priority_for_leaf(
                    &state.ui.layout.viewer_panel_instances,
                    &leaf_id,
                )),
                viewer_panel_metadata_source_to_code(viewer_panel_metadata_source_for_leaf(
                    &state.ui.layout.viewer_panel_instances,
                    &leaf_id,
                )),
                viewer_panel_image_source_to_code(viewer_panel_image_source_for_leaf(
                    &state.ui.layout.viewer_panel_instances,
                    &leaf_id,
                )),
            )
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_control_cluster_menu_target_leaf_id("".into());
            ui.set_show_control_cluster_menu(false);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id.clone());
            ui.set_viewer_panel_settings_target_panel_kind(panel_kind);
            ui.set_viewer_panel_settings_display_priority_index(priority_index);
            ui.set_viewer_panel_settings_metadata_source_index(metadata_source_index);
            ui.set_viewer_panel_settings_image_source_index(image_source_index);
            ui.set_viewer_panel_settings_x(x_px.max(8) as f32);
            ui.set_viewer_panel_settings_y(y_px.max(8) as f32);
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_set_viewer_panel_display_priority(move |leaf_id, priority_code| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let display_priority = viewer_panel_display_priority_from_code(priority_code);
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            upsert_viewer_panel_display_priority_for_leaf(
                &mut next_config.ui.layout.viewer_panel_instances,
                &leaf_id_text,
                display_priority,
            );
            next_config = sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) =
                workspace_size_snapshot(&layout_workspace_size_clone);
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let output_options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
            &next_config,
            &output_options_snapshot,
            Some(&runtime_override),
            &runtime_audio_state_clone,
        );
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                viewer_panel_display_priority_to_code(display_priority),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                viewer_panel_metadata_source_to_code(viewer_panel_metadata_source_for_leaf(
                    &next_config.ui.layout.viewer_panel_instances,
                    &leaf_id_text,
                )),
            );
            ui.set_viewer_panel_settings_image_source_index(viewer_panel_image_source_to_code(
                viewer_panel_image_source_for_leaf(
                    &next_config.ui.layout.viewer_panel_instances,
                    &leaf_id_text,
                ),
            ));
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_set_viewer_panel_metadata_source(move |leaf_id, source_code| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let metadata_source = viewer_panel_metadata_source_from_code(source_code);
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            upsert_viewer_panel_metadata_source_for_leaf(
                &mut next_config.ui.layout.viewer_panel_instances,
                &leaf_id_text,
                metadata_source,
            );
            next_config = sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) =
                workspace_size_snapshot(&layout_workspace_size_clone);
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let output_options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
            &next_config,
            &output_options_snapshot,
            Some(&runtime_override),
            &runtime_audio_state_clone,
        );
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                viewer_panel_display_priority_to_code(viewer_panel_display_priority_for_leaf(
                    &next_config.ui.layout.viewer_panel_instances,
                    &leaf_id_text,
                )),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                viewer_panel_metadata_source_to_code(metadata_source),
            );
            ui.set_viewer_panel_settings_image_source_index(viewer_panel_image_source_to_code(
                viewer_panel_image_source_for_leaf(
                    &next_config.ui.layout.viewer_panel_instances,
                    &leaf_id_text,
                ),
            ));
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_set_viewer_panel_image_source(move |leaf_id, source_code| {
        let leaf_id_text = leaf_id.to_string();
        if leaf_id_text.trim().is_empty() {
            return;
        }
        let image_source = viewer_panel_image_source_from_code(source_code);
        let (next_config, workspace_width_px, workspace_height_px) = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            let mut next_config = state.clone();
            upsert_viewer_panel_image_source_for_leaf(
                &mut next_config.ui.layout.viewer_panel_instances,
                &leaf_id_text,
                image_source,
            );
            next_config = sanitize_config(next_config);
            *state = next_config.clone();
            let (workspace_width_px, workspace_height_px) =
                workspace_size_snapshot(&layout_workspace_size_clone);
            (next_config, workspace_width_px, workspace_height_px)
        };
        persist_state_files_with_config_path(&next_config, &config_file_clone);
        let output_options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
            &next_config,
            &output_options_snapshot,
            Some(&runtime_override),
            &runtime_audio_state_clone,
        );
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next_config, workspace_width_px, workspace_height_px);
            ui.set_viewer_panel_settings_target_leaf_id(leaf_id_text.clone().into());
            ui.set_viewer_panel_settings_display_priority_index(
                viewer_panel_display_priority_to_code(viewer_panel_display_priority_for_leaf(
                    &next_config.ui.layout.viewer_panel_instances,
                    &leaf_id_text,
                )),
            );
            ui.set_viewer_panel_settings_metadata_source_index(
                viewer_panel_metadata_source_to_code(viewer_panel_metadata_source_for_leaf(
                    &next_config.ui.layout.viewer_panel_instances,
                    &leaf_id_text,
                )),
            );
            ui.set_viewer_panel_settings_image_source_index(viewer_panel_image_source_to_code(
                image_source,
            ));
            ui.set_show_viewer_panel_settings_menu(true);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
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
        let config_snapshot = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if state.ui.window_width != width || state.ui.window_height != height {
                state.ui.window_width = width;
                state.ui.window_height = height;
            }
            state.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_sidebar_width_px(sidebar_width_from_window(width));
            apply_layout_to_ui(
                &ui,
                &config_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
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
            apply_layout_to_ui(
                &ui,
                &config_snapshot,
                workspace_width_px,
                workspace_height_px,
            );
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_open_layout_editor(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_layout_edit_mode() {
                ui.set_show_layout_editor_dialog(false);
                ui.set_show_layout_leaf_context_menu(false);
                ui.set_show_layout_splitter_context_menu(false);
                ui.set_layout_edit_mode(false);
                let (workspace_width_px, workspace_height_px) =
                    workspace_size_snapshot(&layout_workspace_size_clone);
                let config_snapshot = {
                    let state = config_state_clone
                        .lock()
                        .expect("config state lock poisoned");
                    state.clone()
                };
                apply_layout_to_ui(
                    &ui,
                    &config_snapshot,
                    workspace_width_px,
                    workspace_height_px,
                );
                return;
            }
        }

        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let next = {
            let mut state = config_state_clone
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
        if let Some(ui) = ui_handle_clone.upgrade() {
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
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
    });

    let config_state_clone = Arc::clone(&config_state);
    let config_file_clone = config_file.clone();
    ui.on_layout_intro_proceed(move |dont_show_again| {
        if !dont_show_again {
            return;
        }
        let next = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if state.ui.show_layout_edit_intro {
                state.ui.show_layout_edit_intro = false;
            }
            state.clone()
        };
        persist_state_files_with_config_path(&next, &config_file_clone);
    });

    let config_state_clone = Arc::clone(&config_state);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_intro_cancel(move |dont_show_again| {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let next = {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            if dont_show_again && state.ui.show_layout_edit_intro {
                state.ui.show_layout_edit_intro = false;
            }
            state.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_layout_edit_mode(false);
            ui.set_show_layout_leaf_context_menu(false);
            ui.set_show_layout_splitter_context_menu(false);
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_select_leaf(move |leaf_id| {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let next = {
            let mut state = config_state_clone
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
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_replace_selected_leaf(move |panel_code| {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let previous = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let selected_leaf_id = previous.ui.layout.selected_leaf_id.clone();
        let Some(selected_leaf_id) = selected_leaf_id else {
            return;
        };
        let (panel, preset_actions) = resolve_layout_panel_selection(panel_code);
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
        let mut next = with_updated_layout(&previous, updated_layout);
        if panel == LayoutPanelKind::ButtonCluster {
            if let Some(actions) = preset_actions {
                upsert_button_cluster_actions_for_leaf(
                    &mut next.ui.layout.button_cluster_instances,
                    &selected_leaf_id,
                    actions,
                );
            }
        } else {
            remove_button_cluster_instance_for_leaf(
                &mut next.ui.layout.button_cluster_instances,
                &selected_leaf_id,
            );
        }
        next = sanitize_config(next);
        push_layout_undo_snapshot(
            &layout_undo_stack_clone,
            &layout_redo_stack_clone,
            &previous.ui.layout,
        );
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_split_selected_leaf(move |axis_code, panel_code| {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let previous = {
            let state = config_state_clone
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
        let (panel, preset_actions) = resolve_layout_panel_selection(panel_code);
        let Some(mut updated_layout) =
            split_leaf(&previous.ui.layout, &selected_leaf_id, axis, panel)
        else {
            return;
        };
        let added_leaf_ids = newly_added_leaf_ids(&previous.ui.layout, &updated_layout);
        let added_leaf_id = added_leaf_ids.first().cloned();
        updated_layout.selected_leaf_id = if panel == LayoutPanelKind::None {
            Some(selected_leaf_id.clone())
        } else {
            added_leaf_id
                .clone()
                .or_else(|| first_leaf_id(&updated_layout.root))
        };
        let mut next = with_updated_layout(&previous, updated_layout);
        if panel == LayoutPanelKind::ButtonCluster {
            if let (Some(new_leaf_id), Some(actions)) = (added_leaf_id, preset_actions) {
                upsert_button_cluster_actions_for_leaf(
                    &mut next.ui.layout.button_cluster_instances,
                    &new_leaf_id,
                    actions,
                );
            }
        }
        next = sanitize_config(next);
        push_layout_undo_snapshot(
            &layout_undo_stack_clone,
            &layout_redo_stack_clone,
            &previous.ui.layout,
        );
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_delete_selected_leaf(move || {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let previous = {
            let state = config_state_clone
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
        let next = with_updated_layout(&previous, updated_layout);
        push_layout_undo_snapshot(
            &layout_undo_stack_clone,
            &layout_redo_stack_clone,
            &previous.ui.layout,
        );
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_add_root_leaf(move |panel_code| {
        let (panel, preset_actions) = resolve_layout_panel_selection(panel_code);
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let previous = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let mut updated_layout = add_root_leaf_if_empty(&previous.ui.layout, panel);
        let added_leaf_ids = newly_added_leaf_ids(&previous.ui.layout, &updated_layout);
        let added_leaf_id = added_leaf_ids.first().cloned();
        updated_layout.selected_leaf_id = first_leaf_id(&updated_layout.root);
        let mut next = with_updated_layout(&previous, updated_layout);
        if panel == LayoutPanelKind::ButtonCluster {
            if let (Some(new_leaf_id), Some(actions)) = (added_leaf_id, preset_actions) {
                upsert_button_cluster_actions_for_leaf(
                    &mut next.ui.layout.button_cluster_instances,
                    &new_leaf_id,
                    actions,
                );
            }
        }
        next = sanitize_config(next);
        push_layout_undo_snapshot(
            &layout_undo_stack_clone,
            &layout_redo_stack_clone,
            &previous.ui.layout,
        );
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_preview_layout_splitter_ratio(move |split_id, ratio| {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let snapshot = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(updated_layout) = set_split_ratio(&snapshot.ui.layout, &split_id, ratio) else {
            return;
        };
        let preview_config = with_updated_layout(&snapshot, updated_layout);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(
                &ui,
                &preview_config,
                workspace_width_px,
                workspace_height_px,
            );
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_commit_layout_splitter_ratio(move |split_id, ratio| {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let quantized_ratio = quantize_splitter_ratio_to_precision(ratio);
        let previous = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(updated_layout) = set_split_ratio(&previous.ui.layout, &split_id, quantized_ratio)
        else {
            return;
        };
        let next = with_updated_layout(&previous, updated_layout);
        push_layout_undo_snapshot(
            &layout_undo_stack_clone,
            &layout_redo_stack_clone,
            &previous.ui.layout,
        );
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_undo_last_action(move || {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let current = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(previous_layout) = pop_layout_snapshot(&layout_undo_stack_clone) else {
            return;
        };
        push_layout_snapshot(&layout_redo_stack_clone, &current.ui.layout);
        let next = with_updated_layout(&current, previous_layout);
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_layout_redo_last_action(move || {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let current = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let Some(next_layout) = pop_layout_snapshot(&layout_redo_stack_clone) else {
            return;
        };
        push_layout_snapshot(&layout_undo_stack_clone, &current.ui.layout);
        let next = with_updated_layout(&current, next_layout);
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    let config_state_clone = Arc::clone(&config_state);
    let layout_undo_stack_clone = Arc::clone(&layout_undo_stack);
    let layout_redo_stack_clone = Arc::clone(&layout_redo_stack);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_reset_layout_default(move || {
        let (workspace_width_px, workspace_height_px) =
            workspace_size_snapshot(&layout_workspace_size_clone);
        let previous = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };
        let mut reset_layout = load_system_layout_template();
        reset_layout.selected_leaf_id = first_leaf_id(&reset_layout.root);
        let next = with_updated_layout(&previous, reset_layout);
        push_layout_undo_snapshot(
            &layout_undo_stack_clone,
            &layout_redo_stack_clone,
            &previous.ui.layout,
        );
        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next.clone();
        }
        persist_state_files_with_config_path(&next, &config_file_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_layout_to_ui(&ui, &next, workspace_width_px, workspace_height_px);
        }
    });

    // Wire up settings open handler
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
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
            workspace_size_snapshot(&layout_workspace_size_clone);
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(
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

    let bus_sender_clone = bus_sender.clone();
    ui.on_cast_refresh_devices(move || {
        let _ = bus_sender_clone.send(Message::Cast(CastMessage::DiscoverDevices));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cast_connect_device(move |device_id| {
        let id = device_id.trim().to_string();
        if id.is_empty() {
            return;
        }
        let _ = bus_sender_clone.send(Message::Cast(CastMessage::Connect { device_id: id }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cast_disconnect(move || {
        let _ = bus_sender_clone.send(Message::Cast(CastMessage::Disconnect));
    });

    let tooltip_hover_generation = Arc::new(Mutex::new(0u64));
    let tooltip_hover_generation_clone = Arc::clone(&tooltip_hover_generation);
    let ui_handle_clone = ui.as_weak().clone();
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
        slint::Timer::single_shot(Duration::from_millis(TOOLTIP_HOVER_DELAY_MS), move || {
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
        });
    });

    // Wire up settings apply handler
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let staged_audio_settings_clone = Arc::clone(&staged_audio_settings);
    let playback_session_active_clone = Arc::clone(&playback_session_active);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let ui_handle_clone = ui.as_weak().clone();
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
              dark_mode_enabled,
              sample_rate_mode_index,
              resampler_quality_index,
              dither_on_bitdepth_reduce,
              downmix_higher_channel_tracks,
              cast_allow_transcode_fallback| {
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

            let next_config = sanitize_config(Config {
                output: OutputConfig {
                    output_device_name: if output_device_auto {
                        String::new()
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
                    dark_mode: dark_mode_enabled,
                    playlist_album_art_column_min_width_px: previous_config
                        .ui
                        .playlist_album_art_column_min_width_px,
                    playlist_album_art_column_max_width_px: previous_config
                        .ui
                        .playlist_album_art_column_max_width_px,
                    layout: previous_config.ui.layout.clone(),
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
                workspace_size_snapshot(&layout_workspace_size_clone);
            if let Some(ui) = ui_handle_clone.upgrade() {
                apply_config_to_ui(
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
            let runtime_config = resolve_effective_runtime_config(
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
            let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
                runtime_config,
            )));
        },
    );

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_toggle_playlist_column(move |column_index| {
        let column_idx = column_index.max(0) as usize;
        let previous_config = {
            let state = config_state_clone
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

        let next_config = sanitize_config(Config {
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

        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(&next_config, &config_file_clone);

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_add_custom_playlist_column(move |name, format| {
        let trimmed_name = name.trim();
        let trimmed_format = format.trim();
        if trimmed_name.is_empty() || trimmed_format.is_empty() {
            return;
        }

        let previous_config = {
            let state = config_state_clone
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

        let next_config = sanitize_config(Config {
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

        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(&next_config, &config_file_clone);

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_delete_custom_playlist_column(move |column_index| {
        let column_idx = column_index.max(0) as usize;
        if let Some(ui) = ui_handle_clone.upgrade() {
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
            let state = config_state_clone
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

        let next_config = sanitize_config(Config {
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

        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(&next_config, &config_file_clone);

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_reorder_playlist_columns(move |from_visible_index, to_visible_index| {
        let from_visible_index = from_visible_index.max(0) as usize;
        let to_visible_index = to_visible_index.max(0) as usize;
        debug!(
            "Reorder playlist columns requested: from_visible_index={} to_visible_index={}",
            from_visible_index, to_visible_index
        );
        let previous_config = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            state.clone()
        };

        let updated_columns = reorder_visible_playlist_columns(
            &previous_config.ui.playlist_columns,
            from_visible_index,
            to_visible_index,
        );
        if updated_columns == previous_config.ui.playlist_columns {
            debug!("Reorder playlist columns: no effective change");
            return;
        }

        let next_config = sanitize_config(Config {
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

        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_playlist_columns_to_ui(&ui, &next_config);
        }

        {
            let mut state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            *state = next_config.clone();
        }

        persist_state_files_with_config_path(&next_config, &config_file_clone);

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_override = runtime_output_override_snapshot(&runtime_output_override_clone);
        let runtime_config = resolve_effective_runtime_config(
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
        let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
            runtime_config,
        )));
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::SetActivePlaylistColumnOrder(playlist_column_order_keys(
                &next_config.ui.playlist_columns,
            )),
        ));
    });

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
    let integration_manager_bus_receiver = bus_sender.subscribe();
    let integration_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut integration_manager = IntegrationManager::new(
            integration_manager_bus_receiver,
            integration_manager_bus_sender,
        );
        integration_manager.run();
    });
    let startup_opensubsonic_seed = find_opensubsonic_backend(&config).map(|backend| {
        let (password, status_text) = match get_opensubsonic_password(OPENSUBSONIC_PROFILE_ID) {
            Ok(password) => {
                let status = if backend.enabled && password.is_none() {
                    "Missing saved password".to_string()
                } else {
                    "Restored from config".to_string()
                };
                (password, Some(status))
            }
            Err(error) => {
                warn!(
                    "Failed to load OpenSubsonic credential from credential store: {}",
                    error
                );
                (None, Some(format!("Credential store error: {error}")))
            }
        };
        let snapshot = opensubsonic_profile_snapshot(backend, status_text);
        let connect_now = backend.enabled && password.is_some();
        (snapshot, password, connect_now)
    });
    if let Some((snapshot, password, connect_now)) = startup_opensubsonic_seed.clone() {
        let _ = bus_sender.send(Message::Integration(
            IntegrationMessage::UpsertBackendProfile {
                profile: snapshot,
                password,
                connect_now,
            },
        ));
    }

    let playlist_manager_bus_receiver = bus_sender.subscribe();
    let playlist_manager_bus_sender = bus_sender.clone();
    let db_manager = DbManager::new().expect("Failed to initialize database");
    thread::spawn(move || {
        let mut playlist_manager = PlaylistManager::new(
            Playlist::new(),
            playlist_manager_bus_receiver,
            playlist_manager_bus_sender,
            db_manager,
            playlist_bulk_import_rx,
        );
        playlist_manager.run();
    });

    // Setup library manager
    let library_manager_bus_receiver = bus_sender.subscribe();
    let library_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut library_manager = LibraryManager::new(
            library_manager_bus_receiver,
            library_manager_bus_sender,
            db_manager,
            library_scan_progress_tx,
        );
        library_manager.run();
    });

    // Setup library enrichment manager
    let enrichment_manager_bus_receiver = bus_sender.subscribe();
    let enrichment_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut enrichment_manager = LibraryEnrichmentManager::new(
            enrichment_manager_bus_receiver,
            enrichment_manager_bus_sender,
            db_manager,
        );
        enrichment_manager.run();
    });

    // Setup metadata manager
    let metadata_manager_bus_receiver = bus_sender.subscribe();
    let metadata_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut metadata_manager = MetadataManager::new(
            metadata_manager_bus_receiver,
            metadata_manager_bus_sender,
            db_manager,
        );
        metadata_manager.run();
    });

    // Setup media controls manager
    let media_controls_bus_receiver = bus_sender.subscribe();
    let media_controls_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut media_controls_manager =
            MediaControlsManager::new(media_controls_bus_receiver, media_controls_bus_sender);
        media_controls_manager.run();
    });

    // Setup cast manager
    let cast_manager_bus_receiver = bus_sender.subscribe();
    let cast_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut cast_manager = CastManager::new(cast_manager_bus_receiver, cast_manager_bus_sender);
        cast_manager.run();
    });

    // Setup UI manager
    let ui_manager_bus_sender = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    let initial_online_metadata_enabled = config.library.online_metadata_enabled;
    let initial_online_metadata_prompt_pending = config.library.online_metadata_prompt_pending;
    thread::spawn(move || {
        let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ui_manager = UiManager::new(
                ui_handle_clone,
                ui_manager_bus_sender.subscribe(),
                ui_manager_bus_sender.clone(),
                initial_online_metadata_enabled,
                initial_online_metadata_prompt_pending,
                library_scan_progress_rx,
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
    if let Some((snapshot, password, _)) = startup_opensubsonic_seed {
        let _ = bus_sender.send(Message::Integration(
            IntegrationMessage::UpsertBackendProfile {
                profile: snapshot,
                password,
                connect_now: false,
            },
        ));
    }

    // Setup AudioPlayer
    let player_bus_sender = bus_sender.clone();
    let player_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_player = AudioPlayer::new(player_bus_receiver, player_bus_sender);
        audio_player.run();
    });

    let _ = bus_sender.send(Message::Integration(IntegrationMessage::RequestSnapshot));

    // Re-detect output options only when an audio-device open coincides with
    // an output-device inventory change (startup has already done initial detection).
    let mut device_event_receiver = bus_sender.subscribe();
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let output_device_inventory_clone = Arc::clone(&output_device_inventory);
    let ui_handle_clone = ui.as_weak().clone();
    let layout_workspace_size_clone = Arc::clone(&layout_workspace_size);
    let runtime_output_override_clone = Arc::clone(&runtime_output_override);
    let runtime_audio_state_clone = Arc::clone(&runtime_audio_state);
    let staged_audio_settings_clone = Arc::clone(&staged_audio_settings);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    thread::spawn(move || loop {
        match device_event_receiver.blocking_recv() {
            Ok(Message::Config(ConfigMessage::SetRuntimeOutputRate {
                sample_rate_hz,
                reason,
            })) => {
                {
                    let mut runtime_override = runtime_output_override_clone
                        .lock()
                        .expect("runtime output override lock poisoned");
                    runtime_override.sample_rate_hz = Some(sample_rate_hz.clamp(8_000, 192_000));
                }
                debug!(
                    "Runtime output sample-rate override set to {} Hz ({})",
                    sample_rate_hz, reason
                );
                let persisted_config = {
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
                let runtime_override =
                    runtime_output_override_snapshot(&runtime_output_override_clone);
                let runtime = resolve_effective_runtime_config(
                    &persisted_config,
                    &options_snapshot,
                    Some(&runtime_override),
                    &runtime_audio_state_clone,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                {
                    let mut last_signature = last_runtime_signature_clone
                        .lock()
                        .expect("runtime signature lock poisoned");
                    *last_signature = runtime_signature;
                }
                let _ =
                    bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(runtime)));
            }
            Ok(Message::Config(ConfigMessage::ClearRuntimeOutputRateOverride)) => {
                {
                    let mut runtime_override = runtime_output_override_clone
                        .lock()
                        .expect("runtime output override lock poisoned");
                    runtime_override.sample_rate_hz = None;
                }
                debug!("Runtime output sample-rate override cleared");
                let mut force_broadcast_runtime = false;
                if let Some(staged) = {
                    let mut pending = staged_audio_settings_clone
                        .lock()
                        .expect("staged audio settings lock poisoned");
                    pending.take()
                } {
                    let mut runtime_audio_state = runtime_audio_state_clone
                        .lock()
                        .expect("runtime audio state lock poisoned");
                    runtime_audio_state.output = staged.output;
                    runtime_audio_state.cast = staged.cast;
                    force_broadcast_runtime = true;
                }
                let persisted_config = {
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
                let runtime_override =
                    runtime_output_override_snapshot(&runtime_output_override_clone);
                let runtime = resolve_effective_runtime_config(
                    &persisted_config,
                    &options_snapshot,
                    Some(&runtime_override),
                    &runtime_audio_state_clone,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                let should_broadcast_runtime = {
                    let mut last_signature = last_runtime_signature_clone
                        .lock()
                        .expect("runtime signature lock poisoned");
                    if *last_signature == runtime_signature {
                        force_broadcast_runtime
                    } else {
                        *last_signature = runtime_signature;
                        true
                    }
                };
                if should_broadcast_runtime {
                    let _ = bus_sender_clone
                        .send(Message::Config(ConfigMessage::ConfigChanged(runtime)));
                }
            }
            Ok(Message::Config(ConfigMessage::AudioDeviceOpened { .. })) => {
                let current_inventory = snapshot_output_device_names(&cpal::default_host());
                let inventory_changed = {
                    let mut inventory = output_device_inventory_clone
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
                    let state = config_state_clone
                        .lock()
                        .expect("config state lock poisoned");
                    state.clone()
                };
                let detected_options = detect_output_settings_options(&persisted_config);
                {
                    let mut options = output_options_clone
                        .lock()
                        .expect("output options lock poisoned");
                    *options = detected_options.clone();
                }

                let _ = bus_sender_clone.send(Message::Config(
                    ConfigMessage::OutputDeviceCapabilitiesChanged {
                        verified_sample_rates: detected_options.verified_sample_rate_values.clone(),
                    },
                ));

                let runtime_override =
                    runtime_output_override_snapshot(&runtime_output_override_clone);
                let runtime = resolve_effective_runtime_config(
                    &persisted_config,
                    &detected_options,
                    Some(&runtime_override),
                    &runtime_audio_state_clone,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                let should_broadcast_runtime = {
                    let mut last_signature = last_runtime_signature_clone
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
                    let _ = bus_sender_clone
                        .send(Message::Config(ConfigMessage::ConfigChanged(runtime)));
                }

                let ui_weak = ui_handle_clone.clone();
                let config_for_ui = persisted_config.clone();
                let options_for_ui = detected_options.clone();
                let (workspace_width_px, workspace_height_px) =
                    workspace_size_snapshot(&layout_workspace_size_clone);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        apply_config_to_ui(
                            &ui,
                            &config_for_ui,
                            &options_for_ui,
                            workspace_width_px,
                            workspace_height_px,
                        );
                    }
                });
            }
            Ok(Message::Playlist(PlaylistMessage::ActivePlaylistColumnOrder(
                column_order_keys,
            ))) => {
                let Some(column_order_keys) = column_order_keys else {
                    continue;
                };

                let next_config = {
                    let mut state = config_state_clone
                        .lock()
                        .expect("config state lock poisoned");
                    let reordered_columns =
                        apply_column_order_keys(&state.ui.playlist_columns, &column_order_keys);
                    if reordered_columns == state.ui.playlist_columns {
                        continue;
                    }
                    let updated = sanitize_config(Config {
                        output: state.output.clone(),
                        cast: state.cast.clone(),
                        ui: UiConfig {
                            show_layout_edit_intro: state.ui.show_layout_edit_intro,
                            show_tooltips: state.ui.show_tooltips,
                            auto_scroll_to_playing_track: state.ui.auto_scroll_to_playing_track,
                            dark_mode: state.ui.dark_mode,
                            playlist_album_art_column_min_width_px: state
                                .ui
                                .playlist_album_art_column_min_width_px,
                            playlist_album_art_column_max_width_px: state
                                .ui
                                .playlist_album_art_column_max_width_px,
                            layout: state.ui.layout.clone(),
                            playlist_columns: reordered_columns,
                            window_width: state.ui.window_width,
                            window_height: state.ui.window_height,
                            volume: state.ui.volume,
                            playback_order: state.ui.playback_order,
                            repeat_mode: state.ui.repeat_mode,
                        },
                        library: state.library.clone(),
                        buffering: state.buffering.clone(),
                        integrations: state.integrations.clone(),
                    });
                    *state = updated.clone();
                    updated
                };

                let options_snapshot = {
                    let options = output_options_clone
                        .lock()
                        .expect("output options lock poisoned");
                    options.clone()
                };

                let runtime_override =
                    runtime_output_override_snapshot(&runtime_output_override_clone);
                let runtime = resolve_effective_runtime_config(
                    &next_config,
                    &options_snapshot,
                    Some(&runtime_override),
                    &runtime_audio_state_clone,
                );
                let runtime_signature = OutputRuntimeSignature::from_output(&runtime.output);
                {
                    let mut last_signature = last_runtime_signature_clone
                        .lock()
                        .expect("runtime signature lock poisoned");
                    *last_signature = runtime_signature;
                }
                let _ =
                    bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(runtime)));

                let ui_weak = ui_handle_clone.clone();
                let config_for_ui = next_config;
                let options_for_ui = options_snapshot;
                let (workspace_width_px, workspace_height_px) =
                    workspace_size_snapshot(&layout_workspace_size_clone);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        apply_config_to_ui(
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
                let ui_weak = ui_handle_clone.clone();
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
                let ui_weak = ui_handle_clone.clone();
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
                let ui_weak = ui_handle_clone.clone();
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
                let ui_weak = ui_handle_clone.clone();
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

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Playlist(
        PlaylistMessage::RequestActivePlaylistColumnOrder,
    ));

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Playlist(
        PlaylistMessage::RequestActivePlaylistColumnWidthOverrides,
    ));

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
        runtime_config,
    )));
    let _ = bus_sender_clone.send(Message::Config(
        ConfigMessage::OutputDeviceCapabilitiesChanged {
            verified_sample_rates: initial_output_options.verified_sample_rate_values.clone(),
        },
    ));

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::SetVolume(
        config.ui.volume,
    )));

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Cast(CastMessage::DiscoverDevices));

    ui.run()?;

    let final_config = {
        let state = config_state.lock().expect("config state lock poisoned");
        state.clone()
    };
    persist_state_files(&final_config, &config_file, &layout_file);

    info!("Application exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        any::Any,
        collections::BTreeSet,
        path::{Path, PathBuf},
        rc::Rc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::config::{
        BufferingConfig, Config, LibraryConfig, OutputConfig, PlaylistColumnConfig, UiConfig,
        UiPlaybackOrder, UiRepeatMode,
    };
    use crate::layout::LayoutConfig;
    use slint::{Model, ModelRc, ModelTracker, VecModel};

    use super::{
        apply_column_order_keys, audio_settings_changed, choose_preferred_u16,
        choose_preferred_u32, clamp_width_for_visible_column, collect_audio_files_from_folder,
        default_album_art_column_width_bounds, default_button_cluster_actions_by_index,
        is_album_art_builtin_column, is_supported_audio_file, load_layout_file,
        load_system_layout_template, output_preferences_changed,
        playlist_column_key_at_visible_index, playlist_column_width_bounds,
        playlist_column_width_bounds_with_album_art, quantize_splitter_ratio_to_precision,
        reorder_visible_playlist_columns, resolve_playlist_header_column_from_x,
        resolve_playlist_header_divider_from_x, resolve_playlist_header_gap_from_x,
        resolve_runtime_config, sanitize_config, sanitize_playlist_columns,
        select_manual_output_option_index_u32, select_output_option_index_u16,
        serialize_config_with_preserved_comments, serialize_layout_with_preserved_comments,
        should_apply_custom_column_delete, sidebar_width_from_window, system_layout_template_text,
        update_or_replace_vec_model, visible_playlist_column_kinds, OutputSettingsOptions,
        RuntimeOutputOverride,
    };

    fn unique_temp_directory(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "roqtune_{}_{}_{}",
            test_name,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn test_update_or_replace_vec_model_reuses_existing_vec_model_when_len_matches() {
        let current: ModelRc<i32> = ModelRc::from(Rc::new(VecModel::from(vec![1, 2])));
        let original_ptr = current
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected") as *const VecModel<i32>;

        let updated = update_or_replace_vec_model(current.clone(), vec![9, 8]);
        let updated_model = updated
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected");
        let updated_ptr = updated_model as *const VecModel<i32>;

        assert_eq!(original_ptr, updated_ptr);
        assert_eq!(updated_model.row_count(), 2);
        assert_eq!(updated_model.row_data(0), Some(9));
        assert_eq!(updated_model.row_data(1), Some(8));
    }

    #[test]
    fn test_update_or_replace_vec_model_reuses_existing_vec_model_when_len_changes() {
        let current: ModelRc<i32> = ModelRc::from(Rc::new(VecModel::from(vec![1, 2])));
        let original_ptr = current
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected") as *const VecModel<i32>;

        let updated = update_or_replace_vec_model(current.clone(), vec![7, 6, 5]);
        let updated_model = updated
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected");
        let updated_ptr = updated_model as *const VecModel<i32>;

        assert_eq!(original_ptr, updated_ptr);
        assert_eq!(updated_model.row_count(), 3);
        assert_eq!(updated_model.row_data(0), Some(7));
        assert_eq!(updated_model.row_data(1), Some(6));
        assert_eq!(updated_model.row_data(2), Some(5));
    }

    #[test]
    fn test_update_or_replace_vec_model_falls_back_to_vec_model_for_non_vec_model_inputs() {
        struct StaticModel {
            values: Vec<i32>,
        }

        impl Model for StaticModel {
            type Data = i32;

            fn row_count(&self) -> usize {
                self.values.len()
            }

            fn row_data(&self, row: usize) -> Option<Self::Data> {
                self.values.get(row).copied()
            }

            fn model_tracker(&self) -> &dyn ModelTracker {
                &()
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let current: ModelRc<i32> = ModelRc::new(StaticModel { values: vec![1, 2] });
        assert!(current.as_any().downcast_ref::<VecModel<i32>>().is_none());

        let updated = update_or_replace_vec_model(current, vec![4, 3]);
        let updated_model = updated
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected after fallback creation");
        assert_eq!(updated_model.row_count(), 2);
        assert_eq!(updated_model.row_data(0), Some(4));
        assert_eq!(updated_model.row_data(1), Some(3));
    }

    #[test]
    fn test_quantize_splitter_ratio_to_precision_rounds_to_two_decimals() {
        assert_eq!(quantize_splitter_ratio_to_precision(0.1234), 0.12);
        assert_eq!(quantize_splitter_ratio_to_precision(0.1251), 0.13);
        assert_eq!(quantize_splitter_ratio_to_precision(0.9999), 1.0);
    }

    #[test]
    fn test_quantize_splitter_ratio_to_precision_keeps_non_finite_values() {
        assert!(quantize_splitter_ratio_to_precision(f32::INFINITY).is_infinite());
        assert!(quantize_splitter_ratio_to_precision(f32::NEG_INFINITY).is_infinite());
        assert!(quantize_splitter_ratio_to_precision(f32::NAN).is_nan());
    }

    #[test]
    fn test_import_menu_exposes_add_files_and_add_folder_options() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> show_import_menu: false;"),
            "App window should expose import menu state"
        );
        assert!(
            slint_ui.contains("text: \"Add files\"")
                && slint_ui.contains("text: \"Add folders\"")
                && slint_ui.contains("text: \"Create new playlist\""),
            "Import menu should expose Add files, Add folders, and Create new playlist actions"
        );
        assert!(
            slint_ui.contains("callback open_folder();"),
            "App window should expose folder-import callback"
        );
    }

    #[test]
    fn test_library_view_shows_add_folder_cta_when_no_folders_are_configured() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains(
                "property <bool> has-configured-folders: root.settings_library_folders.length > 0;"
            ),
            "Library list container should detect when no folders are configured"
        );
        assert!(
            slint_ui.contains("if !library-list-container.has-configured-folders : Rectangle {")
                && slint_ui.contains("text: \"Add folders to get started\"")
                && slint_ui.contains("root.library_add_folder();"),
            "Library view should expose an in-view call-to-action to add folders"
        );
    }

    #[test]
    fn test_library_folder_menu_exposes_add_and_configure_actions() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> show_library_folder_menu: false;"),
            "App window should expose library folder menu state"
        );
        assert!(
            slint_ui.contains("text: \"Add folder\"")
                && slint_ui.contains("root.library_add_folder();"),
            "Library folder menu should expose a direct Add folder action"
        );
        assert!(
            slint_ui.contains("text: \"Configure folders ...\"")
                && slint_ui.contains("root.open_settings();")
                && slint_ui.contains("root.settings_dialog_tab_index = 2;"),
            "Library folder menu should open settings directly to the Library tab"
        );
    }

    #[test]
    fn test_settings_menu_exposes_layout_edit_toggle_and_settings_entry() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> show_settings_menu: false;"),
            "App window should expose settings action menu state"
        );
        assert!(
            slint_ui.contains("if (action-id == 10) {") && slint_ui.contains("root.show_settings_menu = true;"),
            "Settings action should open the settings action menu instead of the full settings dialog directly"
        );
        assert!(
            slint_ui.contains("text: \"Layout Editing Mode\"")
                && slint_ui.contains("root.open_layout_editor();"),
            "Settings action menu should expose layout editing mode toggle"
        );
        assert!(
            slint_ui.contains("quick-layout-toggle := Switch {")
                && slint_ui.contains("checked: root.layout_edit_mode;")
                && slint_ui.contains("width: 36px;"),
            "Settings action menu layout mode control should use compact switch control"
        );
        assert!(
            slint_ui.contains("text: \"Settings\"") && slint_ui.contains("root.open_settings();"),
            "Settings action menu should still expose normal settings dialog entry"
        );
    }

    #[test]
    fn test_settings_dialog_exposes_layout_tutorial_visibility_toggle() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains("in-out property <bool> settings_show_layout_edit_tutorial: true;"),
            "Settings dialog should expose tutorial visibility state"
        );
        assert!(
            slint_ui.contains("in-out property <bool> settings_show_tooltips: true;")
                && slint_ui.contains("in-out property <bool> show_tooltips_enabled: true;"),
            "Settings dialog should expose tooltip settings and runtime state"
        );
        assert!(
            slint_ui
                .contains("in-out property <bool> settings_auto_scroll_to_playing_track: true;"),
            "Settings dialog should expose auto-scroll playing-track setting"
        );
        assert!(
            slint_ui.contains("in-out property <bool> settings_dark_mode: true;"),
            "Settings dialog should expose persisted dark-mode setting"
        );
        assert!(
            slint_ui.contains("in-out property <int> settings_dialog_tab_index: 0;")
                && slint_ui
                    .contains("labels: [\"General\", \"Audio\", \"Library\", \"Integrations\"];"),
            "Settings dialog should expose and render General/Audio/Library/Integrations tabs"
        );
        assert!(
            slint_ui.contains("text: \"Show layout editing mode tutorial\""),
            "Settings dialog should provide tutorial visibility toggle row"
        );
        assert!(
            slint_ui.contains("if root.settings_dialog_tab_index == 0 : ScrollView {")
                && slint_ui.contains("settings-layout-intro-toggle := Switch {")
                && slint_ui.contains("x: parent.width - self.width - 8px;")
                && slint_ui.contains("checked <=> root.settings_show_layout_edit_tutorial;"),
            "General tab tutorial row should keep compact right-aligned switch binding"
        );
        assert!(
            slint_ui.contains("if root.settings_dialog_tab_index == 2 : VerticalLayout {")
                && slint_ui.contains("text: \"Library Folders\"")
                && slint_ui.contains("text: \"Add Folder\"")
                && slint_ui.contains("text: \"Rescan\""),
            "Library tab should expose folder management controls"
        );
        assert!(
            slint_ui.contains("root.settings_show_layout_edit_tutorial"),
            "Settings apply flow should submit tutorial visibility flag"
        );
        assert!(
            slint_ui.contains("text: \"Show tooltips\"")
                && slint_ui.contains("checked <=> root.settings_show_tooltips;"),
            "General tab should expose tooltip visibility toggle row"
        );
        assert!(
            slint_ui.contains("text: \"Auto scroll to playing track\"")
                && slint_ui.contains("checked <=> root.settings_auto_scroll_to_playing_track;"),
            "General tab should expose auto-scroll playing-track toggle row"
        );
        assert!(
            slint_ui.contains("text: \"Appearance\"")
                && slint_ui.contains("text: \"Dark mode\"")
                && slint_ui.contains("checked <=> root.settings_dark_mode;"),
            "General tab should expose appearance section with dark-mode toggle"
        );
        assert!(
            slint_ui.contains(
                "callback apply_settings(int, int, int, int, string, string, string, string, bool, bool, bool, bool, int, int, bool, bool, bool);"
            ),
            "Apply settings callback should include auto-scroll, dark mode, sample-rate mode, resampler quality, dither, and downmix controls"
        );
        assert!(
            slint_ui.contains("label: \"Output Sample Rate\"")
                && slint_ui.contains("options: root.settings_sample_rate_mode_options;")
                && slint_ui.contains("selected_index <=> root.settings_sample_rate_mode_index;")
                && slint_ui.contains("if root.settings_dialog_tab_index == 1 : ScrollView {"),
            "Audio tab should expose Match Content and Manual sample-rate mode selector"
        );
    }

    #[test]
    fn test_default_utility_button_cluster_preset_excludes_layout_editor_action() {
        assert_eq!(
            default_button_cluster_actions_by_index(2),
            vec![7, 8, 11, 10],
            "Utility preset should not include layout editor by default"
        );
    }

    #[test]
    fn test_is_supported_audio_file_checks_known_extensions_case_insensitively() {
        assert!(is_supported_audio_file(Path::new("/tmp/track.mp3")));
        assert!(is_supported_audio_file(Path::new("/tmp/track.FLAC")));
        assert!(is_supported_audio_file(Path::new("/tmp/track.m4a")));
        assert!(!is_supported_audio_file(Path::new("/tmp/track.txt")));
        assert!(!is_supported_audio_file(Path::new("/tmp/track")));
    }

    #[test]
    fn test_collect_audio_files_from_folder_recurses_and_filters_non_audio_files() {
        let base = unique_temp_directory("import_scan");
        let nested = base.join("nested");
        let deep = nested.join("deep");
        std::fs::create_dir_all(&deep).expect("test directories should be created");
        std::fs::write(base.join("track_b.flac"), b"").expect("should write flac fixture");
        std::fs::write(base.join("ignore.txt"), b"").expect("should write text fixture");
        std::fs::write(nested.join("track_a.MP3"), b"").expect("should write mp3 fixture");
        std::fs::write(deep.join("track_c.wav"), b"").expect("should write wav fixture");

        let imported = collect_audio_files_from_folder(&base);
        let mut expected = vec![
            nested.join("track_a.MP3"),
            base.join("track_b.flac"),
            deep.join("track_c.wav"),
        ];
        expected.sort_unstable();
        assert_eq!(imported, expected);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_focus_touch_areas_are_not_full_pane_overlays() {
        let slint_ui = include_str!("roqtune.slint");

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
        let slint_ui = include_str!("roqtune.slint");

        assert!(
            slint_ui.contains("property <length> null-column-width: 120px;"),
            "App window should define a null column width"
        );
        assert!(
            slint_ui.contains("property <bool> in-null-column: self.in-viewport")
                && slint_ui.contains("track-list.viewport-width - root.null-column-width"),
            "Track overlay should detect clicks in null column"
        );
        assert!(
            slint_ui.contains(
                "property <bool> is-in-rows: self.in-viewport && raw-row >= 0 && raw-row < root.track_model.length;"
            ),
            "Track overlay should detect when click is outside rows"
        );
        assert!(
            slint_ui.contains("root.deselect_all();"),
            "Track overlay should deselect on null-column or empty-space clicks"
        );
    }

    #[test]
    fn test_custom_column_menu_supports_delete_with_confirmation() {
        let slint_ui = include_str!("roqtune.slint");
        let menu_ui = include_str!("ui/components/menus.slint");

        assert!(
            menu_ui.contains("in property <[bool]> custom: [];"),
            "Column header menu should receive custom-column flags"
        );
        assert!(
            menu_ui.contains("callback delete-column(int);"),
            "Column header menu should expose delete callback"
        );
        assert!(
            menu_ui.contains("column-toggle := Switch {")
                && menu_ui.contains("width: 36px;")
                && menu_ui.contains("font-size: 12px;"),
            "Column header menu should use compact switch controls with consistent label sizing"
        );
        assert!(
            menu_ui.contains("source: AppIcons.close;")
                && menu_ui.contains("colorize: delete-column-ta.has-hover ? #ff5c5c : #db3f3f;"),
            "Custom columns should render a red SVG delete icon"
        );
        assert!(
            slint_ui.contains("show_delete_custom_column_confirm"),
            "App window should expose confirmation state for custom-column deletion"
        );
        assert!(
            slint_ui
                .contains("root.delete_custom_playlist_column(root.delete_custom_column_index);"),
            "Confirmed deletion should invoke custom-column delete callback"
        );
    }

    #[test]
    fn test_should_apply_custom_column_delete_requires_matching_confirmation_state() {
        assert!(!should_apply_custom_column_delete(false, 3, 3));
        assert!(!should_apply_custom_column_delete(true, -1, 0));
        assert!(!should_apply_custom_column_delete(true, 1, 0));
        assert!(should_apply_custom_column_delete(true, 2, 2));
    }

    #[test]
    fn test_playlist_header_exposes_drag_reorder_wiring() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains("column-header-drag-ta := TouchArea"),
            "Header should include drag TouchArea for column reordering"
        );
        assert!(
            slint_ui.contains("callback reorder_playlist_columns(int, int);"),
            "App window should expose reorder callback"
        );
        assert!(
            slint_ui.contains("callback playlist_columns_viewport_resized(int);"),
            "App window should expose viewport resize callback for adaptive playlist columns"
        );
        assert!(
            slint_ui.contains("callback window_resized(int, int);"),
            "App window should expose window resize callback for persistence"
        );
        assert!(
            slint_ui.contains("callback playlist_header_column_at(int) -> int;"),
            "App window should expose column hit test callback"
        );
        assert!(
            slint_ui.contains("callback playlist_header_gap_at(int) -> int;"),
            "App window should expose gap hit test callback"
        );
        assert!(
            slint_ui.contains("callback playlist_header_divider_at(int) -> int;"),
            "App window should expose divider hit test callback"
        );
        assert!(
            slint_ui.contains("callback preview_playlist_column_width(int, int);"),
            "App window should expose live resize preview callback"
        );
        assert!(
            slint_ui.contains("callback commit_playlist_column_width(int, int);"),
            "App window should expose resize commit callback"
        );
        assert!(
            slint_ui.contains("callback reset_playlist_column_width(int);"),
            "App window should expose resize reset callback"
        );
        assert!(
            slint_ui.contains("callback playlist_header_column_min_width(int) -> int;"),
            "App window should expose column min-width callback"
        );
        assert!(
            slint_ui.contains("callback playlist_header_column_max_width(int) -> int;"),
            "App window should expose column max-width callback"
        );
        assert!(
            slint_ui.contains("in-out property <int> sidebar_width_px:"),
            "Sidebar width should be controllable by viewport-driven logic"
        );
        assert!(
            slint_ui.contains("callback cycle_playlist_sort_by_column(int);")
                && slint_ui.contains("in-out property <[int]> playlist_column_sort_states: [];"),
            "Playlist header should expose sort callback and per-column sort-state model"
        );
        assert!(
            slint_ui.contains("for leaf-id[i] in root.layout_leaf_ids : Rectangle {")
                && slint_ui.contains(
                    "root.layout-region-panel-kind(i) == root.panel_kind_track_list"
                ),
            "Track list should be rendered as a movable docking panel that supports duplicate instances"
        );
    }

    #[test]
    fn test_playlist_column_width_rendering_matches_runtime_width_model() {
        let slint_ui = include_str!("roqtune.slint");
        let playlist_ui = include_str!("ui/components/playlist.slint");

        assert!(
            slint_ui.contains("return max(1px, root.playlist_column_widths_px[index] * 1px);"),
            "Header width rendering should use runtime column widths directly"
        );
        assert!(
            slint_ui.contains("let min_width_px = max(1, root.column_resize_min_width_px);"),
            "Resize preview clamp should honor callback-provided minimums without a fixed floor"
        );
        assert!(
            playlist_ui.contains("return max(1px, root.column-widths-px[index] * 1px);"),
            "Row cells should use the same runtime width model as header hit-testing"
        );
    }

    #[test]
    fn test_layout_editor_and_splitter_callbacks_are_wired_in_slint() {
        let slint_ui = include_str!("roqtune.slint");
        let controls_ui = include_str!("ui/components/controls.slint");
        let media_ui = include_str!("ui/components/media.slint");
        let status_ui = include_str!("ui/components/status.slint");
        assert!(
            slint_ui.contains("callback open_layout_editor();"),
            "Layout editor open callback should be declared"
        );
        assert!(
            slint_ui.contains("callback layout_intro_proceed(bool);")
                && slint_ui.contains("callback layout_intro_cancel(bool);"),
            "Layout intro callbacks should be declared"
        );
        assert!(
            slint_ui.contains("callback layout_select_leaf(string);")
                && slint_ui.contains("callback layout_replace_selected_leaf(int);")
                && slint_ui.contains("callback layout_split_selected_leaf(int, int);")
                && slint_ui.contains("callback layout_delete_selected_leaf();"),
            "Leaf edit callbacks should be declared"
        );
        assert!(
            slint_ui.contains("callback preview_layout_splitter_ratio(string, float);"),
            "Splitter preview callback should be declared"
        );
        assert!(
            slint_ui.contains("callback commit_layout_splitter_ratio(string, float);"),
            "Splitter commit callback should be declared"
        );
        assert!(
            slint_ui.contains("x: splitter.axis == 1 ? -4px : 0px;")
                && slint_ui
                    .contains("width: splitter.axis == 1 ? parent.width + 8px : parent.width;")
                && slint_ui.contains("y: splitter.axis == 0 ? -4px : 0px;")
                && slint_ui
                    .contains("height: splitter.axis == 0 ? parent.height + 8px : parent.height;"),
            "Layout splitter drag hit areas should remain fixed-size while dragging"
        );
        assert!(
            !slint_ui.contains("active-x-pad") && !slint_ui.contains("active-y-pad"),
            "Layout splitter drag should not mutate touch-area geometry during active drag"
        );
        assert!(
            slint_ui.contains(
                "event.kind == PointerEventKind.up && event.button == PointerEventButton.left"
            ) && slint_ui.contains(
                "event.kind == PointerEventKind.cancel && root.layout_active_splitter_index == i"
            ) && slint_ui.contains("moved => {"),
            "Layout splitter drag should preview on move and end on up/cancel"
        );
        assert!(
            slint_ui.contains("callback layout_workspace_resized(int, int);"),
            "Workspace resize callback should be declared"
        );
        assert!(
            slint_ui.contains("in-out property <[int]> layout_region_panel_kinds:"),
            "Layout region assignment property should exist"
        );
        assert!(
            status_ui.contains("component StatusBar inherits Rectangle"),
            "Status bar should be extracted into a reusable component"
        );
        assert!(
            slint_ui.contains("in-out property <[LayoutSplitterModel]> layout_splitters: []"),
            "Layout editor should expose dynamic splitter model"
        );
        assert!(
            controls_ui.contains("component ButtonCluster inherits Rectangle")
                && media_ui.contains("component SeekBarControl inherits Rectangle")
                && media_ui.contains("component VolumeSliderControl inherits Rectangle"),
            "Control bar should be composed from reusable sub-components"
        );
        assert!(
            media_ui.contains("component AlbumArtViewer inherits Rectangle")
                && media_ui.contains("component MetadataViewer inherits Rectangle"),
            "Album art panel should be split into viewer and metadata components"
        );
        assert!(
            slint_ui.contains("callback open_control_cluster_menu_for_leaf(string, int, int);"),
            "Control cluster menu open callback should be declared"
        );
        assert!(
            slint_ui.contains("callback add_control_cluster_button(string, int);"),
            "Control cluster add callback should be declared"
        );
        assert!(
            slint_ui.contains("callback remove_control_cluster_button(string, int);"),
            "Control cluster remove callback should be declared"
        );
        assert!(
            slint_ui.contains("event.text == Key.F6")
                && slint_ui.contains("event.modifiers.control")
                && slint_ui.contains("root.open_layout_editor();"),
            "Layout editor should be reachable via keyboard shortcut"
        );
        assert!(
            slint_ui.contains("(event.text == \"f\" || event.text == \"F\")")
                && slint_ui.contains("root.open_library_search();")
                && slint_ui.contains("root.open_playlist_search();"),
            "Ctrl+F should route to library or playlist search by active mode"
        );
        assert!(
            slint_ui.contains("callback library_search_query_edited(string);")
                && !slint_ui.contains("callback set_library_search_query(string);"),
            "Library search should expose edited callback naming"
        );
        assert!(
            slint_ui.contains("callback open_global_library_search();"),
            "Library pane should expose global search callback"
        );
        assert!(
            slint_ui.contains("library-search-input := LineEdit {")
                && slint_ui.contains("edited(text) => {")
                && slint_ui.contains("root.library_search_query_edited(text);"),
            "Library search input should use the edited(text) callback form"
        );
        assert!(
            slint_ui.contains("library-list-row-ta := TouchArea {")
                && slint_ui.contains("double-clicked => {")
                && slint_ui.contains("root.library_item_activated(row);"),
            "Library rows should activate detail/playback via native double-click handler"
        );
        assert!(
            slint_ui.contains("icon-source: AppIcons.search;")
                && slint_ui.contains("root.open_global_library_search();")
                && slint_ui.contains("icon-source: AppIcons.refresh;")
                && slint_ui.contains("root.library_rescan();"),
            "Library sidebar should include global search button before rescan"
        );
        assert!(
            !slint_ui.contains("library-search-input := LineEdit {\n                                placeholder-text: \"Search this library view...\";\n                                horizontal-stretch: 1;\n                                init => { self.focus(); }"),
            "Library search input should not request focus in init"
        );
        assert!(
            slint_ui.contains("(event.text == \"c\" || event.text == \"C\")")
                && slint_ui.contains("root.copy_selected_tracks();")
                && slint_ui.contains("(event.text == \"x\" || event.text == \"X\")")
                && slint_ui.contains("root.cut_selected_tracks();")
                && slint_ui.contains("(event.text == \"v\" || event.text == \"V\")")
                && slint_ui.contains("root.paste_copied_tracks();"),
            "Playlist rows should support Ctrl+C/Ctrl+X/Ctrl+V shortcuts"
        );
        assert!(
            slint_ui.contains("(event.text == \"z\" || event.text == \"Z\")")
                && slint_ui.contains("root.layout_undo_last_action();")
                && slint_ui.contains("root.undo_last_action();")
                && slint_ui.contains("root.layout_redo_last_action();")
                && slint_ui.contains("root.redo_last_action();"),
            "Ctrl+Z and Ctrl+Shift+Z should route undo and redo by mode"
        );
        assert!(
            slint_ui.contains("callback copy_selected_tracks();")
                && slint_ui.contains("callback cut_selected_tracks();")
                && slint_ui.contains("callback paste_copied_tracks();")
                && slint_ui.contains("callback undo_last_action();")
                && slint_ui.contains("callback redo_last_action();")
                && slint_ui.contains("callback layout_redo_last_action();"),
            "App window should expose track-list and layout undo/redo callbacks"
        );
        assert!(
            slint_ui.contains("event.text == Key.Escape && root.layout_edit_mode"),
            "Escape should exit layout edit mode quickly"
        );
        assert!(
            slint_ui.contains("root.show_layout_editor_dialog = false;")
                && slint_ui.contains("text: \"Start Editing\""),
            "Layout intro dialog should expose proceed action"
        );
        assert!(
            slint_ui.contains("background: root.layout_edit_mode ? #252525 : #202020;")
                && slint_ui.contains("if root.layout_edit_mode : Text {")
                && slint_ui.contains("text: \"Spacer\""),
            "Spacer panels should stay visually invisible outside layout edit mode"
        );
    }

    #[test]
    fn test_resolve_playlist_header_column_from_x_uses_variable_widths() {
        let widths = vec![70, 200, 180];
        assert_eq!(resolve_playlist_header_column_from_x(10, &widths), 0);
        assert_eq!(resolve_playlist_header_column_from_x(75, &widths), -1);
        assert_eq!(resolve_playlist_header_column_from_x(120, &widths), 1);
        assert_eq!(resolve_playlist_header_column_from_x(285, &widths), -1);
        assert_eq!(resolve_playlist_header_column_from_x(330, &widths), 2);
    }

    #[test]
    fn test_resolve_playlist_header_gap_from_x_tracks_real_column_edges() {
        let widths = vec![70, 200, 180];
        assert_eq!(resolve_playlist_header_gap_from_x(0, &widths), 0);
        assert_eq!(resolve_playlist_header_gap_from_x(69, &widths), 1);
        assert_eq!(resolve_playlist_header_gap_from_x(75, &widths), 1);
        assert_eq!(resolve_playlist_header_gap_from_x(90, &widths), 1);
        assert_eq!(resolve_playlist_header_gap_from_x(279, &widths), 2);
        assert_eq!(resolve_playlist_header_gap_from_x(285, &widths), 2);
        assert_eq!(resolve_playlist_header_gap_from_x(460, &widths), 3);
    }

    #[test]
    fn test_resolve_playlist_header_divider_from_x_hits_column_boundaries() {
        let widths = vec![70, 200, 180];
        assert_eq!(resolve_playlist_header_divider_from_x(70, &widths, 4), 0);
        assert_eq!(resolve_playlist_header_divider_from_x(73, &widths, 4), 0);
        assert_eq!(resolve_playlist_header_divider_from_x(281, &widths, 4), 1);
        assert_eq!(resolve_playlist_header_divider_from_x(470, &widths, 4), 2);
        assert_eq!(resolve_playlist_header_divider_from_x(150, &widths, 4), -1);
    }

    #[test]
    fn test_sidebar_width_from_window_is_responsive_and_clamped() {
        assert_eq!(sidebar_width_from_window(600), 180);
        assert_eq!(sidebar_width_from_window(900), 216);
        assert_eq!(sidebar_width_from_window(2000), 300);
    }

    #[test]
    fn test_reorder_visible_playlist_columns_preserves_hidden_slot_layout() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Hidden".to_string(),
                format: "{genre}".to_string(),
                enabled: false,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Artist".to_string(),
                format: "{artist}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album".to_string(),
                format: "{album}".to_string(),
                enabled: true,
                custom: false,
            },
        ];

        let reordered = reorder_visible_playlist_columns(&columns, 0, 2);
        let formats: Vec<String> = reordered.into_iter().map(|column| column.format).collect();
        assert_eq!(
            formats,
            vec!["{artist}", "{genre}", "{album}", "{title}"],
            "Visible columns should reorder while hidden slot remains in place"
        );
    }

    #[test]
    fn test_apply_column_order_keys_prioritizes_saved_order_and_appends_unknowns() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Artist".to_string(),
                format: "{artist}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album & Year".to_string(),
                format: "{album} ({year})".to_string(),
                enabled: true,
                custom: true,
            },
        ];

        let saved_order = vec![
            "custom:Album & Year|{album} ({year})".to_string(),
            "{artist}".to_string(),
        ];
        let reordered = apply_column_order_keys(&columns, &saved_order);
        let names: Vec<String> = reordered.into_iter().map(|column| column.name).collect();
        assert_eq!(names, vec!["Album & Year", "Artist", "Title"]);
    }

    #[test]
    fn test_playlist_column_key_at_visible_index_ignores_hidden_columns() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Hidden".to_string(),
                format: "{genre}".to_string(),
                enabled: false,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album & Year".to_string(),
                format: "{album} ({year})".to_string(),
                enabled: true,
                custom: true,
            },
        ];

        assert_eq!(
            playlist_column_key_at_visible_index(&columns, 0),
            Some("{title}".to_string())
        );
        assert_eq!(
            playlist_column_key_at_visible_index(&columns, 1),
            Some("custom:Album & Year|{album} ({year})".to_string())
        );
        assert_eq!(playlist_column_key_at_visible_index(&columns, 2), None);
    }

    #[test]
    fn test_playlist_column_width_bounds_for_track_custom_and_album_art_columns() {
        let track_column = PlaylistColumnConfig {
            name: "Track #".to_string(),
            format: "{track_number}".to_string(),
            enabled: true,
            custom: false,
        };
        let album_art_column = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_column = PlaylistColumnConfig {
            name: "Album & Year".to_string(),
            format: "{album} ({year})".to_string(),
            enabled: true,
            custom: true,
        };

        let track_bounds = playlist_column_width_bounds(&track_column);
        assert_eq!(track_bounds.min_px, 52);
        assert_eq!(track_bounds.max_px, 90);

        let album_art_bounds = playlist_column_width_bounds(&album_art_column);
        assert_eq!(album_art_bounds.min_px, 16);
        assert_eq!(album_art_bounds.max_px, 480);

        let custom_bounds = playlist_column_width_bounds(&custom_column);
        assert_eq!(custom_bounds.min_px, 110);
        assert_eq!(custom_bounds.max_px, 340);
    }

    #[test]
    fn test_playlist_column_width_bounds_use_custom_album_art_limits() {
        let album_art_column = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };

        let bounds = playlist_column_width_bounds_with_album_art(
            &album_art_column,
            super::ColumnWidthBounds {
                min_px: 12,
                max_px: 600,
            },
        );

        assert_eq!(bounds.min_px, 12);
        assert_eq!(bounds.max_px, 600);
    }

    #[test]
    fn test_is_album_art_builtin_column_requires_builtin_marker() {
        let builtin_album_art = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_album_art = PlaylistColumnConfig {
            name: "Custom Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: true,
        };
        let title_column = PlaylistColumnConfig {
            name: "Title".to_string(),
            format: "{title}".to_string(),
            enabled: true,
            custom: false,
        };

        assert!(is_album_art_builtin_column(&builtin_album_art));
        assert!(!is_album_art_builtin_column(&custom_album_art));
        assert!(!is_album_art_builtin_column(&title_column));
    }

    #[test]
    fn test_clamp_width_for_visible_column_respects_column_profile_bounds() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Track #".to_string(),
                format: "{track_number}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Hidden".to_string(),
                format: "{genre}".to_string(),
                enabled: false,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
        ];

        let album_art_bounds = default_album_art_column_width_bounds();
        assert_eq!(
            clamp_width_for_visible_column(&columns, 0, 10, album_art_bounds),
            Some(52)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 0, 200, album_art_bounds),
            Some(90)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 1, 1000, album_art_bounds),
            Some(440)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 3, 120, album_art_bounds),
            None
        );
    }

    #[test]
    fn test_choose_preferred_uses_priority_order_when_available() {
        let detected_u16 = BTreeSet::from([1, 2, 6]);
        let detected_u32 = BTreeSet::from([48_000, 96_000]);

        assert_eq!(choose_preferred_u16(&detected_u16, &[6, 2, 1], 2), 6);
        assert_eq!(
            choose_preferred_u32(&detected_u32, &[44_100, 48_000, 96_000], 44_100),
            48_000
        );
    }

    #[test]
    fn test_choose_preferred_falls_back_to_first_detected_when_needed() {
        let detected_u16 = BTreeSet::from([4, 8]);
        let detected_u32 = BTreeSet::from([32_000, 192_000]);

        assert_eq!(choose_preferred_u16(&detected_u16, &[2, 6], 2), 4);
        assert_eq!(
            choose_preferred_u32(&detected_u32, &[44_100, 48_000], 44_100),
            32_000
        );
    }

    #[test]
    fn test_select_output_option_index_uses_auto_and_custom_slots() {
        assert_eq!(select_output_option_index_u16(true, 2, &[1, 2], 3), 0);
        assert_eq!(select_output_option_index_u16(false, 2, &[1, 2], 3), 2);
        assert_eq!(select_output_option_index_u16(false, 7, &[1, 2], 3), 3);

        assert_eq!(
            select_manual_output_option_index_u32(48_000, &[44_100, 48_000], 2),
            1
        );
    }

    #[test]
    fn test_resolve_runtime_config_applies_auto_values_only_for_auto_fields() {
        let persisted = Config {
            output: OutputConfig {
                output_device_name: String::new(),
                output_device_auto: true,
                channel_count: 6,
                sample_rate_khz: 48_000,
                bits_per_sample: 16,
                channel_count_auto: true,
                sample_rate_auto: false,
                bits_per_sample_auto: true,
                resampler_quality: crate::config::ResamplerQuality::High,
                dither_on_bitdepth_reduce: true,
                downmix_higher_channel_tracks: true,
            },
            cast: crate::config::CastConfig::default(),
            ui: UiConfig {
                show_layout_edit_intro: true,
                show_tooltips: true,
                auto_scroll_to_playing_track: true,
                dark_mode: true,
                playlist_album_art_column_min_width_px: 16,
                playlist_album_art_column_max_width_px: 480,
                layout: LayoutConfig::default(),
                playlist_columns: crate::config::default_playlist_columns(),
                window_width: 900,
                window_height: 650,
                volume: 1.0,
                playback_order: UiPlaybackOrder::Default,
                repeat_mode: UiRepeatMode::Off,
            },
            library: LibraryConfig::default(),
            buffering: BufferingConfig::default(),
            integrations: crate::config::IntegrationsConfig::default(),
        };
        let options = OutputSettingsOptions {
            device_names: vec!["Device A".to_string(), "Device B".to_string()],
            auto_device_name: "Device B".to_string(),
            channel_values: vec![1, 2, 6],
            sample_rate_values: vec![44_100, 48_000],
            bits_per_sample_values: vec![16, 24],
            verified_sample_rate_values: vec![44_100, 48_000],
            verified_sample_rates_summary: "Verified rates: 44.1k, 48k".to_string(),
            auto_channel_value: 2,
            auto_sample_rate_value: 44_100,
            auto_bits_per_sample_value: 24,
        };

        let runtime = resolve_runtime_config(&persisted, &options, None);
        assert_eq!(runtime.output.channel_count, 2);
        assert_eq!(runtime.output.sample_rate_khz, 48_000);
        assert_eq!(runtime.output.bits_per_sample, 24);
        assert_eq!(runtime.output.output_device_name, "Device B");
    }

    #[test]
    fn test_resolve_runtime_config_applies_runtime_sample_rate_override() {
        let persisted = Config::default();
        let options = OutputSettingsOptions {
            device_names: vec!["Device A".to_string()],
            auto_device_name: "Device A".to_string(),
            channel_values: vec![2],
            sample_rate_values: vec![44_100, 48_000, 96_000],
            bits_per_sample_values: vec![16, 24],
            verified_sample_rate_values: vec![44_100, 48_000, 96_000],
            verified_sample_rates_summary: "Verified rates: 44.1k, 48k, 96k".to_string(),
            auto_channel_value: 2,
            auto_sample_rate_value: 48_000,
            auto_bits_per_sample_value: 24,
        };
        let runtime_override = RuntimeOutputOverride {
            sample_rate_hz: Some(96_000),
        };

        let runtime = resolve_runtime_config(&persisted, &options, Some(&runtime_override));
        assert_eq!(runtime.output.sample_rate_khz, 96_000);
        assert_eq!(persisted.output.sample_rate_khz, 44_100);
    }

    #[test]
    fn test_output_preferences_changed_detects_output_changes() {
        let previous = Config::default().output;
        let mut next = previous.clone();
        next.sample_rate_auto = false;

        assert!(output_preferences_changed(&previous, &next));
    }

    #[test]
    fn test_output_preferences_changed_detects_downmix_toggle() {
        let previous = Config::default().output;
        let mut next = previous.clone();
        next.downmix_higher_channel_tracks = !next.downmix_higher_channel_tracks;

        assert!(output_preferences_changed(&previous, &next));
    }

    #[test]
    fn test_output_preferences_changed_ignores_non_output_fields() {
        let previous = Config::default().output;
        let next = previous.clone();

        assert!(!output_preferences_changed(&previous, &next));
    }

    #[test]
    fn test_audio_settings_changed_detects_cast_setting_change() {
        let previous = Config::default();
        let mut next = previous.clone();
        next.cast.allow_transcode_fallback = !next.cast.allow_transcode_fallback;

        assert!(audio_settings_changed(&previous, &next));
    }

    #[test]
    fn test_sanitize_config_clamps_and_orders_album_art_width_bounds() {
        let input = Config {
            ui: UiConfig {
                playlist_album_art_column_min_width_px: 900,
                playlist_album_art_column_max_width_px: 20,
                ..Config::default().ui
            },
            ..Config::default()
        };

        let sanitized = sanitize_config(input);
        assert_eq!(sanitized.ui.playlist_album_art_column_min_width_px, 24);
        assert_eq!(sanitized.ui.playlist_album_art_column_max_width_px, 512);
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_keeps_existing_comments() {
        let existing = r#"
# top comment
[output]
# output comment
output_device_name = ""
output_device_auto = true
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24
channel_count_auto = true
sample_rate_auto = true
bits_per_sample_auto = true

[ui]
# ui comment
show_layout_edit_intro = true
playlist_album_art_column_min_width_px = 16
playlist_album_art_column_max_width_px = 480
window_width = 900
window_height = 650
volume = 1.0
button_cluster_instances = []

[[ui.playlist_columns]]
name = "Title"
format = "{title}"
enabled = true
custom = false

[buffering]
# buffering comment
player_low_watermark_ms = 12000
player_target_buffer_ms = 24000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1500
"#;

        let mut config = Config::default();
        config.output.output_device_name = "My Device".to_string();
        config.ui.volume = 0.55;

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving serialization should succeed");

        assert!(serialized.contains("# top comment"));
        assert!(serialized.contains("# output comment"));
        assert!(serialized.contains("# ui comment"));
        assert!(serialized.contains("# buffering comment"));
        assert!(serialized.contains("output_device_name = \"My Device\""));
        assert!(serialized.contains("volume = 0.55"));
        assert!(
            !serialized.contains("button_cluster_instances"),
            "layout-owned button cluster settings should not be persisted in config.toml"
        );
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_rejects_invalid_toml() {
        let invalid = "[output\noutput_device_auto = true";
        let config = Config::default();
        let result = serialize_config_with_preserved_comments(invalid, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_keeps_system_template_comments() {
        let existing = include_str!("../config/config.system.toml");
        let mut config: Config =
            toml::from_str(existing).expect("system config template should parse");
        config.ui.show_layout_edit_intro = false;

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving serialization should succeed");

        assert!(serialized.contains("# roqtune system config template"));
        assert!(serialized.contains("# ADVANCED USERS ONLY"));
        assert!(serialized.contains("# Playback volume (0.0 .. 1.0)."));
        assert!(serialized.contains("show_layout_edit_intro = false"));
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_strips_layout_owned_ui_keys() {
        let existing = include_str!("../config/config.system.toml");
        let mut config: Config =
            toml::from_str(existing).expect("system config template should parse");
        config.ui.layout.button_cluster_instances =
            vec![crate::config::ButtonClusterInstanceConfig {
                leaf_id: "n8".to_string(),
                actions: vec![1, 2, 3],
            }];

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving serialization should succeed");

        assert!(
            !serialized.contains("button_cluster_instances"),
            "button cluster settings should not be persisted in config.toml.\nserialized=\n{}",
            serialized
        );
        assert!(
            !serialized.contains("[[ui.playlist_columns]]"),
            "playlist columns should not be persisted in config.toml"
        );
        assert!(
            !serialized.contains("playlist_album_art_column_min_width_px"),
            "playlist album art width fields should not be persisted in config.toml"
        );
        assert!(
            !serialized.contains("playlist_album_art_column_max_width_px"),
            "playlist album art width fields should not be persisted in config.toml"
        );
        assert!(
            !serialized.contains("viewer_panel_instances"),
            "viewer panel settings should not be persisted in config.toml"
        );
        let parsed_back: toml_edit::DocumentMut = serialized
            .parse()
            .expect("serialized config should remain valid TOML");
        assert!(parsed_back
            .get("ui")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("button_cluster_instances"))
            .is_none());
        assert!(parsed_back
            .get("ui")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("playlist_columns"))
            .is_none());
        assert!(parsed_back
            .get("ui")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("viewer_panel_instances"))
            .is_none());
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_keeps_existing_comments() {
        let existing = r#"
# layout top comment
version = 1

# album art width comment
playlist_album_art_column_min_width_px = 16
playlist_album_art_column_max_width_px = 480

[[playlist_columns]]
name = "Title"
format = "{title}"
enabled = true
custom = false

[root]
# root comment
node_type = "leaf"
id = "l1"
panel = "track_list"
"#;
        let mut layout: LayoutConfig =
            toml::from_str(existing).expect("existing layout should parse");
        layout.playlist_album_art_column_max_width_px = 512;
        layout.button_cluster_instances = vec![crate::config::ButtonClusterInstanceConfig {
            leaf_id: "n8".to_string(),
            actions: vec![1, 2, 3],
        }];

        let serialized = serialize_layout_with_preserved_comments(existing, &layout)
            .expect("comment-preserving layout serialization should succeed");

        assert!(serialized.contains("# layout top comment"));
        assert!(serialized.contains("# album art width comment"));
        assert!(serialized.contains("# root comment"));
        assert!(serialized.contains("playlist_album_art_column_max_width_px = 512"));
        assert!(serialized.contains("[[button_cluster_instances]]"));
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_rejects_invalid_toml() {
        let invalid = "[root\nnode_type = \"leaf\"";
        let layout = LayoutConfig::default();
        let result = serialize_layout_with_preserved_comments(invalid, &layout);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_keeps_system_template_comments() {
        let existing = include_str!("../config/layout.system.toml");
        let mut layout: LayoutConfig =
            toml::from_str(existing).expect("layout system template should parse");
        layout.playlist_album_art_column_min_width_px = 24;

        let serialized = serialize_layout_with_preserved_comments(existing, &layout)
            .expect("comment-preserving layout serialization should succeed");

        assert!(serialized.contains("# roqtune layout system template"));
        assert!(serialized.contains("# Node conventions:"));
        assert!(serialized.contains("playlist_album_art_column_min_width_px = 24"));
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_keeps_unknown_keys() {
        let existing = r#"
version = 1
custom_layout_flag = "keep-me"

[root]
node_type = "leaf"
id = "l1"
panel = "track_list"
"#;
        let layout = LayoutConfig::default();

        let serialized = serialize_layout_with_preserved_comments(existing, &layout)
            .expect("comment-preserving layout serialization should succeed");
        assert!(serialized.contains("custom_layout_flag = \"keep-me\""));
    }

    #[test]
    fn test_load_system_layout_template_matches_checked_in_template() {
        let parsed_from_template: LayoutConfig = toml::from_str(system_layout_template_text())
            .expect("layout system template should parse");
        let loaded = load_system_layout_template();
        assert_eq!(loaded, parsed_from_template);
    }

    #[test]
    fn test_load_layout_file_falls_back_to_system_template_when_missing() {
        let temp_dir = unique_temp_directory("layout_missing_fallback");
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let missing_path = temp_dir.join("missing-layout.toml");
        let loaded = load_layout_file(&missing_path);
        assert_eq!(loaded, load_system_layout_template());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_load_layout_file_falls_back_to_system_template_when_invalid() {
        let temp_dir = unique_temp_directory("layout_invalid_fallback");
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let invalid_path = temp_dir.join("layout.toml");
        std::fs::write(&invalid_path, "[root\nnode_type = \"leaf\"")
            .expect("invalid layout file should be written");
        let loaded = load_layout_file(&invalid_path);
        assert_eq!(loaded, load_system_layout_template());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_keeps_unknown_keys() {
        let existing = r#"
[output]
output_device_name = ""
output_device_auto = true
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24
channel_count_auto = true
sample_rate_auto = true
bits_per_sample_auto = true

[ui]
show_layout_edit_intro = true
show_tooltips = true
window_width = 900
window_height = 650
volume = 1.0

[library]
folders = []
online_metadata_enabled = false
online_metadata_prompt_pending = true
artist_image_cache_ttl_days = 30
artist_image_cache_max_size_mb = 256
custom_library_key = "keep-me"

[buffering]
player_low_watermark_ms = 12000
player_target_buffer_ms = 24000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1500
"#;
        let config = Config::default();

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving config serialization should succeed");
        assert!(serialized.contains("custom_library_key = \"keep-me\""));
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_persists_integrations_backends() {
        let existing = r#"
[output]
output_device_name = ""
output_device_auto = true
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24
channel_count_auto = true
sample_rate_auto = true
bits_per_sample_auto = true

[cast]
allow_transcode_fallback = true

[ui]
show_layout_edit_intro = true
show_tooltips = true
auto_scroll_to_playing_track = true
dark_mode = true
window_width = 900
window_height = 650
volume = 0.8
playback_order = "default"
repeat_mode = "off"

[library]
folders = []
online_metadata_enabled = false
online_metadata_prompt_pending = true
artist_image_cache_ttl_days = 30
artist_image_cache_max_size_mb = 512

[buffering]
player_low_watermark_ms = 1500
player_target_buffer_ms = 12000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1000

[integrations]
backends = []
"#;
        let mut config = Config::default();
        config.integrations.backends = vec![crate::config::BackendProfileConfig {
            profile_id: "opensubsonic-default".to_string(),
            backend_kind: crate::config::IntegrationBackendKind::OpenSubsonic,
            display_name: "OpenSubsonic".to_string(),
            endpoint: "https://music.example.com".to_string(),
            username: "alice".to_string(),
            enabled: true,
        }];

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("integration backend should serialize");
        assert!(serialized.contains("[[integrations.backends]]"));
        assert!(serialized.contains("profile_id = \"opensubsonic-default\""));
        assert!(serialized.contains("backend_kind = \"open_subsonic\""));
        assert!(serialized.contains("endpoint = \"https://music.example.com\""));
        assert!(serialized.contains("username = \"alice\""));
        assert!(serialized.contains("enabled = true"));
    }

    #[test]
    fn test_sanitize_playlist_columns_restores_builtins_and_preserves_custom() {
        let custom = PlaylistColumnConfig {
            name: "Album & Year".to_string(),
            format: "{album} ({year})".to_string(),
            enabled: true,
            custom: true,
        };
        let input_columns = vec![
            PlaylistColumnConfig {
                name: "Artist".to_string(),
                format: "{artist}".to_string(),
                enabled: false,
                custom: false,
            },
            custom.clone(),
        ];

        let sanitized = sanitize_playlist_columns(&input_columns);
        assert!(sanitized.iter().any(|column| column.format == "{title}"));
        assert!(sanitized.iter().any(|column| column.format == "{artist}"));
        assert!(sanitized
            .iter()
            .any(|column| !column.custom && column.format == "{album_art}"));
        assert!(sanitized.iter().any(|column| column == &custom));
        assert!(sanitized.iter().any(|column| column.enabled));
    }

    #[test]
    fn test_visible_playlist_column_kinds_marks_builtin_album_art_only() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Custom Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: true,
            },
        ];

        assert_eq!(visible_playlist_column_kinds(&columns), vec![0, 1, 0]);
    }

    #[test]
    fn test_sanitize_playlist_columns_preserves_existing_builtin_order() {
        let input_columns = vec![
            PlaylistColumnConfig {
                name: "Track #".to_string(),
                format: "{track_number}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Artist".to_string(),
                format: "{artist}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album".to_string(),
                format: "{album}".to_string(),
                enabled: true,
                custom: false,
            },
        ];

        let sanitized = sanitize_playlist_columns(&input_columns);
        let visible_order: Vec<String> = sanitized
            .iter()
            .filter(|column| column.enabled)
            .map(|column| column.format.clone())
            .collect();
        assert_eq!(
            visible_order,
            vec![
                "{track_number}".to_string(),
                "{title}".to_string(),
                "{artist}".to_string(),
                "{album}".to_string(),
            ]
        );
    }
}
