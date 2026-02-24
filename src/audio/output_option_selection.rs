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
