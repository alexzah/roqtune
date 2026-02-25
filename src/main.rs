//! roqtune binary entrypoint and top-level orchestration glue.

mod app_bootstrap;
mod app_callbacks;
mod app_config_coordinator;
mod app_context;
mod app_runtime;
mod audio;
mod backends;
mod cast;
mod config;
mod config_persistence;
mod db_manager;
mod image_pipeline;
mod integration;
mod layout;
mod library;
mod media_controls_manager;
mod media_file_discovery;
mod metadata;
#[path = "playlist/playlist.rs"]
mod playlist;
#[path = "playlist/playlist_manager.rs"]
mod playlist_manager;
mod protocol;
mod protocol_utils;
mod runtime;
mod runtime_config;
mod theme;
mod ui;
mod ui_manager;

pub(crate) use audio::{audio_decoder, audio_player, audio_probe, output_option_selection};
pub(crate) use cast::cast_manager;
pub(crate) use integration::{
    integration_keyring, integration_manager, integration_uri, opensubsonic_controller,
};
pub(crate) use library::{library_enrichment_manager, library_manager};
pub(crate) use metadata::{metadata_manager, metadata_tags};
pub(crate) use runtime::audio_runtime_reactor;

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
use slint::{ComponentHandle, ModelRc, VecModel};
use theme::{
    custom_color_component_labels, custom_color_values_for_ui, parse_slint_color, resolve_theme,
    scheme_index_for_id, scheme_picker_options,
};
use tokio::sync::broadcast;
// Re-export shared UI helpers at crate root for callback modules that call through `crate::...`.
pub(crate) use ui::layout_editor_state::*;
pub(crate) use ui::playlist_columns::*;
use ui_manager::UiState;

#[allow(missing_docs)]
mod slint_generated {
    slint::include_modules!();
}
pub(crate) use slint_generated::*;

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

/// Briefly toggles the playlist read-only visual indicator for blocked edits.
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

/// Spawns a background debouncer that emits only the latest queued query after inactivity.
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
    /// Enumerated output device names shown to the user.
    pub(crate) device_names: Vec<String>,
    /// Auto-selected output device name.
    pub(crate) auto_device_name: String,
    /// Supported channel-count options.
    pub(crate) channel_values: Vec<u16>,
    /// Supported sample-rate options.
    pub(crate) sample_rate_values: Vec<u32>,
    /// Supported bit-depth options.
    pub(crate) bits_per_sample_values: Vec<u16>,
    /// Sample rates verified by probing the selected device.
    pub(crate) verified_sample_rate_values: Vec<u32>,
    /// Human-readable summary of verified sample rates.
    pub(crate) verified_sample_rates_summary: String,
    /// Auto-selected channel-count value.
    pub(crate) auto_channel_value: u16,
    /// Auto-selected sample-rate value.
    pub(crate) auto_sample_rate_value: u32,
    /// Auto-selected bit-depth value.
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
const PLAYLIST_COLUMN_KIND_FAVORITE: i32 = 2;
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

/// Sanitizes loaded config values and normalizes derived fields into safe runtime ranges.
pub(crate) fn sanitize_config(config: Config) -> Config {
    let sanitized_playlist_columns = sanitize_playlist_columns(&config.ui.playlist_columns);
    let output_device_name = if config.output.output_device_auto {
        "default".to_string()
    } else {
        config.output.output_device_name.trim().to_string()
    };
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
    let clamped_image_memory_cache_ttl_secs =
        config.library.image_memory_cache_ttl_secs.clamp(1, 3600);
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
            legacy_dark_mode: config.ui.legacy_dark_mode,
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
            image_memory_cache_ttl_secs: clamped_image_memory_cache_ttl_secs,
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

/// Applies a config snapshot to Slint UI properties and models.
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
    let resolved_theme = resolve_theme(&config.ui.layout);
    let parse_theme_color = |value: &str| {
        parse_slint_color(value).unwrap_or_else(|| slint::Color::from_rgb_u8(0, 0, 0))
    };
    let app_palette = ui.global::<AppPalette>();
    app_palette.set_color_scheme(resolved_theme.mode.to_slint_color_scheme());
    app_palette.set_window_bg(parse_theme_color(&resolved_theme.colors.window_bg));
    app_palette.set_panel_bg(parse_theme_color(&resolved_theme.colors.panel_bg));
    app_palette.set_panel_bg_alt(parse_theme_color(&resolved_theme.colors.panel_bg_alt));
    app_palette.set_panel_bg_elevated(parse_theme_color(&resolved_theme.colors.panel_bg_elevated));
    app_palette.set_overlay_scrim(parse_theme_color(&resolved_theme.colors.overlay_scrim));
    app_palette.set_border(parse_theme_color(&resolved_theme.colors.border));
    app_palette.set_separator(parse_theme_color(&resolved_theme.colors.separator));
    app_palette.set_text_primary(parse_theme_color(&resolved_theme.colors.text_primary));
    app_palette.set_text_secondary(parse_theme_color(&resolved_theme.colors.text_secondary));
    app_palette.set_text_muted(parse_theme_color(&resolved_theme.colors.text_muted));
    app_palette.set_text_disabled(parse_theme_color(&resolved_theme.colors.text_disabled));
    app_palette.set_accent(parse_theme_color(&resolved_theme.colors.accent));
    app_palette.set_accent_on(parse_theme_color(&resolved_theme.colors.accent_on));
    app_palette.set_accent_soft_bg(parse_theme_color(&resolved_theme.colors.accent_soft_bg));
    app_palette
        .set_accent_soft_border(parse_theme_color(&resolved_theme.colors.accent_soft_border));
    app_palette.set_control_hover_bg(parse_theme_color(&resolved_theme.colors.control_hover_bg));
    app_palette
        .set_control_pressed_bg(parse_theme_color(&resolved_theme.colors.control_pressed_bg));
    app_palette.set_selection_bg(parse_theme_color(&resolved_theme.colors.selection_bg));
    app_palette.set_selection_border(parse_theme_color(&resolved_theme.colors.selection_border));
    app_palette.set_tooltip_bg(parse_theme_color(&resolved_theme.colors.tooltip_bg));
    app_palette.set_tooltip_border(parse_theme_color(&resolved_theme.colors.tooltip_border));
    app_palette.set_tooltip_text(parse_theme_color(&resolved_theme.colors.tooltip_text));
    app_palette.set_warning(parse_theme_color(&resolved_theme.colors.warning));
    app_palette.set_danger(parse_theme_color(&resolved_theme.colors.danger));
    app_palette.set_success(parse_theme_color(&resolved_theme.colors.success));
    app_palette.set_focus_ring(parse_theme_color(&resolved_theme.colors.focus_ring));
    ui.set_ui_dark_mode(matches!(resolved_theme.mode, theme::ThemeSchemeMode::Dark));
    let scheme_options: Vec<slint::SharedString> = scheme_picker_options()
        .iter()
        .map(|(_, label)| label.as_str().into())
        .collect();
    ui.set_settings_color_scheme_options(ModelRc::from(Rc::new(VecModel::from(scheme_options))));
    ui.set_settings_color_scheme_index(scheme_index_for_id(&config.ui.layout.color_scheme));
    let custom_color_labels: Vec<slint::SharedString> = custom_color_component_labels()
        .into_iter()
        .map(|label| label.into())
        .collect();
    ui.set_settings_custom_color_labels(ModelRc::from(Rc::new(VecModel::from(
        custom_color_labels,
    ))));
    let custom_color_values: Vec<slint::SharedString> =
        custom_color_values_for_ui(&resolved_theme.colors)
            .into_iter()
            .map(|value| value.into())
            .collect();
    ui.set_settings_custom_color_values(ModelRc::from(Rc::new(VecModel::from(
        custom_color_values.clone(),
    ))));
    ui.set_settings_custom_color_draft_values(ModelRc::from(Rc::new(VecModel::from(
        custom_color_values,
    ))));
    ui.set_show_custom_color_scheme_dialog(false);
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
