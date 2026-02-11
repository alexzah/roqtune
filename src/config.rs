//! Persistent application configuration model and defaults.

use crate::layout::LayoutConfig;

/// Root configuration persisted to `config.toml`.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Config {
    /// Audio output and device preferences.
    pub output: OutputConfig,
    #[serde(default)]
    /// UI preferences.
    pub ui: UiConfig,
    #[serde(default)]
    /// Library indexing and first-start preferences.
    pub library: LibraryConfig,
    #[serde(default)]
    /// Decoder/player buffering behavior.
    pub buffering: BufferingConfig,
}

/// Output device and format preferences.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct OutputConfig {
    #[serde(default)]
    pub output_device_name: String,
    #[serde(default = "default_true")]
    pub output_device_auto: bool,
    pub channel_count: u16,
    pub sample_rate_khz: u32,
    pub bits_per_sample: u16,
    #[serde(default = "default_true")]
    pub channel_count_auto: bool,
    #[serde(default = "default_true")]
    pub sample_rate_auto: bool,
    #[serde(default = "default_true")]
    pub bits_per_sample_auto: bool,
}

/// UI preferences persisted between sessions.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UiConfig {
    #[serde(default = "default_true")]
    pub show_layout_edit_intro: bool,
    #[serde(default = "default_true")]
    pub show_tooltips: bool,
    #[serde(default = "default_playlist_album_art_column_min_width_px")]
    pub playlist_album_art_column_min_width_px: u32,
    #[serde(default = "default_playlist_album_art_column_max_width_px")]
    pub playlist_album_art_column_max_width_px: u32,
    /// Runtime-only layout state loaded from `layout.toml`.
    #[serde(skip)]
    pub layout: LayoutConfig,
    #[serde(default)]
    pub button_cluster_instances: Vec<ButtonClusterInstanceConfig>,
    #[serde(default = "default_playlist_columns")]
    pub playlist_columns: Vec<PlaylistColumnConfig>,
    #[serde(default = "default_window_width")]
    pub window_width: u32,
    #[serde(default = "default_window_height")]
    pub window_height: u32,
    #[serde(default = "default_volume")]
    pub volume: f32,
}

/// Library indexing preferences persisted between sessions.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LibraryConfig {
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default = "default_true")]
    pub show_first_start_prompt: bool,
}

/// Declarative playlist column definition.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct PlaylistColumnConfig {
    /// Header label.
    pub name: String,
    /// Format string used to render each row cell.
    pub format: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub custom: bool,
}

/// Per-leaf button cluster configuration persisted with layout preferences.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ButtonClusterInstanceConfig {
    /// Layout leaf identifier that owns this cluster instance.
    pub leaf_id: String,
    /// Ordered action ids rendered in the cluster.
    pub actions: Vec<i32>,
}

/// Tuning knobs for decode/playback buffering.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BufferingConfig {
    #[serde(default = "default_player_low_watermark_ms")]
    pub player_low_watermark_ms: u32,
    #[serde(default = "default_player_target_buffer_ms")]
    pub player_target_buffer_ms: u32,
    #[serde(default = "default_player_request_interval_ms")]
    pub player_request_interval_ms: u32,
    #[serde(default = "default_decoder_request_chunk_ms")]
    pub decoder_request_chunk_ms: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            output_device_name: String::new(),
            output_device_auto: true,
            channel_count: 2,
            sample_rate_khz: 44100,
            bits_per_sample: 24,
            channel_count_auto: true,
            sample_rate_auto: true,
            bits_per_sample_auto: true,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_layout_edit_intro: true,
            show_tooltips: true,
            playlist_album_art_column_min_width_px: default_playlist_album_art_column_min_width_px(
            ),
            playlist_album_art_column_max_width_px: default_playlist_album_art_column_max_width_px(
            ),
            layout: LayoutConfig::default(),
            button_cluster_instances: Vec::new(),
            playlist_columns: default_playlist_columns(),
            window_width: default_window_width(),
            window_height: default_window_height(),
            volume: default_volume(),
        }
    }
}

impl Default for BufferingConfig {
    fn default() -> Self {
        Self {
            player_low_watermark_ms: default_player_low_watermark_ms(),
            player_target_buffer_ms: default_player_target_buffer_ms(),
            player_request_interval_ms: default_player_request_interval_ms(),
            decoder_request_chunk_ms: default_decoder_request_chunk_ms(),
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            show_first_start_prompt: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_player_low_watermark_ms() -> u32 {
    12_000
}

fn default_player_target_buffer_ms() -> u32 {
    24_000
}

fn default_player_request_interval_ms() -> u32 {
    120
}

fn default_decoder_request_chunk_ms() -> u32 {
    1_500
}

fn default_window_width() -> u32 {
    900
}

fn default_window_height() -> u32 {
    650
}

fn default_volume() -> f32 {
    1.0
}

pub fn default_playlist_album_art_column_min_width_px() -> u32 {
    16
}

pub fn default_playlist_album_art_column_max_width_px() -> u32 {
    480
}

/// Returns the built-in playlist column set used for new configs.
pub fn default_playlist_columns() -> Vec<PlaylistColumnConfig> {
    vec![
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
        PlaylistColumnConfig {
            name: "Album Artist".to_string(),
            format: "{album_artist}".to_string(),
            enabled: false,
            custom: false,
        },
        PlaylistColumnConfig {
            name: "Genre".to_string(),
            format: "{genre}".to_string(),
            enabled: false,
            custom: false,
        },
        PlaylistColumnConfig {
            name: "Year".to_string(),
            format: "{year}".to_string(),
            enabled: false,
            custom: false,
        },
        PlaylistColumnConfig {
            name: "Track #".to_string(),
            format: "{track_number}".to_string(),
            enabled: false,
            custom: false,
        },
        PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: false,
            custom: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{default_playlist_columns, BufferingConfig, Config, LayoutConfig};

    #[test]
    fn test_default_config_has_expected_values_and_auto_output_modes() {
        let config = Config::default();

        assert_eq!(config.output.channel_count, 2);
        assert_eq!(config.output.sample_rate_khz, 44_100);
        assert_eq!(config.output.bits_per_sample, 24);
        assert!(config.output.output_device_name.is_empty());
        assert!(config.output.output_device_auto);
        assert!(config.output.channel_count_auto);
        assert!(config.output.sample_rate_auto);
        assert!(config.output.bits_per_sample_auto);

        assert!(config.ui.show_layout_edit_intro);
        assert!(config.ui.show_tooltips);
        assert_eq!(config.ui.playlist_album_art_column_min_width_px, 16);
        assert_eq!(config.ui.playlist_album_art_column_max_width_px, 480);
        assert_eq!(config.ui.layout, LayoutConfig::default());
        assert!(config.ui.button_cluster_instances.is_empty());
        assert_eq!(config.ui.playlist_columns, default_playlist_columns());
        assert_eq!(config.ui.window_width, 900);
        assert_eq!(config.ui.window_height, 650);
        assert!((config.ui.volume - 1.0).abs() < f32::EPSILON);
        assert!(config.library.folders.is_empty());
        assert!(config.library.show_first_start_prompt);
        assert_eq!(config.buffering.player_low_watermark_ms, 12_000);
        assert_eq!(config.buffering.player_target_buffer_ms, 24_000);
        assert_eq!(config.buffering.player_request_interval_ms, 120);
        assert_eq!(config.buffering.decoder_request_chunk_ms, 1_500);
    }

    #[test]
    fn test_legacy_config_deserialization_defaults_auto_modes_to_true() {
        let legacy_config_toml = r#"
[output]
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24

[buffering]
player_low_watermark_ms = 12000
player_target_buffer_ms = 24000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1500
"#;

        let parsed: Config = toml::from_str(legacy_config_toml).expect("config should parse");
        assert!(parsed.output.output_device_name.is_empty());
        assert!(parsed.output.output_device_auto);
        assert!(parsed.output.channel_count_auto);
        assert!(parsed.output.sample_rate_auto);
        assert!(parsed.output.bits_per_sample_auto);
        assert_eq!(parsed.ui.layout, LayoutConfig::default());
        assert!(parsed.ui.button_cluster_instances.is_empty());
        assert!(parsed.ui.show_layout_edit_intro);
        assert!(parsed.ui.show_tooltips);
        assert_eq!(parsed.ui.playlist_album_art_column_min_width_px, 16);
        assert_eq!(parsed.ui.playlist_album_art_column_max_width_px, 480);
        assert_eq!(parsed.ui.playlist_columns, default_playlist_columns());
        assert_eq!(parsed.ui.window_width, 900);
        assert_eq!(parsed.ui.window_height, 650);
        assert!((parsed.ui.volume - 1.0).abs() < f32::EPSILON);
        assert!(parsed.library.folders.is_empty());
        assert!(parsed.library.show_first_start_prompt);
        assert_eq!(
            parsed.buffering.player_target_buffer_ms,
            BufferingConfig::default().player_target_buffer_ms
        );
    }

    #[test]
    fn test_default_playlist_columns_include_album_art_builtin_disabled() {
        let columns = default_playlist_columns();
        let album_art_column = columns
            .iter()
            .find(|column| column.format == "{album_art}")
            .expect("album art built-in column should exist");

        assert_eq!(album_art_column.name, "Album Art");
        assert!(!album_art_column.enabled);
        assert!(!album_art_column.custom);
    }

    #[test]
    fn test_config_serialization_omits_layout_table() {
        let config_text =
            toml::to_string(&Config::default()).expect("default config should serialize");

        assert!(!config_text.contains("[ui.layout]"));
        assert!(!config_text.contains("node_type"));
    }

    #[test]
    fn test_system_config_template_matches_default_values() {
        let parsed: Config = toml::from_str(include_str!("../config/config.system.toml"))
            .expect("system config template should parse");
        let defaults = Config::default();

        assert_eq!(
            parsed.output.output_device_name,
            defaults.output.output_device_name
        );
        assert_eq!(
            parsed.output.output_device_auto,
            defaults.output.output_device_auto
        );
        assert_eq!(parsed.output.channel_count, defaults.output.channel_count);
        assert_eq!(
            parsed.output.sample_rate_khz,
            defaults.output.sample_rate_khz
        );
        assert_eq!(
            parsed.output.bits_per_sample,
            defaults.output.bits_per_sample
        );
        assert_eq!(
            parsed.output.channel_count_auto,
            defaults.output.channel_count_auto
        );
        assert_eq!(
            parsed.output.sample_rate_auto,
            defaults.output.sample_rate_auto
        );
        assert_eq!(
            parsed.output.bits_per_sample_auto,
            defaults.output.bits_per_sample_auto
        );

        assert_eq!(
            parsed.ui.show_layout_edit_intro,
            defaults.ui.show_layout_edit_intro
        );
        assert_eq!(parsed.ui.show_tooltips, defaults.ui.show_tooltips);
        assert_eq!(
            parsed.ui.playlist_album_art_column_min_width_px,
            defaults.ui.playlist_album_art_column_min_width_px
        );
        assert_eq!(
            parsed.ui.playlist_album_art_column_max_width_px,
            defaults.ui.playlist_album_art_column_max_width_px
        );
        assert_eq!(parsed.ui.layout, LayoutConfig::default());
        assert!(parsed.ui.button_cluster_instances.is_empty());
        assert_eq!(parsed.ui.playlist_columns, default_playlist_columns());
        assert_eq!(parsed.ui.window_width, defaults.ui.window_width);
        assert_eq!(parsed.ui.window_height, defaults.ui.window_height);
        assert!((parsed.ui.volume - defaults.ui.volume).abs() < f32::EPSILON);
        assert_eq!(parsed.library.folders, defaults.library.folders);
        assert_eq!(
            parsed.library.show_first_start_prompt,
            defaults.library.show_first_start_prompt
        );

        assert_eq!(
            parsed.buffering.player_low_watermark_ms,
            defaults.buffering.player_low_watermark_ms
        );
        assert_eq!(
            parsed.buffering.player_target_buffer_ms,
            defaults.buffering.player_target_buffer_ms
        );
        assert_eq!(
            parsed.buffering.player_request_interval_ms,
            defaults.buffering.player_request_interval_ms
        );
        assert_eq!(
            parsed.buffering.decoder_request_chunk_ms,
            defaults.buffering.decoder_request_chunk_ms
        );
    }
}
