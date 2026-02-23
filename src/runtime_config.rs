use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::{
    config::{CastConfig, Config, OutputConfig},
    protocol::{ConfigDeltaEntry, ConfigMessage, Message},
};

#[derive(Debug, Clone, Default)]
pub struct RuntimeOutputOverride {
    pub sample_rate_hz: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct RuntimeAudioState {
    pub output: OutputConfig,
    pub cast: CastConfig,
}

#[derive(Debug, Clone)]
pub struct StagedAudioSettings {
    pub output: OutputConfig,
    pub cast: CastConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRuntimeSignature {
    output_device_name: String,
    channel_count: u16,
    sample_rate_hz: u32,
    bits_per_sample: u16,
}

impl OutputRuntimeSignature {
    pub fn from_output(output: &OutputConfig) -> Self {
        Self {
            output_device_name: output.output_device_name.clone(),
            channel_count: output.channel_count,
            sample_rate_hz: output.sample_rate_khz,
            bits_per_sample: output.bits_per_sample,
        }
    }
}

pub fn runtime_output_override_snapshot(
    runtime_output_override: &Arc<Mutex<RuntimeOutputOverride>>,
) -> RuntimeOutputOverride {
    let state = runtime_output_override
        .lock()
        .expect("runtime output override lock poisoned");
    state.clone()
}

pub fn output_preferences_changed(previous: &OutputConfig, next: &OutputConfig) -> bool {
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

pub fn audio_settings_changed(previous: &Config, next: &Config) -> bool {
    output_preferences_changed(&previous.output, &next.output)
        || previous.cast.allow_transcode_fallback != next.cast.allow_transcode_fallback
}

pub fn config_delta_entries(previous: &Config, next: &Config) -> Vec<ConfigDeltaEntry> {
    let mut deltas = Vec::new();
    if previous.output != next.output {
        deltas.push(ConfigDeltaEntry::Output(next.output.clone()));
    }
    if previous.cast != next.cast {
        deltas.push(ConfigDeltaEntry::Cast(next.cast.clone()));
    }
    if previous.ui != next.ui {
        deltas.push(ConfigDeltaEntry::Ui(next.ui.clone()));
    }
    if previous.library != next.library {
        deltas.push(ConfigDeltaEntry::Library(next.library.clone()));
    }
    if previous.buffering != next.buffering {
        deltas.push(ConfigDeltaEntry::Buffering(next.buffering.clone()));
    }
    if previous.integrations != next.integrations {
        deltas.push(ConfigDeltaEntry::Integrations(next.integrations.clone()));
    }
    deltas
}

pub fn publish_runtime_config_delta(
    bus_sender: &broadcast::Sender<Message>,
    last_runtime_config: &Arc<Mutex<Config>>,
    next_runtime_config: Config,
) -> bool {
    let deltas = {
        let mut previous_runtime = last_runtime_config
            .lock()
            .expect("last runtime config lock poisoned");
        let deltas = config_delta_entries(&previous_runtime, &next_runtime_config);
        *previous_runtime = next_runtime_config;
        deltas
    };
    if deltas.is_empty() {
        return false;
    }
    let _ = bus_sender.send(Message::Config(ConfigMessage::ConfigChanged(deltas)));
    true
}

pub fn update_last_runtime_config_snapshot(
    last_runtime_config: &Arc<Mutex<Config>>,
    runtime_config: Config,
) {
    let mut previous_runtime = last_runtime_config
        .lock()
        .expect("last runtime config lock poisoned");
    *previous_runtime = runtime_config;
}

pub fn config_diff_is_runtime_sample_rate_only(previous: &Config, next: &Config) -> bool {
    if previous.cast != next.cast
        || previous.ui != next.ui
        || previous.library != next.library
        || previous.buffering != next.buffering
        || previous.integrations != next.integrations
    {
        return false;
    }
    if previous.output.sample_rate_khz == next.output.sample_rate_khz {
        return false;
    }
    let mut previous_output_without_rate = previous.output.clone();
    let mut next_output_without_rate = next.output.clone();
    previous_output_without_rate.sample_rate_khz = 0;
    next_output_without_rate.sample_rate_khz = 0;
    previous_output_without_rate == next_output_without_rate
}
