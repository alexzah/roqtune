//! Config/layout persistence helpers with comment-preserving TOML updates.

use std::path::{Path, PathBuf};

use log::warn;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

use crate::{
    config::{Config, IntegrationBackendKind, UiPlaybackOrder, UiRepeatMode},
    layout::LayoutConfig,
};

fn set_table_value_preserving_decor(table: &mut Table, key: &str, item: Item) {
    let replacing_scalar_with_aot = item.is_array_of_tables()
        && table
            .get(key)
            .is_some_and(|current| !current.is_array_of_tables());
    if replacing_scalar_with_aot {
        table.remove(key);
        table[key] = item;
        return;
    }

    let existing_value_decor = table
        .get(key)
        .and_then(|current| current.as_value().map(|value| value.decor().clone()));
    table[key] = item;
    if let Some(existing_value_decor) = existing_value_decor {
        if let Some(next_value) = table[key].as_value_mut() {
            *next_value.decor_mut() = existing_value_decor;
        }
    }
}

fn set_table_scalar_if_changed<T, F>(
    table: &mut Table,
    key: &str,
    previous_value: T,
    next_value: T,
    to_item: F,
) where
    T: PartialEq + Copy,
    F: FnOnce(T) -> Item,
{
    if table.contains_key(key) && previous_value == next_value {
        return;
    }
    set_table_value_preserving_decor(table, key, to_item(next_value));
}

fn ensure_section_table(document: &mut DocumentMut, key: &str) {
    let root = document.as_table_mut();
    let should_replace = !matches!(root.get(key), Some(item) if item.is_table());
    if should_replace {
        root.insert(key, Item::Table(Table::new()));
    }
}

fn write_config_to_document(document: &mut DocumentMut, previous: &Config, config: &Config) {
    ensure_section_table(document, "output");
    ensure_section_table(document, "cast");
    ensure_section_table(document, "ui");
    ensure_section_table(document, "library");
    ensure_section_table(document, "buffering");
    ensure_section_table(document, "integrations");

    {
        let output = document["output"]
            .as_table_mut()
            .expect("output should be a table");
        if !output.contains_key("output_device_name")
            || previous.output.output_device_name != config.output.output_device_name
        {
            set_table_value_preserving_decor(
                output,
                "output_device_name",
                value(config.output.output_device_name.clone()),
            );
        }
        set_table_scalar_if_changed(
            output,
            "output_device_auto",
            previous.output.output_device_auto,
            config.output.output_device_auto,
            value,
        );
        set_table_scalar_if_changed(
            output,
            "channel_count",
            i64::from(previous.output.channel_count),
            i64::from(config.output.channel_count),
            value,
        );
        set_table_scalar_if_changed(
            output,
            "sample_rate_khz",
            i64::from(previous.output.sample_rate_khz),
            i64::from(config.output.sample_rate_khz),
            value,
        );
        set_table_scalar_if_changed(
            output,
            "bits_per_sample",
            i64::from(previous.output.bits_per_sample),
            i64::from(config.output.bits_per_sample),
            value,
        );
        set_table_scalar_if_changed(
            output,
            "channel_count_auto",
            previous.output.channel_count_auto,
            config.output.channel_count_auto,
            value,
        );
        set_table_scalar_if_changed(
            output,
            "sample_rate_auto",
            previous.output.sample_rate_auto,
            config.output.sample_rate_auto,
            value,
        );
        set_table_scalar_if_changed(
            output,
            "bits_per_sample_auto",
            previous.output.bits_per_sample_auto,
            config.output.bits_per_sample_auto,
            value,
        );
        if !output.contains_key("resampler_quality")
            || previous.output.resampler_quality != config.output.resampler_quality
        {
            let resampler_quality = match config.output.resampler_quality {
                crate::config::ResamplerQuality::High => "high",
                crate::config::ResamplerQuality::Highest => "highest",
            };
            set_table_value_preserving_decor(output, "resampler_quality", value(resampler_quality));
        }
        if !output.contains_key("dither_on_bitdepth_reduce")
            || previous.output.dither_on_bitdepth_reduce != config.output.dither_on_bitdepth_reduce
        {
            set_table_value_preserving_decor(
                output,
                "dither_on_bitdepth_reduce",
                value(config.output.dither_on_bitdepth_reduce),
            );
        }
        if !output.contains_key("downmix_higher_channel_tracks")
            || previous.output.downmix_higher_channel_tracks
                != config.output.downmix_higher_channel_tracks
        {
            set_table_value_preserving_decor(
                output,
                "downmix_higher_channel_tracks",
                value(config.output.downmix_higher_channel_tracks),
            );
        }
    }

    {
        let cast = document["cast"]
            .as_table_mut()
            .expect("cast should be a table");
        set_table_scalar_if_changed(
            cast,
            "allow_transcode_fallback",
            previous.cast.allow_transcode_fallback,
            config.cast.allow_transcode_fallback,
            value,
        );
    }

    {
        let ui = document["ui"].as_table_mut().expect("ui should be a table");
        set_table_scalar_if_changed(
            ui,
            "show_layout_edit_intro",
            previous.ui.show_layout_edit_intro,
            config.ui.show_layout_edit_intro,
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "show_tooltips",
            previous.ui.show_tooltips,
            config.ui.show_tooltips,
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "auto_scroll_to_playing_track",
            previous.ui.auto_scroll_to_playing_track,
            config.ui.auto_scroll_to_playing_track,
            value,
        );
        ui.remove("dark_mode");
        // Layout-owned state is persisted in layout.toml only.
        ui.remove("button_cluster_instances");
        ui.remove("playlist_columns");
        ui.remove("playlist_album_art_column_min_width_px");
        ui.remove("playlist_album_art_column_max_width_px");
        ui.remove("layout");
        ui.remove("viewer_panel_instances");
        set_table_scalar_if_changed(
            ui,
            "window_width",
            i64::from(previous.ui.window_width),
            i64::from(config.ui.window_width),
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "window_height",
            i64::from(previous.ui.window_height),
            i64::from(config.ui.window_height),
            value,
        );
        set_table_scalar_if_changed(
            ui,
            "volume",
            f64::from(previous.ui.volume),
            f64::from(config.ui.volume),
            value,
        );
        if !ui.contains_key("playback_order")
            || previous.ui.playback_order != config.ui.playback_order
        {
            let playback_order = match config.ui.playback_order {
                UiPlaybackOrder::Default => "default",
                UiPlaybackOrder::Shuffle => "shuffle",
                UiPlaybackOrder::Random => "random",
            };
            set_table_value_preserving_decor(ui, "playback_order", value(playback_order));
        }
        if !ui.contains_key("repeat_mode") || previous.ui.repeat_mode != config.ui.repeat_mode {
            let repeat_mode = match config.ui.repeat_mode {
                UiRepeatMode::Off => "off",
                UiRepeatMode::Playlist => "playlist",
                UiRepeatMode::Track => "track",
            };
            set_table_value_preserving_decor(ui, "repeat_mode", value(repeat_mode));
        }
    }

    {
        let library = document["library"]
            .as_table_mut()
            .expect("library should be a table");
        library.remove("show_first_start_prompt");
        set_table_scalar_if_changed(
            library,
            "online_metadata_enabled",
            previous.library.online_metadata_enabled,
            config.library.online_metadata_enabled,
            value,
        );
        set_table_scalar_if_changed(
            library,
            "online_metadata_prompt_pending",
            previous.library.online_metadata_prompt_pending,
            config.library.online_metadata_prompt_pending,
            value,
        );
        set_table_scalar_if_changed(
            library,
            "list_image_max_edge_px",
            i64::from(previous.library.list_image_max_edge_px),
            i64::from(config.library.list_image_max_edge_px),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "cover_art_cache_max_size_mb",
            i64::from(previous.library.cover_art_cache_max_size_mb),
            i64::from(config.library.cover_art_cache_max_size_mb),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "cover_art_memory_cache_max_size_mb",
            i64::from(previous.library.cover_art_memory_cache_max_size_mb),
            i64::from(config.library.cover_art_memory_cache_max_size_mb),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "artist_image_memory_cache_max_size_mb",
            i64::from(previous.library.artist_image_memory_cache_max_size_mb),
            i64::from(config.library.artist_image_memory_cache_max_size_mb),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "image_memory_cache_ttl_secs",
            i64::from(previous.library.image_memory_cache_ttl_secs),
            i64::from(config.library.image_memory_cache_ttl_secs),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "artist_image_cache_ttl_days",
            i64::from(previous.library.artist_image_cache_ttl_days),
            i64::from(config.library.artist_image_cache_ttl_days),
            value,
        );
        set_table_scalar_if_changed(
            library,
            "artist_image_cache_max_size_mb",
            i64::from(previous.library.artist_image_cache_max_size_mb),
            i64::from(config.library.artist_image_cache_max_size_mb),
            value,
        );
        if !library.contains_key("folders") || previous.library.folders != config.library.folders {
            let mut folders = Array::new();
            for folder in &config.library.folders {
                folders.push(folder.as_str());
            }
            set_table_value_preserving_decor(library, "folders", value(folders));
        }
    }

    {
        let buffering = document["buffering"]
            .as_table_mut()
            .expect("buffering should be a table");
        set_table_scalar_if_changed(
            buffering,
            "player_low_watermark_ms",
            i64::from(previous.buffering.player_low_watermark_ms),
            i64::from(config.buffering.player_low_watermark_ms),
            value,
        );
        set_table_scalar_if_changed(
            buffering,
            "player_target_buffer_ms",
            i64::from(previous.buffering.player_target_buffer_ms),
            i64::from(config.buffering.player_target_buffer_ms),
            value,
        );
        set_table_scalar_if_changed(
            buffering,
            "player_request_interval_ms",
            i64::from(previous.buffering.player_request_interval_ms),
            i64::from(config.buffering.player_request_interval_ms),
            value,
        );
        set_table_scalar_if_changed(
            buffering,
            "decoder_request_chunk_ms",
            i64::from(previous.buffering.decoder_request_chunk_ms),
            i64::from(config.buffering.decoder_request_chunk_ms),
            value,
        );
    }

    {
        let integrations = document["integrations"]
            .as_table_mut()
            .expect("integrations should be a table");
        if !integrations.contains_key("backends")
            || previous.integrations.backends != config.integrations.backends
        {
            let mut backends = ArrayOfTables::new();
            for backend in &config.integrations.backends {
                let mut row = Table::new();
                row.insert("profile_id", value(backend.profile_id.clone()));
                row.insert(
                    "backend_kind",
                    value(match backend.backend_kind {
                        IntegrationBackendKind::OpenSubsonic => "open_subsonic",
                    }),
                );
                row.insert("display_name", value(backend.display_name.clone()));
                row.insert("endpoint", value(backend.endpoint.clone()));
                row.insert("username", value(backend.username.clone()));
                row.insert("enabled", value(backend.enabled));
                backends.push(row);
            }
            set_table_value_preserving_decor(
                integrations,
                "backends",
                Item::ArrayOfTables(backends),
            );
        }
    }
}

fn merge_table_with_targeted_updates(destination: &mut Table, source: &Table) {
    for (key, source_item) in source.iter() {
        match source_item {
            Item::Table(source_table) => {
                if !destination.get(key).is_some_and(Item::is_table) {
                    destination.insert(key, Item::Table(Table::new()));
                }
                let destination_table = destination
                    .get_mut(key)
                    .and_then(Item::as_table_mut)
                    .expect("table inserted above");
                merge_table_with_targeted_updates(destination_table, source_table);
            }
            Item::ArrayOfTables(source_array) => {
                if !destination.get(key).is_some_and(Item::is_array_of_tables) {
                    set_table_value_preserving_decor(destination, key, source_item.clone());
                    continue;
                }
                let destination_array = destination
                    .get_mut(key)
                    .and_then(Item::as_array_of_tables_mut)
                    .expect("array-of-tables verified above");
                let shared_len = destination_array.len().min(source_array.len());
                for index in 0..shared_len {
                    let destination_table = destination_array
                        .get_mut(index)
                        .expect("index should exist in destination array");
                    let source_table = source_array
                        .get(index)
                        .expect("index should exist in source array");
                    merge_table_with_targeted_updates(destination_table, source_table);
                }
                if source_array.len() > destination_array.len() {
                    for index in destination_array.len()..source_array.len() {
                        let source_table = source_array
                            .get(index)
                            .expect("index should exist in source array");
                        destination_array.push(source_table.clone());
                    }
                } else if source_array.len() < destination_array.len() {
                    for _ in source_array.len()..destination_array.len() {
                        destination_array.remove(source_array.len());
                    }
                }
            }
            _ => {
                set_table_value_preserving_decor(destination, key, source_item.clone());
            }
        }
    }
}

/// Serializes `config` while preserving comments and unrelated keys from `existing_text`.
pub fn serialize_config_with_preserved_comments(
    existing_text: &str,
    config: &Config,
) -> Result<String, String> {
    let previous = toml::from_str::<Config>(existing_text)
        .map_err(|err| format!("failed to parse existing config as Config: {}", err))?;
    let mut document = existing_text
        .parse::<DocumentMut>()
        .map_err(|err| format!("failed to parse existing config as TOML document: {}", err))?;
    write_config_to_document(&mut document, &previous, config);
    Ok(document.to_string())
}

/// Serializes `layout` while preserving comments and unrelated keys from `existing_text`.
pub fn serialize_layout_with_preserved_comments(
    existing_text: &str,
    layout: &LayoutConfig,
) -> Result<String, String> {
    let next_layout_text = toml::to_string(layout)
        .map_err(|err| format!("failed to serialize layout to TOML: {}", err))?;
    let next_document = next_layout_text
        .parse::<DocumentMut>()
        .map_err(|err| format!("failed to parse serialized layout TOML document: {}", err))?;
    let mut existing_document = existing_text
        .parse::<DocumentMut>()
        .map_err(|err| format!("failed to parse existing layout as TOML document: {}", err))?;

    merge_table_with_targeted_updates(existing_document.as_table_mut(), next_document.as_table());
    Ok(existing_document.to_string())
}

/// Persists `config.toml`, preferring comment-preserving updates when possible.
pub fn persist_config_file(config: &Config, path: &Path) {
    let existing_text = std::fs::read_to_string(path).ok();
    let config_text = if let Some(existing_text) = existing_text {
        match serialize_config_with_preserved_comments(&existing_text, config) {
            Ok(updated_text) => Some(updated_text),
            Err(err) => {
                warn!(
                    "Failed to preserve config comments for {} ({}). Falling back to plain serialization.",
                    path.display(),
                    err
                );
                toml::to_string(config).ok()
            }
        }
    } else {
        toml::to_string(config).ok()
    };

    let Some(config_text) = config_text else {
        log::error!("Failed to serialize config for {}", path.display());
        return;
    };

    if let Err(err) = std::fs::write(path, config_text) {
        log::error!("Failed to persist config to {}: {}", path.display(), err);
    }
}

/// Persists `layout.toml`, preferring comment-preserving updates when possible.
pub fn persist_layout_file(layout: &LayoutConfig, path: &Path) {
    let existing_text = std::fs::read_to_string(path).ok();
    let layout_text = if let Some(existing_text) = existing_text {
        match serialize_layout_with_preserved_comments(&existing_text, layout) {
            Ok(updated_text) => Some(updated_text),
            Err(err) => {
                warn!(
                    "Failed to preserve layout comments for {} ({}). Falling back to plain serialization.",
                    path.display(),
                    err
                );
                toml::to_string(layout).ok()
            }
        }
    } else {
        toml::to_string(layout).ok()
    };

    let Some(layout_text) = layout_text else {
        log::error!("Failed to serialize layout for {}", path.display());
        return;
    };

    if let Err(err) = std::fs::write(path, layout_text) {
        log::error!("Failed to persist layout to {}: {}", path.display(), err);
    }
}

/// Persists both config and layout state files to explicit paths.
pub fn persist_state_files(config: &Config, config_path: &Path, layout_path: &Path) {
    persist_config_file(config, config_path);
    persist_layout_file(&config.ui.layout, layout_path);
}

/// Persists state files using a config path and a sibling `layout.toml`.
pub fn persist_state_files_with_config_path(config: &Config, config_path: &Path) {
    let layout_path = config_path
        .parent()
        .map(|parent| parent.join("layout.toml"))
        .unwrap_or_else(|| PathBuf::from("layout.toml"));
    persist_state_files(config, config_path, &layout_path);
}

/// Returns the checked-in system layout template text.
pub fn system_layout_template_text() -> &'static str {
    include_str!("../config/layout.system.toml")
}

/// Loads and parses the checked-in system layout template.
pub fn load_system_layout_template() -> LayoutConfig {
    toml::from_str(system_layout_template_text())
        .expect("layout system template should parse into LayoutConfig")
}

/// Loads `layout.toml`, falling back to the system template on read/parse failure.
pub fn load_layout_file(path: &Path) -> LayoutConfig {
    let layout_content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            warn!(
                "Failed to read layout file {}. Using layout system template. error={}",
                path.display(),
                err
            );
            return load_system_layout_template();
        }
    };

    match toml::from_str::<LayoutConfig>(&layout_content) {
        Ok(layout) => layout,
        Err(err) => {
            warn!(
                "Failed to parse layout file {}. Using layout system template. error={}",
                path.display(),
                err
            );
            load_system_layout_template()
        }
    }
}

/// Copies layout-owned playlist column state into legacy UI fields for compatibility.
pub fn hydrate_ui_columns_from_layout(config: &mut Config) {
    config.ui.playlist_album_art_column_min_width_px =
        config.ui.layout.playlist_album_art_column_min_width_px;
    config.ui.playlist_album_art_column_max_width_px =
        config.ui.layout.playlist_album_art_column_max_width_px;
    config.ui.playlist_columns = config.ui.layout.playlist_columns.clone();
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{config::Config, layout::LayoutConfig};

    use super::{
        load_layout_file, load_system_layout_template, serialize_config_with_preserved_comments,
        serialize_layout_with_preserved_comments, system_layout_template_text,
    };

    fn unique_temp_directory(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "roqtune_{}_{}_{}",
            test_name,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_keeps_existing_comments() {
        let existing = r#"
# top comment
[output]
# output comment
output_device_name = ""
output_device_auto = true
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24
channel_count_auto = true
sample_rate_auto = true
bits_per_sample_auto = true

[ui]
# ui comment
show_layout_edit_intro = true
playlist_album_art_column_min_width_px = 16
playlist_album_art_column_max_width_px = 480
window_width = 900
window_height = 650
volume = 1.0
button_cluster_instances = []

[[ui.playlist_columns]]
name = "Title"
format = "{title}"
enabled = true
custom = false

[buffering]
# buffering comment
player_low_watermark_ms = 12000
player_target_buffer_ms = 24000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1500
"#;

        let mut config = Config::default();
        config.output.output_device_name = "My Device".to_string();
        config.ui.volume = 0.55;

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving serialization should succeed");

        assert!(serialized.contains("# top comment"));
        assert!(serialized.contains("# output comment"));
        assert!(serialized.contains("# ui comment"));
        assert!(serialized.contains("# buffering comment"));
        assert!(serialized.contains("output_device_name = \"My Device\""));
        assert!(serialized.contains("volume = 0.55"));
        assert!(
            !serialized.contains("button_cluster_instances"),
            "layout-owned button cluster settings should not be persisted in config.toml"
        );
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_rejects_invalid_toml() {
        let invalid = "[output\noutput_device_auto = true";
        let config = Config::default();
        let result = serialize_config_with_preserved_comments(invalid, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_keeps_system_template_comments() {
        let existing = include_str!("../config/config.system.toml");
        let mut config: Config =
            toml::from_str(existing).expect("system config template should parse");
        config.ui.show_layout_edit_intro = false;

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving serialization should succeed");

        assert!(serialized.contains("# roqtune system config template"));
        assert!(serialized.contains("# ADVANCED USERS ONLY"));
        assert!(serialized.contains("# Playback volume (0.0 .. 1.0)."));
        assert!(serialized.contains("show_layout_edit_intro = false"));
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_strips_layout_owned_ui_keys() {
        let existing = include_str!("../config/config.system.toml");
        let mut config: Config =
            toml::from_str(existing).expect("system config template should parse");
        config.ui.layout.button_cluster_instances =
            vec![crate::config::ButtonClusterInstanceConfig {
                leaf_id: "n8".to_string(),
                actions: vec![1, 2, 3],
            }];

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving serialization should succeed");

        assert!(
            !serialized.contains("button_cluster_instances"),
            "button cluster settings should not be persisted in config.toml.\nserialized=\n{}",
            serialized
        );
        assert!(
            !serialized.contains("[[ui.playlist_columns]]"),
            "playlist columns should not be persisted in config.toml"
        );
        assert!(
            !serialized.contains("playlist_album_art_column_min_width_px"),
            "playlist album art width fields should not be persisted in config.toml"
        );
        assert!(
            !serialized.contains("playlist_album_art_column_max_width_px"),
            "playlist album art width fields should not be persisted in config.toml"
        );
        assert!(
            !serialized.contains("viewer_panel_instances"),
            "viewer panel settings should not be persisted in config.toml"
        );
        let parsed_back: toml_edit::DocumentMut = serialized
            .parse()
            .expect("serialized config should remain valid TOML");
        assert!(parsed_back
            .get("ui")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("button_cluster_instances"))
            .is_none());
        assert!(parsed_back
            .get("ui")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("playlist_columns"))
            .is_none());
        assert!(parsed_back
            .get("ui")
            .and_then(|item| item.as_table())
            .and_then(|table| table.get("viewer_panel_instances"))
            .is_none());
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_keeps_existing_comments() {
        let existing = r#"
# layout top comment
version = 1

# album art width comment
playlist_album_art_column_min_width_px = 16
playlist_album_art_column_max_width_px = 480

[[playlist_columns]]
name = "Title"
format = "{title}"
enabled = true
custom = false

[root]
# root comment
node_type = "leaf"
id = "l1"
panel = "track_list"
"#;
        let mut layout: LayoutConfig =
            toml::from_str(existing).expect("existing layout should parse");
        layout.playlist_album_art_column_max_width_px = 512;
        layout.button_cluster_instances = vec![crate::config::ButtonClusterInstanceConfig {
            leaf_id: "n8".to_string(),
            actions: vec![1, 2, 3],
        }];

        let serialized = serialize_layout_with_preserved_comments(existing, &layout)
            .expect("comment-preserving layout serialization should succeed");

        assert!(serialized.contains("# layout top comment"));
        assert!(serialized.contains("# album art width comment"));
        assert!(serialized.contains("# root comment"));
        assert!(serialized.contains("playlist_album_art_column_max_width_px = 512"));
        assert!(serialized.contains("[[button_cluster_instances]]"));
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_rejects_invalid_toml() {
        let invalid = "[root\nnode_type = \"leaf\"";
        let layout = LayoutConfig::default();
        let result = serialize_layout_with_preserved_comments(invalid, &layout);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_keeps_system_template_comments() {
        let existing = include_str!("../config/layout.system.toml");
        let mut layout: LayoutConfig =
            toml::from_str(existing).expect("layout system template should parse");
        layout.playlist_album_art_column_min_width_px = 24;

        let serialized = serialize_layout_with_preserved_comments(existing, &layout)
            .expect("comment-preserving layout serialization should succeed");

        assert!(serialized.contains("# roqtune layout system template"));
        assert!(serialized.contains("# Node conventions:"));
        assert!(serialized.contains("playlist_album_art_column_min_width_px = 24"));
    }

    #[test]
    fn test_serialize_layout_with_preserved_comments_keeps_unknown_keys() {
        let existing = r#"
version = 1
custom_layout_flag = "keep-me"

[root]
node_type = "leaf"
id = "l1"
panel = "track_list"
"#;
        let layout = LayoutConfig::default();

        let serialized = serialize_layout_with_preserved_comments(existing, &layout)
            .expect("comment-preserving layout serialization should succeed");
        assert!(serialized.contains("custom_layout_flag = \"keep-me\""));
    }

    #[test]
    fn test_load_system_layout_template_matches_checked_in_template() {
        let parsed_from_template: LayoutConfig = toml::from_str(system_layout_template_text())
            .expect("layout system template should parse");
        let loaded = load_system_layout_template();
        assert_eq!(loaded, parsed_from_template);
    }

    #[test]
    fn test_load_layout_file_falls_back_to_system_template_when_missing() {
        let temp_dir = unique_temp_directory("layout_missing_fallback");
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let missing_path = temp_dir.join("missing-layout.toml");
        let loaded = load_layout_file(&missing_path);
        assert_eq!(loaded, load_system_layout_template());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_load_layout_file_falls_back_to_system_template_when_invalid() {
        let temp_dir = unique_temp_directory("layout_invalid_fallback");
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let invalid_path = temp_dir.join("layout.toml");
        std::fs::write(&invalid_path, "[root\nnode_type = \"leaf\"")
            .expect("invalid layout file should be written");
        let loaded = load_layout_file(&invalid_path);
        assert_eq!(loaded, load_system_layout_template());
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_keeps_unknown_keys() {
        let existing = r#"
[output]
output_device_name = ""
output_device_auto = true
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24
channel_count_auto = true
sample_rate_auto = true
bits_per_sample_auto = true

[ui]
show_layout_edit_intro = true
show_tooltips = true
window_width = 900
window_height = 650
volume = 1.0

[library]
folders = []
online_metadata_enabled = false
online_metadata_prompt_pending = true
artist_image_cache_ttl_days = 30
artist_image_cache_max_size_mb = 256
custom_library_key = "keep-me"

[buffering]
player_low_watermark_ms = 12000
player_target_buffer_ms = 24000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1500
"#;
        let config = Config::default();

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("comment-preserving config serialization should succeed");
        assert!(serialized.contains("custom_library_key = \"keep-me\""));
    }

    #[test]
    fn test_serialize_config_with_preserved_comments_persists_integrations_backends() {
        let existing = r#"
[output]
output_device_name = ""
output_device_auto = true
channel_count = 2
sample_rate_khz = 44100
bits_per_sample = 24
channel_count_auto = true
sample_rate_auto = true
bits_per_sample_auto = true

[cast]
allow_transcode_fallback = true

[ui]
show_layout_edit_intro = true
show_tooltips = true
auto_scroll_to_playing_track = true
dark_mode = true
window_width = 900
window_height = 650
volume = 0.8
playback_order = "default"
repeat_mode = "off"

[library]
folders = []
online_metadata_enabled = false
online_metadata_prompt_pending = true
artist_image_cache_ttl_days = 30
artist_image_cache_max_size_mb = 512

[buffering]
player_low_watermark_ms = 1500
player_target_buffer_ms = 12000
player_request_interval_ms = 120
decoder_request_chunk_ms = 1000

[integrations]
backends = []
"#;
        let mut config = Config::default();
        config.integrations.backends = vec![crate::config::BackendProfileConfig {
            profile_id: "opensubsonic-default".to_string(),
            backend_kind: crate::config::IntegrationBackendKind::OpenSubsonic,
            display_name: "OpenSubsonic".to_string(),
            endpoint: "https://music.example.com".to_string(),
            username: "alice".to_string(),
            enabled: true,
        }];

        let serialized = serialize_config_with_preserved_comments(existing, &config)
            .expect("integration backend should serialize");
        assert!(serialized.contains("[[integrations.backends]]"));
        assert!(serialized.contains("profile_id = \"opensubsonic-default\""));
        assert!(serialized.contains("backend_kind = \"open_subsonic\""));
        assert!(serialized.contains("endpoint = \"https://music.example.com\""));
        assert!(serialized.contains("username = \"alice\""));
        assert!(serialized.contains("enabled = true"));
    }
}
