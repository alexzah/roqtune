//! Playlist column sanitization, sizing, and header hit-testing helpers.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};

use slint::{Model, ModelRc, VecModel};

use crate::{
    config::{self, PlaylistColumnConfig, UiConfig},
    layout::{LayoutConfig, PlaylistColumnWidthOverrideConfig},
    AppWindow,
};

const FAVORITE_COLUMN_WIDTH_PX: i32 = 24;

/// Normalizes a column format token for stable comparisons and keys.
pub(crate) fn normalize_column_format(format: &str) -> String {
    format.trim().to_ascii_lowercase()
}

/// Returns `true` when a column is the built-in album-art placeholder column.
pub(crate) fn is_album_art_builtin_column(column: &PlaylistColumnConfig) -> bool {
    !column.custom && normalize_column_format(&column.format) == "{album_art}"
}

/// Returns `true` when a column is the built-in favorite-button placeholder column.
pub(crate) fn is_favorite_builtin_column(column: &PlaylistColumnConfig) -> bool {
    !column.custom && normalize_column_format(&column.format) == "{favorite}"
}

/// Maps a column to its UI kind code used by Slint models.
pub(crate) fn playlist_column_kind(column: &PlaylistColumnConfig) -> i32 {
    if is_album_art_builtin_column(column) {
        crate::PLAYLIST_COLUMN_KIND_ALBUM_ART
    } else if is_favorite_builtin_column(column) {
        crate::PLAYLIST_COLUMN_KIND_FAVORITE
    } else {
        crate::PLAYLIST_COLUMN_KIND_TEXT
    }
}

/// Returns UI kind codes for currently visible playlist columns.
pub(crate) fn visible_playlist_column_kinds(columns: &[PlaylistColumnConfig]) -> Vec<i32> {
    columns
        .iter()
        .filter(|column| column.enabled)
        .map(playlist_column_kind)
        .collect()
}

/// Sanitizes playlist columns, restoring missing built-ins and deduplicating entries.
pub(crate) fn sanitize_playlist_columns(
    columns: &[PlaylistColumnConfig],
) -> Vec<PlaylistColumnConfig> {
    let default_columns = config::default_playlist_columns();
    let mut seen_builtin_keys: HashSet<String> = HashSet::new();
    let mut seen_custom_keys: HashSet<String> = HashSet::new();
    let mut merged_columns: Vec<PlaylistColumnConfig> = Vec::new();

    let mut default_name_by_key: HashMap<String, String> = HashMap::new();
    for default_column in &default_columns {
        default_name_by_key.insert(
            normalize_column_format(&default_column.format),
            default_column.name.clone(),
        );
    }

    for column in columns {
        let trimmed_name = column.name.trim();
        let trimmed_format = column.format.trim();
        if trimmed_name.is_empty() || trimmed_format.is_empty() {
            continue;
        }

        if column.custom {
            let custom_key = format!("{}|{}", trimmed_name, trimmed_format);
            if seen_custom_keys.insert(custom_key) {
                merged_columns.push(PlaylistColumnConfig {
                    name: trimmed_name.to_string(),
                    format: trimmed_format.to_string(),
                    enabled: column.enabled,
                    custom: true,
                });
            }
        } else {
            let builtin_key = normalize_column_format(trimmed_format);
            if default_name_by_key.contains_key(&builtin_key)
                && seen_builtin_keys.insert(builtin_key.clone())
            {
                let builtin_name = default_name_by_key
                    .get(&builtin_key)
                    .cloned()
                    .unwrap_or_else(|| trimmed_name.to_string());
                merged_columns.push(PlaylistColumnConfig {
                    name: builtin_name,
                    format: trimmed_format.to_string(),
                    enabled: column.enabled,
                    custom: false,
                });
            }
        }
    }

    for default_column in default_columns {
        let key = normalize_column_format(&default_column.format);
        if !seen_builtin_keys.contains(&key) {
            merged_columns.push(default_column);
        }
    }

    if merged_columns.iter().all(|column| !column.enabled) {
        if let Some(first_column) = merged_columns.first_mut() {
            first_column.enabled = true;
        }
    }

    merged_columns
}

/// Builds a stable storage key for a playlist column definition.
pub(crate) fn playlist_column_key(column: &PlaylistColumnConfig) -> String {
    if column.custom {
        format!("custom:{}|{}", column.name.trim(), column.format.trim())
    } else {
        normalize_column_format(&column.format)
    }
}

#[derive(Clone, Copy)]
/// Pixel bounds for a playlist column width.
pub(crate) struct ColumnWidthBounds {
    /// Minimum width in pixels.
    pub(crate) min_px: i32,
    /// Maximum width in pixels.
    pub(crate) max_px: i32,
}

/// Returns album-art width bounds derived from current UI config.
pub(crate) fn album_art_column_width_bounds(ui: &UiConfig) -> ColumnWidthBounds {
    ColumnWidthBounds {
        min_px: ui.playlist_album_art_column_min_width_px as i32,
        max_px: ui.playlist_album_art_column_max_width_px as i32,
    }
}

#[cfg(test)]
/// Returns default album-art width bounds used by unit tests.
pub(crate) fn default_album_art_column_width_bounds() -> ColumnWidthBounds {
    ColumnWidthBounds {
        min_px: config::default_playlist_album_art_column_min_width_px() as i32,
        max_px: config::default_playlist_album_art_column_max_width_px() as i32,
    }
}

/// Returns width bounds for a column, using album-art overrides when applicable.
pub(crate) fn playlist_column_width_bounds_with_album_art(
    column: &PlaylistColumnConfig,
    album_art_bounds: ColumnWidthBounds,
) -> ColumnWidthBounds {
    if is_album_art_builtin_column(column) {
        return album_art_bounds;
    }
    if is_favorite_builtin_column(column) {
        return ColumnWidthBounds {
            min_px: FAVORITE_COLUMN_WIDTH_PX,
            max_px: FAVORITE_COLUMN_WIDTH_PX,
        };
    }

    let normalized_format = column.format.trim().to_ascii_lowercase();
    let normalized_name = column.name.trim().to_ascii_lowercase();

    if normalized_format == "{track}"
        || normalized_format == "{track_number}"
        || normalized_format == "{tracknumber}"
        || normalized_name == "track #"
        || normalized_name == "track"
    {
        return ColumnWidthBounds {
            min_px: 52,
            max_px: 90,
        };
    }

    if normalized_format == "{disc}" || normalized_format == "{disc_number}" {
        return ColumnWidthBounds {
            min_px: 50,
            max_px: 84,
        };
    }

    if normalized_format == "{year}" || normalized_format == "{date}" || normalized_name == "year" {
        return ColumnWidthBounds {
            min_px: 64,
            max_px: 104,
        };
    }

    if normalized_format == "{title}" || normalized_name == "title" {
        return ColumnWidthBounds {
            min_px: 140,
            max_px: 440,
        };
    }

    if normalized_name == "track details" {
        return ColumnWidthBounds {
            min_px: 170,
            max_px: 460,
        };
    }

    if normalized_format == "{artist}"
        || normalized_format == "{album_artist}"
        || normalized_format == "{albumartist}"
        || normalized_name == "artist"
        || normalized_name == "album artist"
    {
        return ColumnWidthBounds {
            min_px: 120,
            max_px: 320,
        };
    }

    if normalized_format == "{album}" || normalized_name == "album" {
        return ColumnWidthBounds {
            min_px: 140,
            max_px: 360,
        };
    }

    if normalized_format == "{genre}" || normalized_name == "genre" {
        return ColumnWidthBounds {
            min_px: 100,
            max_px: 210,
        };
    }

    if normalized_name == "duration"
        || normalized_name == "time"
        || normalized_format == "{duration}"
    {
        return ColumnWidthBounds {
            min_px: 78,
            max_px: 120,
        };
    }

    if column.custom {
        return ColumnWidthBounds {
            min_px: 110,
            max_px: 340,
        };
    }

    ColumnWidthBounds {
        min_px: 100,
        max_px: 300,
    }
}

#[cfg(test)]
/// Returns width bounds for a column using default album-art limits.
pub(crate) fn playlist_column_width_bounds(column: &PlaylistColumnConfig) -> ColumnWidthBounds {
    playlist_column_width_bounds_with_album_art(column, default_album_art_column_width_bounds())
}

/// Resolves width bounds for a visible column index.
pub(crate) fn playlist_column_bounds_at_visible_index(
    columns: &[PlaylistColumnConfig],
    visible_index: usize,
    album_art_bounds: ColumnWidthBounds,
) -> Option<ColumnWidthBounds> {
    columns
        .iter()
        .filter(|column| column.enabled)
        .nth(visible_index)
        .map(|column| playlist_column_width_bounds_with_album_art(column, album_art_bounds))
}

/// Resolves a stable column key for a visible column index.
pub(crate) fn playlist_column_key_at_visible_index(
    columns: &[PlaylistColumnConfig],
    visible_index: usize,
) -> Option<String> {
    columns
        .iter()
        .filter(|column| column.enabled)
        .nth(visible_index)
        .map(playlist_column_key)
}

/// Clamps a proposed width for a visible column to its allowed bounds.
pub(crate) fn clamp_width_for_visible_column(
    columns: &[PlaylistColumnConfig],
    visible_index: usize,
    width_px: i32,
    album_art_bounds: ColumnWidthBounds,
) -> Option<i32> {
    let bounds = playlist_column_bounds_at_visible_index(columns, visible_index, album_art_bounds)?;
    Some(width_px.clamp(bounds.min_px, bounds.max_px.max(bounds.min_px)))
}

/// Applies playlist column models and width state to the root UI component.
pub(crate) fn apply_playlist_columns_to_ui(ui: &AppWindow, config: &crate::config::Config) {
    let visible_headers: Vec<slint::SharedString> = config
        .ui
        .playlist_columns
        .iter()
        .filter(|column| column.enabled)
        .map(|column| column.name.as_str().into())
        .collect();
    let menu_labels: Vec<slint::SharedString> = config
        .ui
        .playlist_columns
        .iter()
        .map(|column| column.name.as_str().into())
        .collect();
    let menu_checked: Vec<bool> = config
        .ui
        .playlist_columns
        .iter()
        .map(|column| column.enabled)
        .collect();
    let menu_is_custom: Vec<bool> = config
        .ui
        .playlist_columns
        .iter()
        .map(|column| column.custom)
        .collect();
    let visible_kinds = visible_playlist_column_kinds(&config.ui.playlist_columns);

    ui.set_playlist_visible_column_headers(ModelRc::from(Rc::new(VecModel::from(visible_headers))));
    ui.set_playlist_visible_column_kinds(ModelRc::from(Rc::new(VecModel::from(visible_kinds))));
    ui.set_playlist_column_menu_labels(ModelRc::from(Rc::new(VecModel::from(menu_labels))));
    ui.set_playlist_column_menu_checked(ModelRc::from(Rc::new(VecModel::from(menu_checked))));
    ui.set_playlist_column_menu_is_custom(ModelRc::from(Rc::new(VecModel::from(menu_is_custom))));
}

/// Sanitizes layout-stored width overrides against known columns and bounds.
pub(crate) fn sanitize_layout_column_width_overrides(
    overrides: &[PlaylistColumnWidthOverrideConfig],
    columns: &[PlaylistColumnConfig],
    album_art_bounds: ColumnWidthBounds,
) -> Vec<PlaylistColumnWidthOverrideConfig> {
    let mut bounds_by_key: HashMap<String, ColumnWidthBounds> =
        HashMap::with_capacity(columns.len());
    for column in columns {
        bounds_by_key.insert(
            playlist_column_key(column),
            playlist_column_width_bounds_with_album_art(column, album_art_bounds),
        );
    }

    let mut seen_keys = HashSet::new();
    let mut sanitized = Vec::new();
    for override_item in overrides {
        let key = override_item.column_key.trim();
        if key.is_empty() || !seen_keys.insert(key.to_string()) {
            continue;
        }
        let Some(bounds) = bounds_by_key.get(key) else {
            continue;
        };
        let clamped_width_px = (override_item.width_px as i32)
            .clamp(bounds.min_px, bounds.max_px.max(bounds.min_px))
            as u32;
        sanitized.push(PlaylistColumnWidthOverrideConfig {
            column_key: key.to_string(),
            width_px: clamped_width_px,
        });
    }
    sanitized
}

/// Inserts or updates a width override for a given column key.
pub(crate) fn upsert_layout_column_width_override(
    layout: &mut LayoutConfig,
    column_key: &str,
    width_px: u32,
) {
    if let Some(existing) = layout
        .playlist_column_width_overrides
        .iter_mut()
        .find(|entry| entry.column_key == column_key)
    {
        existing.width_px = width_px;
        return;
    }
    layout
        .playlist_column_width_overrides
        .push(PlaylistColumnWidthOverrideConfig {
            column_key: column_key.to_string(),
            width_px,
        });
}

/// Removes any width override stored for the given column key.
pub(crate) fn clear_layout_column_width_override(layout: &mut LayoutConfig, column_key: &str) {
    layout
        .playlist_column_width_overrides
        .retain(|entry| entry.column_key != column_key);
}

/// Reorders visible columns while preserving hidden-column relative placement.
pub(crate) fn reorder_visible_playlist_columns(
    columns: &[PlaylistColumnConfig],
    from_visible_index: usize,
    to_visible_index: usize,
) -> Vec<PlaylistColumnConfig> {
    let visible_count = columns.iter().filter(|column| column.enabled).count();
    if visible_count < 2
        || from_visible_index >= visible_count
        || to_visible_index >= visible_count
        || from_visible_index == to_visible_index
    {
        return columns.to_vec();
    }

    let mut visible_columns: Vec<PlaylistColumnConfig> = columns
        .iter()
        .filter(|column| column.enabled)
        .cloned()
        .collect();
    let moved_column = visible_columns.remove(from_visible_index);
    visible_columns.insert(to_visible_index, moved_column);

    let mut visible_iter = visible_columns.into_iter();
    let mut reordered = Vec::with_capacity(columns.len());
    for column in columns {
        if column.enabled {
            if let Some(next_visible) = visible_iter.next() {
                reordered.push(next_visible);
            }
        } else {
            reordered.push(column.clone());
        }
    }
    reordered
}

/// Extracts column widths from a Slint model into a plain vector.
pub(crate) fn playlist_column_widths_from_model(widths_model: ModelRc<i32>) -> Vec<i32> {
    (0..widths_model.row_count())
        .filter_map(|index| widths_model.row_data(index))
        .collect()
}

/// Resolves the visible header column index at `mouse_x_px`, or `-1`.
pub(crate) fn resolve_playlist_header_column_from_x(mouse_x_px: i32, widths_px: &[i32]) -> i32 {
    if mouse_x_px < 0 {
        return -1;
    }
    let mut start_px: i32 = 0;
    for (index, width_px) in widths_px.iter().enumerate() {
        let column_width = (*width_px).max(0);
        let end_px = start_px.saturating_add(column_width);
        if mouse_x_px >= start_px && mouse_x_px < end_px {
            return index as i32;
        }
        let next_start = end_px.saturating_add(crate::PLAYLIST_COLUMN_SPACING_PX);
        if mouse_x_px < next_start {
            return -1;
        }
        start_px = next_start;
    }
    -1
}

/// Resolves the visible header gap index at `mouse_x_px`, or `-1`.
pub(crate) fn resolve_playlist_header_gap_from_x(mouse_x_px: i32, widths_px: &[i32]) -> i32 {
    if widths_px.is_empty() {
        return -1;
    }
    if mouse_x_px < 0 {
        return 0;
    }

    let mut start_px: i32 = 0;
    for (index, width_px) in widths_px.iter().enumerate() {
        let column_width = (*width_px).max(0);
        let end_px = start_px.saturating_add(column_width);
        let midpoint_px = start_px.saturating_add(column_width / 2);

        if mouse_x_px < start_px {
            return index as i32;
        }
        if mouse_x_px < end_px {
            return if mouse_x_px < midpoint_px {
                index as i32
            } else {
                index as i32 + 1
            };
        }

        let next_start = end_px.saturating_add(crate::PLAYLIST_COLUMN_SPACING_PX);
        if mouse_x_px < next_start {
            return index as i32 + 1;
        }
        start_px = next_start;
    }
    widths_px.len() as i32
}

/// Resolves the visible header divider index at `mouse_x_px`, or `-1`.
pub(crate) fn resolve_playlist_header_divider_from_x(
    mouse_x_px: i32,
    widths_px: &[i32],
    hit_tolerance_px: i32,
) -> i32 {
    if mouse_x_px < 0 || widths_px.is_empty() {
        return -1;
    }
    let tolerance = hit_tolerance_px.max(1);
    let mut start_px: i32 = 0;
    for (index, width_px) in widths_px.iter().enumerate() {
        let column_width = (*width_px).max(0);
        let end_px = start_px.saturating_add(column_width);
        if (mouse_x_px - end_px).abs() <= tolerance {
            return index as i32;
        }
        start_px = end_px.saturating_add(crate::PLAYLIST_COLUMN_SPACING_PX);
    }
    -1
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use slint::{Model, ModelRc, VecModel};

    use crate::config::PlaylistColumnConfig;

    use super::{
        clamp_width_for_visible_column, default_album_art_column_width_bounds,
        is_album_art_builtin_column, is_favorite_builtin_column,
        playlist_column_key_at_visible_index, playlist_column_width_bounds,
        playlist_column_width_bounds_with_album_art, playlist_column_widths_from_model,
        reorder_visible_playlist_columns, resolve_playlist_header_column_from_x,
        resolve_playlist_header_divider_from_x, resolve_playlist_header_gap_from_x,
        sanitize_playlist_columns, visible_playlist_column_kinds, ColumnWidthBounds,
    };

    #[test]
    fn test_reorder_visible_playlist_columns_preserves_hidden_slot_layout() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Track #".to_string(),
                format: "{track_number}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Hidden Custom".to_string(),
                format: "{foo}".to_string(),
                enabled: false,
                custom: true,
            },
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
        ];

        let reordered = reorder_visible_playlist_columns(&columns, 0, 2);
        assert_eq!(reordered.len(), columns.len());
        assert_eq!(reordered[1], columns[1]);
        let visible_formats: Vec<String> = reordered
            .iter()
            .filter(|column| column.enabled)
            .map(|column| column.format.clone())
            .collect();
        assert_eq!(
            visible_formats,
            vec![
                "{title}".to_string(),
                "{artist}".to_string(),
                "{track_number}".to_string(),
            ]
        );
    }

    #[test]
    fn test_playlist_column_key_at_visible_index_ignores_hidden_columns() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Hidden".to_string(),
                format: "{hidden}".to_string(),
                enabled: false,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Mood".to_string(),
                format: "{mood}".to_string(),
                enabled: true,
                custom: true,
            },
        ];

        assert_eq!(
            playlist_column_key_at_visible_index(&columns, 0).as_deref(),
            Some("{title}")
        );
        assert_eq!(
            playlist_column_key_at_visible_index(&columns, 1).as_deref(),
            Some("custom:Mood|{mood}")
        );
        assert_eq!(playlist_column_key_at_visible_index(&columns, 2), None);
    }

    #[test]
    fn test_playlist_column_width_bounds_for_track_custom_and_album_art_columns() {
        let track_column = PlaylistColumnConfig {
            name: "Track #".to_string(),
            format: "{track_number}".to_string(),
            enabled: true,
            custom: false,
        };
        let album_art_column = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_column = PlaylistColumnConfig {
            name: "Energy".to_string(),
            format: "{energy}".to_string(),
            enabled: true,
            custom: true,
        };

        let track_bounds = playlist_column_width_bounds(&track_column);
        let album_art_bounds = playlist_column_width_bounds(&album_art_column);
        let custom_bounds = playlist_column_width_bounds(&custom_column);

        assert_eq!((track_bounds.min_px, track_bounds.max_px), (52, 90));
        assert_eq!(
            (album_art_bounds.min_px, album_art_bounds.max_px),
            (16, 480)
        );
        assert_eq!((custom_bounds.min_px, custom_bounds.max_px), (110, 340));
    }

    #[test]
    fn test_playlist_column_width_bounds_for_track_details_builtin() {
        let track_details_column = PlaylistColumnConfig {
            name: "Track Details".to_string(),
            format: crate::config::BUILTIN_TRACK_DETAILS_COLUMN_FORMAT.to_string(),
            enabled: true,
            custom: false,
        };

        let bounds = playlist_column_width_bounds(&track_details_column);
        assert_eq!((bounds.min_px, bounds.max_px), (170, 460));
    }

    #[test]
    fn test_playlist_column_width_bounds_for_favorite_column_are_fixed() {
        let favorite_column = PlaylistColumnConfig {
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
            enabled: true,
            custom: false,
        };

        let bounds = playlist_column_width_bounds(&favorite_column);
        assert_eq!((bounds.min_px, bounds.max_px), (24, 24));
    }

    #[test]
    fn test_playlist_column_width_bounds_use_custom_album_art_limits() {
        let album_art_column = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let bounds = playlist_column_width_bounds_with_album_art(
            &album_art_column,
            ColumnWidthBounds {
                min_px: 24,
                max_px: 512,
            },
        );
        assert_eq!((bounds.min_px, bounds.max_px), (24, 512));
    }

    #[test]
    fn test_is_album_art_builtin_column_requires_builtin_marker() {
        let builtin_album_art = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_album_art = PlaylistColumnConfig {
            name: "Album Art".to_string(),
            format: "{album_art}".to_string(),
            enabled: true,
            custom: true,
        };
        let title_column = PlaylistColumnConfig {
            name: "Title".to_string(),
            format: "{title}".to_string(),
            enabled: true,
            custom: false,
        };

        assert!(is_album_art_builtin_column(&builtin_album_art));
        assert!(!is_album_art_builtin_column(&custom_album_art));
        assert!(!is_album_art_builtin_column(&title_column));
    }

    #[test]
    fn test_is_favorite_builtin_column_requires_builtin_marker() {
        let builtin_favorite = PlaylistColumnConfig {
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
            enabled: true,
            custom: false,
        };
        let custom_favorite = PlaylistColumnConfig {
            name: "Favorite".to_string(),
            format: "{favorite}".to_string(),
            enabled: true,
            custom: true,
        };
        let title_column = PlaylistColumnConfig {
            name: "Title".to_string(),
            format: "{title}".to_string(),
            enabled: true,
            custom: false,
        };

        assert!(is_favorite_builtin_column(&builtin_favorite));
        assert!(!is_favorite_builtin_column(&custom_favorite));
        assert!(!is_favorite_builtin_column(&title_column));
    }

    #[test]
    fn test_clamp_width_for_visible_column_respects_column_profile_bounds() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Track #".to_string(),
                format: "{track_number}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Favorite".to_string(),
                format: "{favorite}".to_string(),
                enabled: true,
                custom: false,
            },
        ];
        let album_art_bounds = default_album_art_column_width_bounds();

        assert_eq!(
            clamp_width_for_visible_column(&columns, 0, 5, album_art_bounds),
            Some(52)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 0, 500, album_art_bounds),
            Some(90)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 1, 8, album_art_bounds),
            Some(16)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 1, 640, album_art_bounds),
            Some(480)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 2, 120, album_art_bounds),
            Some(140)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 2, 720, album_art_bounds),
            Some(440)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 3, 10, album_art_bounds),
            Some(24)
        );
        assert_eq!(
            clamp_width_for_visible_column(&columns, 3, 120, album_art_bounds),
            Some(24)
        );
    }

    #[test]
    fn test_sanitize_playlist_columns_restores_builtins_and_preserves_custom() {
        let custom = PlaylistColumnConfig {
            name: "Album & Year".to_string(),
            format: "{album} ({year})".to_string(),
            enabled: true,
            custom: true,
        };
        let input_columns = vec![
            PlaylistColumnConfig {
                name: "Artist".to_string(),
                format: "{artist}".to_string(),
                enabled: false,
                custom: false,
            },
            custom.clone(),
        ];

        let sanitized = sanitize_playlist_columns(&input_columns);
        assert!(sanitized.iter().any(|column| column.format == "{title}"));
        assert!(sanitized.iter().any(|column| column.format == "{artist}"));
        assert!(sanitized
            .iter()
            .any(|column| !column.custom && column.format == "{album_art}"));
        assert!(sanitized.iter().any(|column| column == &custom));
        assert!(sanitized.iter().any(|column| column.enabled));
    }

    #[test]
    fn test_visible_playlist_column_kinds_marks_builtin_album_art_only() {
        let columns = vec![
            PlaylistColumnConfig {
                name: "Title".to_string(),
                format: "{title}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Album Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: false,
            },
            PlaylistColumnConfig {
                name: "Custom Art".to_string(),
                format: "{album_art}".to_string(),
                enabled: true,
                custom: true,
            },
        ];

        assert_eq!(visible_playlist_column_kinds(&columns), vec![0, 1, 0]);
    }

    #[test]
    fn test_sanitize_playlist_columns_preserves_existing_builtin_order() {
        let input_columns = vec![
            PlaylistColumnConfig {
                name: "Track #".to_string(),
                format: "{track_number}".to_string(),
                enabled: true,
                custom: false,
            },
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
        ];

        let sanitized = sanitize_playlist_columns(&input_columns);
        let visible_order: Vec<String> = sanitized
            .iter()
            .filter(|column| column.enabled)
            .map(|column| column.format.clone())
            .collect();
        assert_eq!(
            visible_order,
            vec![
                "{track_number}".to_string(),
                "{title}".to_string(),
                "{artist}".to_string(),
                "{album}".to_string(),
            ]
        );
    }

    #[test]
    fn test_resolve_playlist_header_column_from_x_uses_variable_widths() {
        let widths = vec![72, 184, 136];
        assert_eq!(resolve_playlist_header_column_from_x(-1, &widths), -1);
        assert_eq!(resolve_playlist_header_column_from_x(0, &widths), 0);
        assert_eq!(resolve_playlist_header_column_from_x(71, &widths), 0);
        assert_eq!(resolve_playlist_header_column_from_x(72, &widths), -1);
        assert_eq!(resolve_playlist_header_column_from_x(82, &widths), 1);
        assert_eq!(resolve_playlist_header_column_from_x(266, &widths), -1);
        assert_eq!(resolve_playlist_header_column_from_x(276, &widths), 2);
    }

    #[test]
    fn test_resolve_playlist_header_gap_from_x_tracks_real_column_edges() {
        let widths = vec![72, 184, 136];
        assert_eq!(resolve_playlist_header_gap_from_x(-5, &widths), 0);
        assert_eq!(resolve_playlist_header_gap_from_x(0, &widths), 0);
        assert_eq!(resolve_playlist_header_gap_from_x(35, &widths), 0);
        assert_eq!(resolve_playlist_header_gap_from_x(36, &widths), 1);
        assert_eq!(resolve_playlist_header_gap_from_x(81, &widths), 1);
        assert_eq!(resolve_playlist_header_gap_from_x(266, &widths), 2);
        assert_eq!(resolve_playlist_header_gap_from_x(500, &widths), 3);
    }

    #[test]
    fn test_resolve_playlist_header_divider_from_x_hits_column_boundaries() {
        let widths = vec![72, 184, 136];
        assert_eq!(resolve_playlist_header_divider_from_x(72, &widths, 2), 0);
        assert_eq!(resolve_playlist_header_divider_from_x(265, &widths, 2), 1);
        assert_eq!(resolve_playlist_header_divider_from_x(411, &widths, 2), 2);
        assert_eq!(resolve_playlist_header_divider_from_x(268, &widths, 1), -1);
    }

    #[test]
    fn test_playlist_column_width_rendering_matches_runtime_width_model() {
        let model: ModelRc<i32> = ModelRc::from(Rc::new(VecModel::from(vec![72, 184, 136])));
        let extracted = playlist_column_widths_from_model(model.clone());
        assert_eq!(extracted, vec![72, 184, 136]);

        let divider_idx = resolve_playlist_header_divider_from_x(267, &extracted, 2);
        assert_eq!(divider_idx, 1);
        let gap_idx = resolve_playlist_header_gap_from_x(267, &extracted);
        assert_eq!(gap_idx, 2);

        assert_eq!(model.row_count(), extracted.len());
    }
}
