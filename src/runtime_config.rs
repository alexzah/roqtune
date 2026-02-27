//! Runtime audio-config diffing, snapshots, and bus-delta publication helpers.

use std::sync::{Arc, Mutex};

use log::debug;
use tokio::sync::broadcast;

use crate::{
    config::{CastConfig, Config, OutputConfig},
    protocol::{
        BufferingConfigDelta, CastConfigDelta, ConfigDeltaEntry, ConfigMessage,
        IntegrationsConfigDelta, LibraryConfigDelta, Message, OutputConfigDelta, UiConfigDelta,
    },
};

#[derive(Debug, Clone, Default)]
/// Runtime-only output overrides that should not be persisted to disk.
pub struct RuntimeOutputOverride {
    /// Optional runtime sample-rate override in Hz.
    pub sample_rate_hz: Option<u32>,
}

#[derive(Debug, Clone)]
/// Last audio settings acknowledged by runtime workers.
pub struct RuntimeAudioState {
    /// Effective output settings currently in use.
    pub output: OutputConfig,
    /// Effective cast settings currently in use.
    pub cast: CastConfig,
}

#[derive(Debug, Clone)]
/// Pending audio settings waiting for runtime apply/ack.
pub struct StagedAudioSettings {
    /// Pending output settings.
    pub output: OutputConfig,
    /// Pending cast settings.
    pub cast: CastConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Signature of runtime output values used for fast change detection.
pub struct OutputRuntimeSignature {
    output_device_name: String,
    channel_count: u16,
    sample_rate_hz: u32,
    bits_per_sample: u16,
}

impl OutputRuntimeSignature {
    /// Builds a signature from an `OutputConfig` snapshot.
    pub fn from_output(output: &OutputConfig) -> Self {
        Self {
            output_device_name: output.output_device_name.clone(),
            channel_count: output.channel_count,
            sample_rate_hz: output.sample_rate_khz,
            bits_per_sample: output.bits_per_sample,
        }
    }
}

/// Clones the current runtime output override from shared state.
pub fn runtime_output_override_snapshot(
    runtime_output_override: &Arc<Mutex<RuntimeOutputOverride>>,
) -> RuntimeOutputOverride {
    let state = runtime_output_override
        .lock()
        .expect("runtime output override lock poisoned");
    state.clone()
}

/// Returns `true` when runtime-relevant output preferences changed.
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

/// Returns `true` when runtime-relevant audio settings changed.
pub fn audio_settings_changed(previous: &Config, next: &Config) -> bool {
    output_preferences_changed(&previous.output, &next.output)
        || previous.cast.allow_transcode_fallback != next.cast.allow_transcode_fallback
}

/// Computes config delta entries between two config snapshots.
pub fn config_delta_entries(previous: &Config, next: &Config) -> Vec<ConfigDeltaEntry> {
    let mut deltas = Vec::new();
    let mut output = OutputConfigDelta::default();
    if previous.output.output_device_name != next.output.output_device_name {
        output.output_device_name = Some(next.output.output_device_name.clone());
    }
    if previous.output.output_device_auto != next.output.output_device_auto {
        output.output_device_auto = Some(next.output.output_device_auto);
    }
    if previous.output.channel_count != next.output.channel_count {
        output.channel_count = Some(next.output.channel_count);
    }
    if previous.output.sample_rate_khz != next.output.sample_rate_khz {
        output.sample_rate_khz = Some(next.output.sample_rate_khz);
    }
    if previous.output.bits_per_sample != next.output.bits_per_sample {
        output.bits_per_sample = Some(next.output.bits_per_sample);
    }
    if previous.output.channel_count_auto != next.output.channel_count_auto {
        output.channel_count_auto = Some(next.output.channel_count_auto);
    }
    if previous.output.sample_rate_auto != next.output.sample_rate_auto {
        output.sample_rate_auto = Some(next.output.sample_rate_auto);
    }
    if previous.output.bits_per_sample_auto != next.output.bits_per_sample_auto {
        output.bits_per_sample_auto = Some(next.output.bits_per_sample_auto);
    }
    if previous.output.resampler_quality != next.output.resampler_quality {
        output.resampler_quality = Some(next.output.resampler_quality);
    }
    if previous.output.dither_on_bitdepth_reduce != next.output.dither_on_bitdepth_reduce {
        output.dither_on_bitdepth_reduce = Some(next.output.dither_on_bitdepth_reduce);
    }
    if previous.output.downmix_higher_channel_tracks != next.output.downmix_higher_channel_tracks {
        output.downmix_higher_channel_tracks = Some(next.output.downmix_higher_channel_tracks);
    }
    if !output.is_empty() {
        deltas.push(ConfigDeltaEntry::Output(output));
    }

    let mut cast = CastConfigDelta::default();
    if previous.cast.allow_transcode_fallback != next.cast.allow_transcode_fallback {
        cast.allow_transcode_fallback = Some(next.cast.allow_transcode_fallback);
    }
    if !cast.is_empty() {
        deltas.push(ConfigDeltaEntry::Cast(cast));
    }

    let mut ui = UiConfigDelta::default();
    if previous.ui.show_layout_edit_intro != next.ui.show_layout_edit_intro {
        ui.show_layout_edit_intro = Some(next.ui.show_layout_edit_intro);
    }
    if previous.ui.show_tooltips != next.ui.show_tooltips {
        ui.show_tooltips = Some(next.ui.show_tooltips);
    }
    if previous.ui.auto_scroll_to_playing_track != next.ui.auto_scroll_to_playing_track {
        ui.auto_scroll_to_playing_track = Some(next.ui.auto_scroll_to_playing_track);
    }
    if previous.ui.playlist_album_art_column_min_width_px
        != next.ui.playlist_album_art_column_min_width_px
    {
        ui.playlist_album_art_column_min_width_px =
            Some(next.ui.playlist_album_art_column_min_width_px);
    }
    if previous.ui.playlist_album_art_column_max_width_px
        != next.ui.playlist_album_art_column_max_width_px
    {
        ui.playlist_album_art_column_max_width_px =
            Some(next.ui.playlist_album_art_column_max_width_px);
    }
    if previous.ui.layout != next.ui.layout {
        ui.layout = Some(next.ui.layout.clone());
    }
    if previous.ui.playlist_columns != next.ui.playlist_columns {
        ui.playlist_columns = Some(next.ui.playlist_columns.clone());
    }
    if previous.ui.window_width != next.ui.window_width {
        ui.window_width = Some(next.ui.window_width);
    }
    if previous.ui.window_height != next.ui.window_height {
        ui.window_height = Some(next.ui.window_height);
    }
    if previous.ui.volume != next.ui.volume {
        ui.volume = Some(next.ui.volume);
    }
    if previous.ui.playback_order != next.ui.playback_order {
        ui.playback_order = Some(next.ui.playback_order);
    }
    if previous.ui.repeat_mode != next.ui.repeat_mode {
        ui.repeat_mode = Some(next.ui.repeat_mode);
    }
    if !ui.is_empty() {
        deltas.push(ConfigDeltaEntry::Ui(ui));
    }

    let mut library = LibraryConfigDelta::default();
    if previous.library.folders != next.library.folders {
        library.folders = Some(next.library.folders.clone());
    }
    if previous.library.online_metadata_enabled != next.library.online_metadata_enabled {
        library.online_metadata_enabled = Some(next.library.online_metadata_enabled);
    }
    if previous.library.online_metadata_prompt_pending
        != next.library.online_metadata_prompt_pending
    {
        library.online_metadata_prompt_pending = Some(next.library.online_metadata_prompt_pending);
    }
    if previous.library.include_playlist_tracks_in_library
        != next.library.include_playlist_tracks_in_library
    {
        library.include_playlist_tracks_in_library =
            Some(next.library.include_playlist_tracks_in_library);
    }
    if previous.library.list_image_max_edge_px != next.library.list_image_max_edge_px {
        library.list_image_max_edge_px = Some(next.library.list_image_max_edge_px);
    }
    if previous.library.cover_art_cache_max_size_mb != next.library.cover_art_cache_max_size_mb {
        library.cover_art_cache_max_size_mb = Some(next.library.cover_art_cache_max_size_mb);
    }
    if previous.library.cover_art_memory_cache_max_size_mb
        != next.library.cover_art_memory_cache_max_size_mb
    {
        library.cover_art_memory_cache_max_size_mb =
            Some(next.library.cover_art_memory_cache_max_size_mb);
    }
    if previous.library.artist_image_memory_cache_max_size_mb
        != next.library.artist_image_memory_cache_max_size_mb
    {
        library.artist_image_memory_cache_max_size_mb =
            Some(next.library.artist_image_memory_cache_max_size_mb);
    }
    if previous.library.image_memory_cache_ttl_secs != next.library.image_memory_cache_ttl_secs {
        library.image_memory_cache_ttl_secs = Some(next.library.image_memory_cache_ttl_secs);
    }
    if previous.library.artist_image_cache_ttl_days != next.library.artist_image_cache_ttl_days {
        library.artist_image_cache_ttl_days = Some(next.library.artist_image_cache_ttl_days);
    }
    if previous.library.artist_image_cache_max_size_mb
        != next.library.artist_image_cache_max_size_mb
    {
        library.artist_image_cache_max_size_mb = Some(next.library.artist_image_cache_max_size_mb);
    }
    if !library.is_empty() {
        deltas.push(ConfigDeltaEntry::Library(library));
    }

    let mut buffering = BufferingConfigDelta::default();
    if previous.buffering.player_low_watermark_ms != next.buffering.player_low_watermark_ms {
        buffering.player_low_watermark_ms = Some(next.buffering.player_low_watermark_ms);
    }
    if previous.buffering.player_target_buffer_ms != next.buffering.player_target_buffer_ms {
        buffering.player_target_buffer_ms = Some(next.buffering.player_target_buffer_ms);
    }
    if previous.buffering.player_request_interval_ms != next.buffering.player_request_interval_ms {
        buffering.player_request_interval_ms = Some(next.buffering.player_request_interval_ms);
    }
    if previous.buffering.decoder_request_chunk_ms != next.buffering.decoder_request_chunk_ms {
        buffering.decoder_request_chunk_ms = Some(next.buffering.decoder_request_chunk_ms);
    }
    if !buffering.is_empty() {
        deltas.push(ConfigDeltaEntry::Buffering(buffering));
    }

    let mut integrations = IntegrationsConfigDelta::default();
    if previous.integrations.backends != next.integrations.backends {
        integrations.backends = Some(next.integrations.backends.clone());
    }
    if !integrations.is_empty() {
        deltas.push(ConfigDeltaEntry::Integrations(integrations));
    }
    deltas
}

/// Publishes a `ConfigChanged` delta message when runtime config differs.
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
    debug!(
        "Publishing ConfigChanged with {} delta entry(ies): {:?}",
        deltas.len(),
        deltas
    );
    let _ = bus_sender.send(Message::Config(ConfigMessage::ConfigChanged(deltas)));
    true
}

/// Replaces the last runtime config snapshot with a new value.
pub fn update_last_runtime_config_snapshot(
    last_runtime_config: &Arc<Mutex<Config>>,
    runtime_config: Config,
) {
    let mut previous_runtime = last_runtime_config
        .lock()
        .expect("last runtime config lock poisoned");
    *previous_runtime = runtime_config;
}

/// Returns `true` when the only effective config change is output sample rate.
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

#[cfg(test)]
mod tests {
    use crate::{config::Config, protocol::ConfigDeltaEntry};

    use super::{audio_settings_changed, config_delta_entries, output_preferences_changed};

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
    fn test_config_delta_entries_include_image_memory_cache_ttl_secs() {
        let previous = Config::default();
        let mut next = previous.clone();
        next.library.image_memory_cache_ttl_secs =
            next.library.image_memory_cache_ttl_secs.saturating_add(1);

        let deltas = config_delta_entries(&previous, &next);
        assert!(
            deltas.iter().any(|delta| matches!(
                delta,
                ConfigDeltaEntry::Library(library)
                    if library.image_memory_cache_ttl_secs
                        == Some(next.library.image_memory_cache_ttl_secs)
            )),
            "expected library delta with image_memory_cache_ttl_secs"
        );
    }
}
