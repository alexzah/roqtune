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
        set_table_scalar_if_changed(
            ui,
            "dark_mode",
            previous.ui.dark_mode,
            config.ui.dark_mode,
            value,
        );
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

pub fn persist_state_files(config: &Config, config_path: &Path, layout_path: &Path) {
    persist_config_file(config, config_path);
    persist_layout_file(&config.ui.layout, layout_path);
}

pub fn persist_state_files_with_config_path(config: &Config, config_path: &Path) {
    let layout_path = config_path
        .parent()
        .map(|parent| parent.join("layout.toml"))
        .unwrap_or_else(|| PathBuf::from("layout.toml"));
    persist_state_files(config, config_path, &layout_path);
}

pub fn system_layout_template_text() -> &'static str {
    include_str!("../config/layout.system.toml")
}

pub fn load_system_layout_template() -> LayoutConfig {
    toml::from_str(system_layout_template_text())
        .expect("layout system template should parse into LayoutConfig")
}

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

pub fn hydrate_ui_columns_from_layout(config: &mut Config) {
    config.ui.playlist_album_art_column_min_width_px =
        config.ui.layout.playlist_album_art_column_min_width_px;
    config.ui.playlist_album_art_column_max_width_px =
        config.ui.layout.playlist_album_art_column_max_width_px;
    config.ui.playlist_columns = config.ui.layout.playlist_columns.clone();
}
