mod app_bootstrap;
mod app_callbacks;
mod app_config_coordinator;
mod app_context;
mod app_runtime;
#[path = "audio/audio_decoder.rs"]
mod audio_decoder;
#[path = "audio/audio_player.rs"]
mod audio_player;
#[path = "audio/audio_probe.rs"]
mod audio_probe;
#[path = "runtime/audio_runtime_reactor.rs"]
mod audio_runtime_reactor;
mod backends;
#[path = "cast/cast_manager.rs"]
mod cast_manager;
mod config;
mod config_persistence;
mod db_manager;
mod image_pipeline;
#[path = "integration/integration_keyring.rs"]
mod integration_keyring;
#[path = "integration/integration_manager.rs"]
mod integration_manager;
#[path = "integration/integration_uri.rs"]
mod integration_uri;
mod layout;
#[path = "library/library_enrichment_manager.rs"]
mod library_enrichment_manager;
#[path = "library/library_manager.rs"]
mod library_manager;
mod media_controls_manager;
mod media_file_discovery;
#[path = "metadata/metadata_manager.rs"]
mod metadata_manager;
#[path = "metadata/metadata_tags.rs"]
mod metadata_tags;
#[path = "integration/opensubsonic_controller.rs"]
mod opensubsonic_controller;
#[path = "audio/output_option_selection.rs"]
mod output_option_selection;
#[path = "playlist/playlist.rs"]
mod playlist;
#[path = "playlist/playlist_manager.rs"]
mod playlist_manager;
mod protocol;
mod runtime_config;
mod ui;
mod ui_manager;

use std::{
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use app_config_coordinator::apply_config_update;
use app_context::AppSharedState;
use config::{
    BackendProfileConfig, BufferingConfig, Config, IntegrationsConfig, LibraryConfig, OutputConfig,
    ResamplerQuality, UiConfig, UiPlaybackOrder, UiRepeatMode,
};
#[cfg(test)]
use config_persistence::{
    load_system_layout_template, serialize_config_with_preserved_comments,
    serialize_layout_with_preserved_comments, system_layout_template_text,
};
use layout::{add_root_leaf_if_empty, sanitize_layout_config};
use log::warn;
use media_file_discovery::{
    collect_audio_files_from_dropped_paths, collect_audio_files_from_folder,
    collect_library_folders_from_dropped_paths, is_supported_audio_file,
    SUPPORTED_AUDIO_EXTENSIONS,
};
use opensubsonic_controller::find_opensubsonic_backend;
pub(crate) use output_option_selection::{
    detect_output_settings_options, snapshot_output_device_names,
};
use output_option_selection::{
    resolve_runtime_config, select_manual_output_option_index_u32,
    select_output_option_index_string, select_output_option_index_u16,
};
use protocol::Message;
use runtime_config::{RuntimeAudioState, RuntimeOutputOverride};
use slint::{language::ColorScheme, ComponentHandle, ModelRc, VecModel};
use tokio::sync::broadcast;
pub(crate) use ui::layout_editor_state::*;
use ui::playlist_columns::*;
use ui_manager::UiState;

slint::include_modules!();

const PLAYLIST_COLUMN_SPACING_PX: i32 = 10;
const PLAYLIST_COLUMN_PREVIEW_THROTTLE_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
struct PlaylistColumnPreviewThrottleState {
    last_sent_at: Option<Instant>,
    last_column_key: Option<String>,
}

fn setup_app_state_associations(ui: &AppWindow, ui_state: &UiState) {
    ui.set_track_model(ModelRc::from(ui_state.track_model.clone()));
    ui.set_library_model(ModelRc::from(Rc::new(VecModel::from(
        Vec::<LibraryRowData>::new(),
    ))));
    ui.set_settings_library_folders(ModelRc::from(Rc::new(VecModel::from(Vec::<
        slint::SharedString,
    >::new()))));
}

pub(crate) fn flash_read_only_view_indicator(ui_handle: slint::Weak<AppWindow>) {
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

pub(crate) fn spawn_debounced_query_dispatcher(
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
pub(crate) struct OutputSettingsOptions {
    pub(crate) device_names: Vec<String>,
    pub(crate) auto_device_name: String,
    pub(crate) channel_values: Vec<u16>,
    pub(crate) sample_rate_values: Vec<u32>,
    pub(crate) bits_per_sample_values: Vec<u16>,
    pub(crate) verified_sample_rate_values: Vec<u32>,
    pub(crate) verified_sample_rates_summary: String,
    pub(crate) auto_channel_value: u16,
    pub(crate) auto_sample_rate_value: u32,
    pub(crate) auto_bits_per_sample_value: u16,
}

pub(crate) fn resolve_effective_runtime_config(
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
const PLAYLIST_COLUMN_KIND_TEXT: i32 = 0;
const PLAYLIST_COLUMN_KIND_ALBUM_ART: i32 = 1;
const TOOLTIP_HOVER_DELAY_MS: u64 = 650;
const PLAYLIST_IMPORT_CHUNK_SIZE: usize = 512;
const DROP_IMPORT_BATCH_DELAY_MS: u64 = 80;
const COLLECTION_MODE_PLAYLIST: i32 = 0;
const COLLECTION_MODE_LIBRARY: i32 = 1;
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
    shared_state: AppSharedState,
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
            .shared_state
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

    apply_config_update(&context.shared_state, next_config, true);
    let _ = context
        .shared_state
        .bus_sender
        .send(Message::Library(protocol::LibraryMessage::RequestScan));

    added_count
}

pub(crate) fn sanitize_config(config: Config) -> Config {
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
    let sanitized_column_width_overrides = sanitize_layout_column_width_overrides(
        &sanitized_layout.playlist_column_width_overrides,
        &sanitized_playlist_columns,
        ColumnWidthBounds {
            min_px: clamped_album_art_column_min_width_px as i32,
            max_px: clamped_album_art_column_max_width_px as i32,
        },
    );
    sanitized_layout.playlist_album_art_column_min_width_px = clamped_album_art_column_min_width_px;
    sanitized_layout.playlist_album_art_column_max_width_px = clamped_album_art_column_max_width_px;
    sanitized_layout.playlist_columns = sanitized_playlist_columns.clone();
    sanitized_layout.playlist_column_width_overrides = sanitized_column_width_overrides;
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
    let clamped_list_image_max_edge_px = config.library.list_image_max_edge_px.clamp(64, 500);
    let clamped_cover_art_cache_max_size_mb =
        config.library.cover_art_cache_max_size_mb.clamp(64, 16_384);
    let clamped_cover_art_memory_cache_max_size_mb = config
        .library
        .cover_art_memory_cache_max_size_mb
        .clamp(8, 2048);
    let clamped_artist_image_memory_cache_max_size_mb = config
        .library
        .artist_image_memory_cache_max_size_mb
        .clamp(8, 2048);
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
            list_image_max_edge_px: clamped_list_image_max_edge_px,
            cover_art_cache_max_size_mb: clamped_cover_art_cache_max_size_mb,
            cover_art_memory_cache_max_size_mb: clamped_cover_art_memory_cache_max_size_mb,
            artist_image_memory_cache_max_size_mb: clamped_artist_image_memory_cache_max_size_mb,
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

pub(crate) fn apply_config_to_ui(
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

fn initialize_logging() {
    let mut clog = colog::basic_builder();
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        // Respect explicit user overrides completely when RUST_LOG is set.
        clog.parse_filters(&rust_log);
    } else {
        // Default policy: full roqtune diagnostics, warnings/errors from dependencies.
        clog.filter(None, log::LevelFilter::Warn);
        clog.filter(Some("roqtune"), log::LevelFilter::Debug);
    }
    clog.init();
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let current_thread = std::thread::current();
        let thread_name = current_thread.name().unwrap_or("unnamed");
        log::error!("panic in thread '{}': {}", thread_name, panic_info);
    }));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    initialize_logging();
    install_panic_hook();
    app_runtime::AppRuntime::build()?.run()
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
    use crate::config_persistence::load_layout_file;
    use crate::layout::LayoutConfig;
    use crate::output_option_selection::{choose_preferred_u16, choose_preferred_u32};
    use crate::runtime_config::{audio_settings_changed, output_preferences_changed};
    use crate::ui::layout_editor_state::{
        default_button_cluster_actions_by_index, update_or_replace_vec_model,
    };
    use slint::{Model, ModelRc, ModelTracker, VecModel};

    use super::{
        apply_column_order_keys, clamp_width_for_visible_column, collect_audio_files_from_folder,
        default_album_art_column_width_bounds, is_album_art_builtin_column,
        is_supported_audio_file, load_system_layout_template, playlist_column_key_at_visible_index,
        playlist_column_width_bounds, playlist_column_width_bounds_with_album_art,
        quantize_splitter_ratio_to_precision, reorder_visible_playlist_columns,
        resolve_playlist_header_column_from_x, resolve_playlist_header_divider_from_x,
        resolve_playlist_header_gap_from_x, resolve_runtime_config, sanitize_config,
        sanitize_playlist_columns, select_manual_output_option_index_u32,
        select_output_option_index_u16, serialize_config_with_preserved_comments,
        serialize_layout_with_preserved_comments, should_apply_custom_column_delete,
        sidebar_width_from_window, system_layout_template_text, visible_playlist_column_kinds,
        OutputSettingsOptions, RuntimeOutputOverride,
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
                && slint_ui.contains("track-list.visible-width - root.null-column-width"),
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
            menu_ui.contains("close-policy: PopupClosePolicy.close-on-click-outside;"),
            "Column header menu should only auto-close when clicking outside"
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
        assert!(
            !slint_ui.contains(
                "root.toggle_playlist_column(index);\n            column-header-menu.close();"
            ),
            "Column toggle should not force-close the column header menu"
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
    fn test_playlist_header_uses_measured_visible_columns_band_contract() {
        let slint_ui = include_str!("roqtune.slint");
        assert!(
            slint_ui.contains("property <length> columns-band-width:"),
            "Header should define an explicit measured columns-band width"
        );
        assert!(
            slint_ui.contains("track-list.visible-width - root.playlist-fixed-chrome-width"),
            "Measured columns band should derive from visible list width"
        );
        assert!(
            slint_ui.contains("property <int> columns-band-width-px"),
            "Header should expose measured columns-band width in pixels"
        );
        assert!(
            slint_ui.contains("if !(root.layout-region-is-visible(i)")
                && slint_ui
                    .contains("root.layout-region-panel-kind(i) == root.panel_kind_track_list"),
            "Viewport-width emission must be gated to visible track-list panels only"
        );
        assert!(
            !slint_ui.contains("self.width - 16px - 40px - root.null-column-width"),
            "Legacy formula-based header width subtraction should be removed"
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
