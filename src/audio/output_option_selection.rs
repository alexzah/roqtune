//! Output device/format option selection and effective runtime config helpers.

use std::collections::BTreeSet;

use cpal::traits::{DeviceTrait, HostTrait};

use crate::{
    audio_probe::get_or_probe_output_device, config::Config, runtime_config::RuntimeOutputOverride,
    OutputSettingsOptions,
};

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

/// Picks the first preferred `u16` value present in `detected`, with fallbacks.
pub(crate) fn choose_preferred_u16(
    detected: &BTreeSet<u16>,
    preferred_values: &[u16],
    fallback: u16,
) -> u16 {
    preferred_values
        .iter()
        .copied()
        .find(|value| detected.contains(value))
        .or_else(|| detected.get(&fallback).copied())
        .or_else(|| detected.iter().next().copied())
        .unwrap_or(fallback)
}

/// Picks the first preferred `u32` value present in `detected`, with fallbacks.
pub(crate) fn choose_preferred_u32(
    detected: &BTreeSet<u32>,
    preferred_values: &[u32],
    fallback: u32,
) -> u32 {
    preferred_values
        .iter()
        .copied()
        .find(|value| detected.contains(value))
        .or_else(|| detected.get(&fallback).copied())
        .or_else(|| detected.iter().next().copied())
        .unwrap_or(fallback)
}

/// Resolves effective runtime output values from persisted settings plus auto/override state.
pub(crate) fn resolve_runtime_config(
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

/// Resolves a settings dropdown index for string-valued options with an Auto slot at index `0`.
pub(crate) fn select_output_option_index_string(
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

/// Resolves a settings dropdown index for `u16` options with an Auto slot at index `0`.
pub(crate) fn select_output_option_index_u16(
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

/// Resolves a manual selection index for `u32` options without an Auto slot.
pub(crate) fn select_manual_output_option_index_u32(
    value: u32,
    values: &[u32],
    custom_index: usize,
) -> usize {
    values
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(custom_index)
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

/// Builds a non-blocking startup snapshot from persisted config only.
///
/// This intentionally avoids any CPAL/ALSA device enumeration or probing so the UI can
/// initialize immediately using the user's prior settings. Runtime probing then refreshes
/// these options asynchronously once audio services are up.
pub(crate) fn bootstrap_output_settings_options(config: &Config) -> OutputSettingsOptions {
    let configured_device_name = config.output.output_device_name.trim().to_string();
    let configured_channel_count = config.output.channel_count.max(1);
    let configured_sample_rate_hz = config.output.sample_rate_khz.max(8_000);
    let configured_bits_per_sample = config.output.bits_per_sample.max(8);

    let device_names = if configured_device_name.is_empty() {
        Vec::new()
    } else {
        vec![configured_device_name.clone()]
    };

    OutputSettingsOptions {
        device_names,
        auto_device_name: configured_device_name,
        channel_values: vec![configured_channel_count],
        sample_rate_values: vec![configured_sample_rate_hz],
        bits_per_sample_values: vec![configured_bits_per_sample],
        verified_sample_rate_values: Vec::new(),
        verified_sample_rates_summary: "Verified rates: (pending probe)".to_string(),
        auto_channel_value: configured_channel_count,
        auto_sample_rate_value: configured_sample_rate_hz,
        auto_bits_per_sample_value: configured_bits_per_sample,
    }
}

/// Returns a stable, deduplicated snapshot of output device names from a host.
pub(crate) fn snapshot_output_device_names(host: &cpal::Host) -> Vec<String> {
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

/// Detects available output settings and computes preferred auto defaults.
pub(crate) fn detect_output_settings_options(config: &Config) -> OutputSettingsOptions {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::{
        config::{
            BufferingConfig, Config, LibraryConfig, OutputConfig, UiConfig, UiPlaybackOrder,
            UiRepeatMode,
        },
        runtime_config::RuntimeOutputOverride,
    };

    use super::{
        bootstrap_output_settings_options, choose_preferred_u16, choose_preferred_u32,
        resolve_runtime_config, select_manual_output_option_index_u32,
        select_output_option_index_string, select_output_option_index_u16,
    };

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
        let values = vec!["Device A".to_string(), "Device B".to_string()];
        assert_eq!(
            select_output_option_index_string(true, "Device A", &values, 3),
            0
        );
        assert_eq!(
            select_output_option_index_string(false, "Device B", &values, 3),
            2
        );
        assert_eq!(
            select_output_option_index_string(false, "Missing", &values, 3),
            3
        );

        let rates = vec![44_100_u32, 48_000, 96_000];
        assert_eq!(select_output_option_index_u16(true, 24, &[16, 24], 3), 0);
        assert_eq!(select_manual_output_option_index_u32(96_000, &rates, 3), 2);
        assert_eq!(select_manual_output_option_index_u32(88_200, &rates, 3), 3);
    }

    #[test]
    fn test_resolve_runtime_config_applies_auto_values_only_for_auto_fields() {
        let persisted = Config {
            output: OutputConfig {
                output_device_name: "Manual Device".to_string(),
                output_device_auto: true,
                channel_count: 6,
                sample_rate_khz: 96_000,
                bits_per_sample: 16,
                channel_count_auto: true,
                sample_rate_auto: false,
                bits_per_sample_auto: true,
                ..Config::default().output
            },
            ui: UiConfig {
                show_tooltips: true,
                auto_scroll_to_playing_track: true,
                dark_mode: true,
                show_layout_edit_intro: true,
                playlist_album_art_column_min_width_px: 16,
                playlist_album_art_column_max_width_px: 480,
                layout: crate::layout::LayoutConfig::default(),
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
            cast: crate::config::CastConfig::default(),
        };
        let options = crate::OutputSettingsOptions {
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
        assert_eq!(runtime.output.sample_rate_khz, 96_000);
        assert_eq!(runtime.output.bits_per_sample, 24);
        assert_eq!(runtime.output.output_device_name, "Device B");
    }

    #[test]
    fn test_resolve_runtime_config_applies_runtime_sample_rate_override() {
        let persisted = Config::default();
        let options = crate::OutputSettingsOptions {
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
    fn test_bootstrap_output_settings_options_uses_persisted_values_without_probe() {
        let config = Config {
            output: OutputConfig {
                output_device_name: "default".to_string(),
                output_device_auto: true,
                channel_count: 2,
                sample_rate_khz: 192_000,
                bits_per_sample: 32,
                ..Config::default().output
            },
            ..Config::default()
        };

        let bootstrap = bootstrap_output_settings_options(&config);
        assert_eq!(bootstrap.device_names, vec!["default".to_string()]);
        assert_eq!(bootstrap.auto_device_name, "default".to_string());
        assert_eq!(bootstrap.channel_values, vec![2]);
        assert_eq!(bootstrap.sample_rate_values, vec![192_000]);
        assert_eq!(bootstrap.bits_per_sample_values, vec![32]);
        assert!(bootstrap.verified_sample_rate_values.is_empty());
        assert_eq!(
            bootstrap.verified_sample_rates_summary,
            "Verified rates: (pending probe)".to_string()
        );
    }
}
