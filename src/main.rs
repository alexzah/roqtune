mod audio_decoder;
mod audio_player;
mod config;
mod db_manager;
mod playlist;
mod playlist_manager;
mod protocol;
mod ui_manager;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
};

use audio_decoder::AudioDecoder;
use audio_player::AudioPlayer;
use config::{BufferingConfig, Config, OutputConfig, PlaylistColumnConfig, UiConfig};
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
    device_names: Vec<String>,
    auto_device_name: String,
    channel_values: Vec<u16>,
    sample_rate_values: Vec<u32>,
    bits_per_sample_values: Vec<u16>,
    auto_channel_value: u16,
    auto_sample_rate_value: u32,
    auto_bits_per_sample_value: u16,
}

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

fn resolve_runtime_config(config: &Config, output_options: &OutputSettingsOptions) -> Config {
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

fn select_output_option_index_u32(
    is_auto: bool,
    value: u32,
    values: &[u32],
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

fn detect_output_settings_options(config: &Config) -> OutputSettingsOptions {
    const COMMON_SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
    const COMMON_CHANNEL_COUNTS: [u16; 4] = [2, 1, 6, 8];
    const COMMON_BIT_DEPTHS: [u16; 3] = [16, 24, 32];
    const PREFERRED_AUTO_SAMPLE_RATES: [u32; 6] =
        [44_100, 48_000, 96_000, 88_200, 192_000, 176_400];
    const PREFERRED_AUTO_CHANNEL_COUNTS: [u16; 4] = [2, 6, 1, 8];
    const PREFERRED_AUTO_BIT_DEPTHS: [u16; 3] = [24, 32, 16];
    const PROBE_SAMPLE_RATES: [u32; 13] = [
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000,
        176_400, 192_000,
    ];

    let mut channels = BTreeSet::new();
    let mut sample_rates = BTreeSet::new();
    let mut bits_per_sample = BTreeSet::new();
    let mut device_names = Vec::new();
    let mut auto_device_name = String::new();

    let host = cpal::default_host();
    if let Some(default_device) = host.default_output_device() {
        auto_device_name = default_device
            .name()
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_default();
    }
    if let Ok(devices) = host.output_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                if !name.is_empty() {
                    device_names.push(name);
                }
            }
        }
    }
    if !config.output.output_device_name.trim().is_empty() {
        device_names.push(config.output.output_device_name.trim().to_string());
    }
    device_names.sort();
    device_names.dedup();

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
    let auto_channel_value = choose_preferred_u16(
        &channels,
        &PREFERRED_AUTO_CHANNEL_COUNTS,
        config.output.channel_count.max(1),
    );
    let auto_sample_rate_value = choose_preferred_u32(
        &sample_rates,
        &PREFERRED_AUTO_SAMPLE_RATES,
        config.output.sample_rate_khz.max(8_000),
    );
    let auto_bits_per_sample_value = choose_preferred_u16(
        &bits_per_sample,
        &PREFERRED_AUTO_BIT_DEPTHS,
        config.output.bits_per_sample.max(8),
    );

    OutputSettingsOptions {
        device_names,
        auto_device_name,
        channel_values,
        sample_rate_values,
        bits_per_sample_values,
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
            output_device_name,
            output_device_auto: config.output.output_device_auto,
            channel_count: clamped_channels,
            sample_rate_khz: clamped_sample_rate_hz,
            bits_per_sample: clamped_bits,
            channel_count_auto: config.output.channel_count_auto,
            sample_rate_auto: config.output.sample_rate_auto,
            bits_per_sample_auto: config.output.bits_per_sample_auto,
        },
        ui: UiConfig {
            show_album_art: config.ui.show_album_art,
            playlist_columns: sanitized_playlist_columns,
        },
        buffering: BufferingConfig {
            player_low_watermark_ms: clamped_low_watermark,
            player_target_buffer_ms: clamped_target,
            player_request_interval_ms: clamped_interval,
            decoder_request_chunk_ms: clamped_decoder_chunk,
        },
    }
}

fn normalize_column_format(format: &str) -> String {
    format.trim().to_ascii_lowercase()
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

    ui.set_playlist_visible_column_headers(ModelRc::from(Rc::new(VecModel::from(visible_headers))));
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

fn apply_config_to_ui(ui: &AppWindow, config: &Config, output_options: &OutputSettingsOptions) {
    ui.set_show_album_art(config.ui.show_album_art);

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

    let mut sample_rate_options_with_other: Vec<slint::SharedString> =
        vec![format!("Auto ({} Hz)", output_options.auto_sample_rate_value).into()];
    sample_rate_options_with_other.extend(
        output_options
            .sample_rate_values
            .iter()
            .map(|value| format!("{} Hz", value).into()),
    );
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

    let device_custom_index = output_options.device_names.len() + 1;
    let channel_custom_index = output_options.channel_values.len() + 1;
    let sample_rate_custom_index = output_options.sample_rate_values.len() + 1;
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
    let sample_rate_index = select_output_option_index_u32(
        config.output.sample_rate_auto,
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

    ui.set_settings_output_device_index(device_index as i32);
    ui.set_settings_channel_index(channel_index as i32);
    ui.set_settings_sample_rate_index(sample_rate_index as i32);
    ui.set_settings_bits_per_sample_index(bits_index as i32);
    ui.set_settings_output_device_custom_value(config.output.output_device_name.to_string().into());
    ui.set_settings_channel_custom_value(config.output.channel_count.to_string().into());
    ui.set_settings_sample_rate_custom_value(config.output.sample_rate_khz.to_string().into());
    ui.set_settings_bits_per_sample_custom_value(config.output.bits_per_sample.to_string().into());
    ui.set_settings_show_album_art(config.ui.show_album_art);
    apply_playlist_columns_to_ui(ui, config);
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

    if let Err(err) = std::fs::create_dir_all(&config_dir) {
        return Err(format!(
            "Failed to create config directory {}: {}",
            config_dir.display(),
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

    let config_content = std::fs::read_to_string(config_file.clone()).unwrap();
    let config = sanitize_config(toml::from_str::<Config>(&config_content).unwrap_or_default());
    let initial_output_options = detect_output_settings_options(&config);
    let runtime_config = resolve_runtime_config(&config, &initial_output_options);

    setup_app_state_associations(&ui, &ui_state);
    apply_config_to_ui(&ui, &config, &initial_output_options);
    let config_state = Arc::new(Mutex::new(config.clone()));
    let output_options = Arc::new(Mutex::new(initial_output_options.clone()));
    let last_runtime_signature = Arc::new(Mutex::new(OutputRuntimeSignature::from_output(
        &runtime_config.output,
    )));

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

    let bus_sender_clone = bus_sender.clone();
    ui.on_playlist_columns_viewport_resized(move |width_px| {
        let clamped_width = width_px.max(0) as u32;
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::PlaylistViewportWidthChanged(clamped_width),
        ));
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
        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        if let Some(ui) = ui_handle_clone.upgrade() {
            apply_config_to_ui(&ui, &current_config, &options_snapshot);
            ui.set_show_settings_dialog(true);
        }
    });

    // Wire up settings apply handler
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    let config_file_clone = config_file.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_apply_settings(
        move |output_device_index,
              channel_index,
              sample_rate_index,
              bits_per_sample_index,
              show_album_art,
              output_device_custom_value,
              channel_custom_value,
              sample_rate_custom_value,
              bits_custom_value| {
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
            let sample_rate_auto = sample_rate_idx == 0;
            let sample_rate_hz = if sample_rate_auto {
                options_snapshot.auto_sample_rate_value
            } else if sample_rate_idx <= options_snapshot.sample_rate_values.len() {
                options_snapshot.sample_rate_values[sample_rate_idx - 1]
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
                },
                ui: UiConfig {
                    show_album_art,
                    playlist_columns: previous_config.ui.playlist_columns.clone(),
                },
                buffering: previous_config.buffering.clone(),
            });

            if let Some(ui) = ui_handle_clone.upgrade() {
                apply_config_to_ui(&ui, &next_config, &options_snapshot);
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

            let runtime_config = resolve_runtime_config(&next_config, &options_snapshot);
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
            ui: UiConfig {
                show_album_art: previous_config.ui.show_album_art,
                playlist_columns: updated_columns,
            },
            buffering: previous_config.buffering.clone(),
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

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_config = resolve_runtime_config(&next_config, &options_snapshot);
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
            ui: UiConfig {
                show_album_art: previous_config.ui.show_album_art,
                playlist_columns: updated_columns,
            },
            buffering: previous_config.buffering.clone(),
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

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_config = resolve_runtime_config(&next_config, &options_snapshot);
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
            ui: UiConfig {
                show_album_art: previous_config.ui.show_album_art,
                playlist_columns: updated_columns,
            },
            buffering: previous_config.buffering.clone(),
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

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_config = resolve_runtime_config(&next_config, &options_snapshot);
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
            ui: UiConfig {
                show_album_art: previous_config.ui.show_album_art,
                playlist_columns: updated_columns,
            },
            buffering: previous_config.buffering.clone(),
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

        let options_snapshot = {
            let options = output_options_clone
                .lock()
                .expect("output options lock poisoned");
            options.clone()
        };
        let runtime_config = resolve_runtime_config(&next_config, &options_snapshot);
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

    // Re-detect output options on device-open notifications and refresh auto runtime output.
    let mut device_event_receiver = bus_sender.subscribe();
    let bus_sender_clone = bus_sender.clone();
    let config_state_clone = Arc::clone(&config_state);
    let output_options_clone = Arc::clone(&output_options);
    let ui_handle_clone = ui.as_weak().clone();
    let last_runtime_signature_clone = Arc::clone(&last_runtime_signature);
    thread::spawn(move || loop {
        match device_event_receiver.blocking_recv() {
            Ok(Message::Config(ConfigMessage::AudioDeviceOpened { .. })) => {
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

                let runtime = resolve_runtime_config(&persisted_config, &detected_options);
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
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        apply_config_to_ui(&ui, &config_for_ui, &options_for_ui);
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
                        ui: UiConfig {
                            show_album_art: state.ui.show_album_art,
                            playlist_columns: reordered_columns,
                        },
                        buffering: state.buffering.clone(),
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

                let runtime = resolve_runtime_config(&next_config, &options_snapshot);
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
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        apply_config_to_ui(&ui, &config_for_ui, &options_for_ui);
                    }
                });
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    });

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Playlist(
        PlaylistMessage::RequestActivePlaylistColumnOrder,
    ));

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(
        runtime_config,
    )));

    ui.run()?;

    info!("Application exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::config::{BufferingConfig, Config, OutputConfig, PlaylistColumnConfig, UiConfig};

    use super::{
        apply_column_order_keys, choose_preferred_u16, choose_preferred_u32,
        reorder_visible_playlist_columns, resolve_runtime_config, sanitize_playlist_columns,
        select_output_option_index_u16, select_output_option_index_u32,
        should_apply_custom_column_delete, OutputSettingsOptions,
    };

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

    #[test]
    fn test_custom_column_menu_supports_delete_with_confirmation() {
        let slint_ui = include_str!("music_player.slint");

        assert!(
            slint_ui.contains("in property <[bool]> custom: [];"),
            "Column header menu should receive custom-column flags"
        );
        assert!(
            slint_ui.contains("callback delete-column(int);"),
            "Column header menu should expose delete callback"
        );
        assert!(
            slint_ui.contains("background: delete-column-ta.has-hover ? #db3f3f : #b93636;"),
            "Custom columns should render a red delete icon"
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
        let slint_ui = include_str!("music_player.slint");
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
            select_output_option_index_u32(false, 48_000, &[44_100, 48_000], 3),
            2
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
            },
            ui: UiConfig {
                show_album_art: true,
                playlist_columns: crate::config::default_playlist_columns(),
            },
            buffering: BufferingConfig::default(),
        };
        let options = OutputSettingsOptions {
            device_names: vec!["Device A".to_string(), "Device B".to_string()],
            auto_device_name: "Device B".to_string(),
            channel_values: vec![1, 2, 6],
            sample_rate_values: vec![44_100, 48_000],
            bits_per_sample_values: vec![16, 24],
            auto_channel_value: 2,
            auto_sample_rate_value: 44_100,
            auto_bits_per_sample_value: 24,
        };

        let runtime = resolve_runtime_config(&persisted, &options);
        assert_eq!(runtime.output.channel_count, 2);
        assert_eq!(runtime.output.sample_rate_khz, 48_000);
        assert_eq!(runtime.output.bits_per_sample, 24);
        assert_eq!(runtime.output.output_device_name, "Device B");
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
        assert!(sanitized.iter().any(|column| column == &custom));
        assert!(sanitized.iter().any(|column| column.enabled));
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
