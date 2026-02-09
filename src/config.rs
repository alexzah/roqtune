//! Persistent application configuration model and defaults.

/// Root configuration persisted to `music_player.toml`.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Config {
    /// Audio output and device preferences.
    pub output: OutputConfig,
    #[serde(default)]
    /// UI and layout preferences.
    pub ui: UiConfig,
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
    pub show_album_art: bool,
    #[serde(default = "default_playlist_columns")]
    pub playlist_columns: Vec<PlaylistColumnConfig>,
    #[serde(default = "default_window_width")]
    pub window_width: u32,
    #[serde(default = "default_window_height")]
    pub window_height: u32,
    #[serde(default = "default_volume")]
    pub volume: f32,
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
            show_album_art: true,
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
    ]
}

#[cfg(test)]
mod tests {
    use super::{default_playlist_columns, BufferingConfig, Config};

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

        assert!(config.ui.show_album_art);
        assert_eq!(config.ui.playlist_columns, default_playlist_columns());
        assert_eq!(config.ui.window_width, 900);
        assert_eq!(config.ui.window_height, 650);
        assert!((config.ui.volume - 1.0).abs() < f32::EPSILON);
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

[ui]
show_album_art = true

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
        assert_eq!(parsed.ui.playlist_columns, default_playlist_columns());
        assert_eq!(parsed.ui.window_width, 900);
        assert_eq!(parsed.ui.window_height, 650);
        assert!((parsed.ui.volume - 1.0).abs() < f32::EPSILON);
        assert_eq!(
            parsed.buffering.player_target_buffer_ms,
            BufferingConfig::default().player_target_buffer_ms
        );
    }
}
