//! Persistent application configuration model and defaults.

use crate::layout::LayoutConfig;

/// Root configuration persisted to `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Config {
    /// Audio output and device preferences.
    pub output: OutputConfig,
    #[serde(default)]
    /// Cast playback preferences.
    pub cast: CastConfig,
    #[serde(default)]
    /// UI preferences.
    pub ui: UiConfig,
    #[serde(default)]
    /// Library indexing preferences.
    pub library: LibraryConfig,
    #[serde(default)]
    /// Decoder/player buffering behavior.
    pub buffering: BufferingConfig,
    #[serde(default)]
    /// Remote integration profile configuration.
    pub integrations: IntegrationsConfig,
}

/// Output device and format preferences.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
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
    #[serde(default)]
    pub resampler_quality: ResamplerQuality,
    #[serde(default = "default_true")]
    pub dither_on_bitdepth_reduce: bool,
    #[serde(default = "default_true")]
    pub downmix_higher_channel_tracks: bool,
}

/// Cast playback preferences persisted between sessions.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, Default)]
pub struct CastConfig {
    /// Enable sender-side transcoding fallback for receivers that reject direct source streams.
    #[serde(default)]
    pub allow_transcode_fallback: bool,
}

/// Resampler quality profile used when sample-rate conversion is required.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResamplerQuality {
    /// Good quality with lower CPU usage.
    #[default]
    High,
    /// Highest quality with higher CPU usage.
    #[serde(alias = "very_high")]
    Highest,
}

/// UI preferences persisted between sessions.
/// Layout-owned settings must live in `LayoutConfig` and be persisted in `layout.toml`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct UiConfig {
    #[serde(default = "default_true")]
    pub show_layout_edit_intro: bool,
    #[serde(default = "default_true")]
    pub show_tooltips: bool,
    #[serde(default = "default_true")]
    pub auto_scroll_to_playing_track: bool,
    /// Legacy dark-mode flag used only for one-time migration to layout-owned theme selection.
    #[serde(rename = "dark_mode", default, skip_serializing)]
    pub legacy_dark_mode: Option<bool>,
    /// Runtime playlist-column width minimum loaded from `layout.toml`.
    #[serde(skip, default = "default_playlist_album_art_column_min_width_px")]
    pub playlist_album_art_column_min_width_px: u32,
    /// Runtime playlist-column width maximum loaded from `layout.toml`.
    #[serde(skip, default = "default_playlist_album_art_column_max_width_px")]
    pub playlist_album_art_column_max_width_px: u32,
    /// Runtime-only layout state loaded from `layout.toml`.
    #[serde(skip)]
    pub layout: LayoutConfig,
    /// Runtime playlist column set loaded from `layout.toml`.
    #[serde(skip, default = "default_playlist_columns")]
    pub playlist_columns: Vec<PlaylistColumnConfig>,
    #[serde(default = "default_window_width")]
    pub window_width: u32,
    #[serde(default = "default_window_height")]
    pub window_height: u32,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub playback_order: UiPlaybackOrder,
    #[serde(default)]
    pub repeat_mode: UiRepeatMode,
}

/// Persisted playback-order preference for startup restore.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiPlaybackOrder {
    #[default]
    Default,
    Shuffle,
    Random,
}

/// Persisted repeat preference for startup restore.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiRepeatMode {
    #[default]
    Off,
    Playlist,
    Track,
}

/// Library indexing preferences persisted between sessions.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LibraryConfig {
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub online_metadata_enabled: bool,
    #[serde(default = "default_true")]
    pub online_metadata_prompt_pending: bool,
    #[serde(default = "default_true")]
    pub include_playlist_tracks_in_library: bool,
    #[serde(default = "default_list_image_max_edge_px")]
    pub list_image_max_edge_px: u32,
    #[serde(default = "default_cover_art_cache_max_size_mb")]
    pub cover_art_cache_max_size_mb: u32,
    #[serde(default = "default_cover_art_memory_cache_max_size_mb")]
    pub cover_art_memory_cache_max_size_mb: u32,
    #[serde(default = "default_artist_image_memory_cache_max_size_mb")]
    pub artist_image_memory_cache_max_size_mb: u32,
    #[serde(default = "default_image_memory_cache_ttl_secs")]
    pub image_memory_cache_ttl_secs: u32,
    #[serde(default = "default_artist_image_cache_ttl_days")]
    pub artist_image_cache_ttl_days: u32,
    #[serde(default = "default_artist_image_cache_max_size_mb")]
    pub artist_image_cache_max_size_mb: u32,
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
/// This is layout-owned data and must be persisted in `layout.toml`, not `config.toml`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ButtonClusterInstanceConfig {
    /// Layout leaf identifier that owns this cluster instance.
    pub leaf_id: String,
    /// Ordered action ids rendered in the cluster.
    pub actions: Vec<i32>,
}

/// Tuning knobs for decode/playback buffering.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
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

/// Integration profile configuration persisted between sessions.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize, Default)]
pub struct IntegrationsConfig {
    #[serde(default)]
    pub backends: Vec<BackendProfileConfig>,
}

/// Persisted backend profile metadata (non-secret fields only).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct BackendProfileConfig {
    pub profile_id: String,
    #[serde(default)]
    pub backend_kind: IntegrationBackendKind,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub enabled: bool,
}

/// Supported backend profile kinds persisted in config.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationBackendKind {
    #[default]
    OpenSubsonic,
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
            resampler_quality: ResamplerQuality::High,
            dither_on_bitdepth_reduce: true,
            downmix_higher_channel_tracks: true,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_layout_edit_intro: true,
            show_tooltips: true,
            auto_scroll_to_playing_track: true,
            legacy_dark_mode: None,
            playlist_album_art_column_min_width_px: default_playlist_album_art_column_min_width_px(
            ),
            playlist_album_art_column_max_width_px: default_playlist_album_art_column_max_width_px(
            ),
            layout: LayoutConfig::default(),
            playlist_columns: default_playlist_columns(),
            window_width: default_window_width(),
            window_height: default_window_height(),
            volume: default_volume(),
            playback_order: UiPlaybackOrder::Default,
            repeat_mode: UiRepeatMode::Off,
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
            online_metadata_enabled: false,
            online_metadata_prompt_pending: true,
            include_playlist_tracks_in_library: true,
            list_image_max_edge_px: default_list_image_max_edge_px(),
            cover_art_cache_max_size_mb: default_cover_art_cache_max_size_mb(),
            cover_art_memory_cache_max_size_mb: default_cover_art_memory_cache_max_size_mb(),
            artist_image_memory_cache_max_size_mb: default_artist_image_memory_cache_max_size_mb(),
            image_memory_cache_ttl_secs: default_image_memory_cache_ttl_secs(),
            artist_image_cache_ttl_days: default_artist_image_cache_ttl_days(),
            artist_image_cache_max_size_mb: default_artist_image_cache_max_size_mb(),
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

fn default_artist_image_cache_ttl_days() -> u32 {
    30
}

fn default_image_memory_cache_ttl_secs() -> u32 {
    20
}

fn default_list_image_max_edge_px() -> u32 {
    320
}

fn default_cover_art_cache_max_size_mb() -> u32 {
    512
}

fn default_cover_art_memory_cache_max_size_mb() -> u32 {
    50
}

fn default_artist_image_memory_cache_max_size_mb() -> u32 {
    50
}

fn default_artist_image_cache_max_size_mb() -> u32 {
    256
}

pub fn default_playlist_album_art_column_min_width_px() -> u32 {
    16
}

pub fn default_playlist_album_art_column_max_width_px() -> u32 {
    480
}

pub const BUILTIN_TRACK_DETAILS_COLUMN_FORMAT: &str =
    "[size=body][color=text_primary]{title;file_name}[/color][/size]\\n[size=caption][color=text_secondary]{artist;album_artist} • {album}[/color][/size]";

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
            name: "Track Details".to_string(),
            format: BUILTIN_TRACK_DETAILS_COLUMN_FORMAT.to_string(),
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
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
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
    use super::{
        default_playlist_columns, BufferingConfig, Config, IntegrationBackendKind, LayoutConfig,
        ResamplerQuality, UiConfig, UiPlaybackOrder, UiRepeatMode,
        BUILTIN_TRACK_DETAILS_COLUMN_FORMAT,
    };

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
        assert_eq!(config.output.resampler_quality, ResamplerQuality::High);
        assert!(config.output.dither_on_bitdepth_reduce);
        assert!(config.output.downmix_higher_channel_tracks);
        assert!(!config.cast.allow_transcode_fallback);

        assert!(config.ui.show_layout_edit_intro);
        assert!(config.ui.show_tooltips);
        assert!(config.ui.auto_scroll_to_playing_track);
        assert_eq!(config.ui.legacy_dark_mode, None);
        assert_eq!(config.ui.playlist_album_art_column_min_width_px, 16);
        assert_eq!(config.ui.playlist_album_art_column_max_width_px, 480);
        assert_eq!(config.ui.layout, LayoutConfig::default());
        assert_eq!(config.ui.playlist_columns, default_playlist_columns());
        assert_eq!(config.ui.window_width, 900);
        assert_eq!(config.ui.window_height, 650);
        assert!((config.ui.volume - 1.0).abs() < f32::EPSILON);
        assert_eq!(config.ui.playback_order, UiPlaybackOrder::Default);
        assert_eq!(config.ui.repeat_mode, UiRepeatMode::Off);
        assert!(config.library.folders.is_empty());
        assert!(!config.library.online_metadata_enabled);
        assert!(config.library.online_metadata_prompt_pending);
        assert_eq!(config.library.list_image_max_edge_px, 320);
        assert_eq!(config.library.cover_art_cache_max_size_mb, 512);
        assert_eq!(config.library.cover_art_memory_cache_max_size_mb, 50);
        assert_eq!(config.library.artist_image_memory_cache_max_size_mb, 50);
        assert_eq!(config.library.image_memory_cache_ttl_secs, 20);
        assert_eq!(config.library.artist_image_cache_ttl_days, 30);
        assert_eq!(config.library.artist_image_cache_max_size_mb, 256);
        assert_eq!(config.buffering.player_low_watermark_ms, 12_000);
        assert_eq!(config.buffering.player_target_buffer_ms, 24_000);
        assert_eq!(config.buffering.player_request_interval_ms, 120);
        assert_eq!(config.buffering.decoder_request_chunk_ms, 1_500);
        assert!(config.integrations.backends.is_empty());
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
        assert_eq!(parsed.output.resampler_quality, ResamplerQuality::High);
        assert!(parsed.output.dither_on_bitdepth_reduce);
        assert!(parsed.output.downmix_higher_channel_tracks);
        assert!(!parsed.cast.allow_transcode_fallback);
        assert_eq!(parsed.ui.layout, LayoutConfig::default());
        assert!(parsed.ui.show_layout_edit_intro);
        assert!(parsed.ui.show_tooltips);
        assert!(parsed.ui.auto_scroll_to_playing_track);
        assert_eq!(parsed.ui.legacy_dark_mode, None);
        assert_eq!(parsed.ui.playlist_album_art_column_min_width_px, 16);
        assert_eq!(parsed.ui.playlist_album_art_column_max_width_px, 480);
        assert_eq!(parsed.ui.playlist_columns, default_playlist_columns());
        assert_eq!(parsed.ui.window_width, 900);
        assert_eq!(parsed.ui.window_height, 650);
        assert!((parsed.ui.volume - 1.0).abs() < f32::EPSILON);
        assert_eq!(parsed.ui.playback_order, UiPlaybackOrder::Default);
        assert_eq!(parsed.ui.repeat_mode, UiRepeatMode::Off);
        assert!(parsed.library.folders.is_empty());
        assert!(!parsed.library.online_metadata_enabled);
        assert!(parsed.library.online_metadata_prompt_pending);
        assert_eq!(parsed.library.list_image_max_edge_px, 320);
        assert_eq!(parsed.library.cover_art_cache_max_size_mb, 512);
        assert_eq!(parsed.library.cover_art_memory_cache_max_size_mb, 50);
        assert_eq!(parsed.library.artist_image_memory_cache_max_size_mb, 50);
        assert_eq!(parsed.library.image_memory_cache_ttl_secs, 20);
        assert_eq!(parsed.library.artist_image_cache_ttl_days, 30);
        assert_eq!(parsed.library.artist_image_cache_max_size_mb, 256);
        assert_eq!(
            parsed.buffering.player_target_buffer_ms,
            BufferingConfig::default().player_target_buffer_ms
        );
        assert!(parsed.integrations.backends.is_empty());
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
    fn test_default_playlist_columns_include_track_details_builtin_disabled() {
        let columns = default_playlist_columns();
        let track_details_column = columns
            .iter()
            .find(|column| column.name == "Track Details")
            .expect("track details built-in column should exist");

        assert_eq!(
            track_details_column.format,
            BUILTIN_TRACK_DETAILS_COLUMN_FORMAT
        );
        assert!(!track_details_column.enabled);
        assert!(!track_details_column.custom);
    }

    #[test]
    fn test_config_serialization_omits_layout_table() {
        let config_text =
            toml::to_string(&Config::default()).expect("default config should serialize");

        assert!(!config_text.contains("[ui.layout]"));
        assert!(!config_text.contains("node_type"));
        assert!(!config_text.contains("[[ui.playlist_columns]]"));
        assert!(!config_text.contains("playlist_album_art_column_min_width_px"));
        assert!(!config_text.contains("playlist_album_art_column_max_width_px"));
        assert!(config_text.contains("playback_order"));
        assert!(config_text.contains("repeat_mode"));
        assert!(config_text.contains("allow_transcode_fallback"));
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
            parsed.output.resampler_quality,
            defaults.output.resampler_quality
        );
        assert_eq!(
            parsed.output.dither_on_bitdepth_reduce,
            defaults.output.dither_on_bitdepth_reduce
        );
        assert_eq!(
            parsed.output.downmix_higher_channel_tracks,
            defaults.output.downmix_higher_channel_tracks
        );

        assert_eq!(
            parsed.ui.show_layout_edit_intro,
            defaults.ui.show_layout_edit_intro
        );
        assert_eq!(parsed.ui.show_tooltips, defaults.ui.show_tooltips);
        assert_eq!(
            parsed.ui.auto_scroll_to_playing_track,
            defaults.ui.auto_scroll_to_playing_track
        );
        assert_eq!(parsed.ui.legacy_dark_mode, defaults.ui.legacy_dark_mode);
        assert_eq!(
            parsed.ui.playlist_album_art_column_min_width_px,
            defaults.ui.playlist_album_art_column_min_width_px
        );
        assert_eq!(
            parsed.ui.playlist_album_art_column_max_width_px,
            defaults.ui.playlist_album_art_column_max_width_px
        );
        assert_eq!(parsed.ui.layout, LayoutConfig::default());
        assert_eq!(parsed.ui.playlist_columns, default_playlist_columns());
        assert_eq!(parsed.ui.window_width, defaults.ui.window_width);
        assert_eq!(parsed.ui.window_height, defaults.ui.window_height);
        assert!((parsed.ui.volume - defaults.ui.volume).abs() < f32::EPSILON);
        assert_eq!(parsed.ui.playback_order, defaults.ui.playback_order);
        assert_eq!(parsed.ui.repeat_mode, defaults.ui.repeat_mode);
        assert_eq!(parsed.library.folders, defaults.library.folders);
        assert_eq!(
            parsed.library.online_metadata_enabled,
            defaults.library.online_metadata_enabled
        );
        assert_eq!(
            parsed.library.online_metadata_prompt_pending,
            defaults.library.online_metadata_prompt_pending
        );
        assert_eq!(
            parsed.library.list_image_max_edge_px,
            defaults.library.list_image_max_edge_px
        );
        assert_eq!(
            parsed.library.cover_art_cache_max_size_mb,
            defaults.library.cover_art_cache_max_size_mb
        );
        assert_eq!(
            parsed.library.cover_art_memory_cache_max_size_mb,
            defaults.library.cover_art_memory_cache_max_size_mb
        );
        assert_eq!(
            parsed.library.artist_image_memory_cache_max_size_mb,
            defaults.library.artist_image_memory_cache_max_size_mb
        );
        assert_eq!(
            parsed.library.image_memory_cache_ttl_secs,
            defaults.library.image_memory_cache_ttl_secs
        );
        assert_eq!(
            parsed.library.artist_image_cache_ttl_days,
            defaults.library.artist_image_cache_ttl_days
        );
        assert_eq!(
            parsed.library.artist_image_cache_max_size_mb,
            defaults.library.artist_image_cache_max_size_mb
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
        assert_eq!(parsed.integrations.backends, defaults.integrations.backends);
    }

    #[test]
    fn test_backend_kind_round_trip() {
        #[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
        struct Wrapper {
            backend_kind: IntegrationBackendKind,
        }

        let value = Wrapper {
            backend_kind: IntegrationBackendKind::OpenSubsonic,
        };
        let serialized =
            toml::to_string(&value).expect("backend kind enum should serialize to toml");
        let parsed: Wrapper =
            toml::from_str(&serialized).expect("backend kind enum should deserialize from toml");
        assert_eq!(parsed, value);
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

        let sanitized = crate::sanitize_config(input);
        assert_eq!(sanitized.ui.playlist_album_art_column_min_width_px, 24);
        assert_eq!(sanitized.ui.playlist_album_art_column_max_width_px, 512);
    }
}
