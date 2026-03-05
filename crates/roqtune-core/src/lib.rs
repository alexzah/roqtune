//! Core (non-GUI) modules for roqtune.
#![allow(dead_code)]

/// Detected and filtered output-setting choices presented in the settings dialog.
#[derive(Debug, Clone)]
pub struct OutputSettingsOptions {
    /// Enumerated output device names shown to the user.
    pub device_names: Vec<String>,
    /// Auto-selected output device name.
    pub auto_device_name: String,
    /// Supported channel-count options.
    pub channel_values: Vec<u16>,
    /// Supported sample-rate options.
    pub sample_rate_values: Vec<u32>,
    /// Supported bit-depth options.
    pub bits_per_sample_values: Vec<u16>,
    /// Sample rates verified by probing the selected device.
    pub verified_sample_rate_values: Vec<u32>,
    /// Human-readable summary of verified sample rates.
    pub verified_sample_rates_summary: String,
    /// Auto-selected channel-count value.
    pub auto_channel_value: u16,
    /// Auto-selected sample-rate value.
    pub auto_sample_rate_value: u32,
    /// Auto-selected bit-depth value.
    pub auto_bits_per_sample_value: u16,
}

pub mod audio;
pub mod backends;
pub mod cast;
pub mod config;
pub mod config_persistence;
pub mod conversion_config;
pub mod db_manager;
pub mod file_operations;
pub mod image_pipeline;
pub mod integration;
pub mod layout;
pub mod library;
pub mod media_controls_manager;
pub mod media_file_discovery;
pub mod metadata;
pub mod playlist;
pub mod playlist_manager;
pub mod protocol;
pub mod protocol_utils;
pub mod runtime_config;
pub mod text_template;

pub use audio::{audio_decoder, audio_player, audio_probe, output_option_selection};
pub use cast::cast_manager;
pub use file_operations::BatchFileOperationManager;
pub use integration::{
    integration_keyring, integration_manager, integration_uri, opensubsonic_controller,
};
pub use library::{library_enrichment_manager, library_manager};
pub use metadata::{metadata_manager, metadata_tags};

/// Sanitizes config values needed by core-only tests and utilities.
pub fn sanitize_config(mut config: config::Config) -> config::Config {
    let mut min_width = config
        .ui
        .playlist_album_art_column_min_width_px
        .clamp(12, 512);
    let mut max_width = config
        .ui
        .playlist_album_art_column_max_width_px
        .clamp(24, 1024);
    if min_width > max_width {
        std::mem::swap(&mut min_width, &mut max_width);
    }
    config.ui.playlist_album_art_column_min_width_px = min_width;
    config.ui.playlist_album_art_column_max_width_px = max_width;
    config
}
