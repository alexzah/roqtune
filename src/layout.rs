//! Docking-layout data model and geometry computations for customizable UI panels.

use std::collections::HashSet;

/// Region index for the top docking slot.
pub const REGION_TOP: usize = 0;
/// Region index for the left docking slot.
pub const REGION_LEFT: usize = 1;
/// Region index for the center docking slot.
pub const REGION_CENTER: usize = 2;
/// Region index for the right docking slot.
pub const REGION_RIGHT: usize = 3;
/// Region index for the bottom docking slot.
pub const REGION_BOTTOM: usize = 4;
/// Number of docking regions.
pub const REGION_COUNT: usize = 5;

/// Pixel thickness used for layout splitters between occupied regions.
pub const SPLITTER_THICKNESS_PX: i32 = 6;

/// User-selectable panel kind assignable to a docking region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPanelKind {
    None,
    ControlBar,
    PlaylistSwitcher,
    TrackList,
    AlbumArtPane,
    Spacer,
    StatusBar,
}

impl LayoutPanelKind {
    /// Converts this panel kind to a stable integer code consumed by Slint.
    pub fn to_code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::ControlBar => 1,
            Self::PlaylistSwitcher => 2,
            Self::TrackList => 3,
            Self::AlbumArtPane => 4,
            Self::Spacer => 5,
            Self::StatusBar => 6,
        }
    }

    /// Builds a panel kind from a Slint integer code.
    pub fn from_code(code: i32) -> Self {
        match code {
            1 => Self::ControlBar,
            2 => Self::PlaylistSwitcher,
            3 => Self::TrackList,
            4 => Self::AlbumArtPane,
            5 => Self::Spacer,
            6 => Self::StatusBar,
            _ => Self::None,
        }
    }
}

/// Persistent docking-layout preferences.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct LayoutConfig {
    /// Panel assignment for each region in `[top, left, center, right, bottom]` order.
    #[serde(default = "default_region_panels")]
    pub region_panels: [LayoutPanelKind; REGION_COUNT],
    /// Preferred top region height in logical pixels.
    #[serde(default = "default_top_size_px")]
    pub top_size_px: u32,
    /// Preferred left region width in logical pixels.
    #[serde(default = "default_left_size_px")]
    pub left_size_px: u32,
    /// Preferred right region width in logical pixels.
    #[serde(default = "default_right_size_px")]
    pub right_size_px: u32,
    /// Preferred bottom region height in logical pixels.
    #[serde(default = "default_bottom_size_px")]
    pub bottom_size_px: u32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            region_panels: default_region_panels(),
            top_size_px: default_top_size_px(),
            left_size_px: default_left_size_px(),
            right_size_px: default_right_size_px(),
            bottom_size_px: default_bottom_size_px(),
        }
    }
}

/// Runtime region lookup by concrete panel component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelRegions {
    /// Region hosting the control bar panel, `-1` when hidden.
    pub control_bar: i32,
    /// Region hosting the playlist switcher panel, `-1` when hidden.
    pub playlist_switcher: i32,
    /// Region hosting the track list panel, `-1` when hidden.
    pub track_list: i32,
    /// Region hosting the album art panel, `-1` when hidden.
    pub album_art: i32,
    /// Region hosting the status bar panel, `-1` when hidden.
    pub status_bar: i32,
}

impl Default for PanelRegions {
    fn default() -> Self {
        Self {
            control_bar: -1,
            playlist_switcher: -1,
            track_list: -1,
            album_art: -1,
            status_bar: -1,
        }
    }
}

/// Geometry and visibility for one splitter handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SplitterGeometry {
    /// Whether the splitter should be rendered and interactive.
    pub visible: bool,
    /// X coordinate in logical pixels.
    pub x: i32,
    /// Y coordinate in logical pixels.
    pub y: i32,
    /// Width in logical pixels.
    pub width: i32,
    /// Height in logical pixels.
    pub height: i32,
}

/// Computed docking layout geometry used by Slint rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutMetrics {
    /// Region `x` coordinates in logical pixels.
    pub region_x_px: [i32; REGION_COUNT],
    /// Region `y` coordinates in logical pixels.
    pub region_y_px: [i32; REGION_COUNT],
    /// Region widths in logical pixels.
    pub region_width_px: [i32; REGION_COUNT],
    /// Region heights in logical pixels.
    pub region_height_px: [i32; REGION_COUNT],
    /// Region visibility flags.
    pub region_visible: [bool; REGION_COUNT],
    /// Runtime panel-to-region lookup.
    pub panel_regions: PanelRegions,
    /// Top splitter geometry.
    pub splitter_top: SplitterGeometry,
    /// Left splitter geometry.
    pub splitter_left: SplitterGeometry,
    /// Right splitter geometry.
    pub splitter_right: SplitterGeometry,
    /// Bottom splitter geometry.
    pub splitter_bottom: SplitterGeometry,
}

fn default_region_panels() -> [LayoutPanelKind; REGION_COUNT] {
    [
        LayoutPanelKind::ControlBar,
        LayoutPanelKind::PlaylistSwitcher,
        LayoutPanelKind::TrackList,
        LayoutPanelKind::AlbumArtPane,
        LayoutPanelKind::StatusBar,
    ]
}

fn default_top_size_px() -> u32 {
    74
}

fn default_left_size_px() -> u32 {
    220
}

fn default_right_size_px() -> u32 {
    230
}

fn default_bottom_size_px() -> u32 {
    24
}

fn min_height_for_panel(panel: LayoutPanelKind, is_top_region: bool) -> u32 {
    match panel {
        LayoutPanelKind::StatusBar => 20,
        LayoutPanelKind::ControlBar => 56,
        LayoutPanelKind::None => 0,
        _ => {
            if is_top_region {
                56
            } else {
                72
            }
        }
    }
}

/// Sanitizes persisted layout configuration and resolves invalid/duplicate assignments.
pub fn sanitize_layout_config(
    config: &LayoutConfig,
    viewport_width_px: u32,
    viewport_height_px: u32,
) -> LayoutConfig {
    let mut sanitized = config.clone();

    let top_min = min_height_for_panel(sanitized.region_panels[REGION_TOP], true).max(1);
    let bottom_min = min_height_for_panel(sanitized.region_panels[REGION_BOTTOM], false).max(1);

    sanitized.top_size_px = sanitized
        .top_size_px
        .clamp(top_min, viewport_height_px.max(top_min));
    sanitized.bottom_size_px = sanitized
        .bottom_size_px
        .clamp(bottom_min, viewport_height_px.max(bottom_min));
    sanitized.left_size_px = sanitized
        .left_size_px
        .clamp(140, viewport_width_px.max(140));
    sanitized.right_size_px = sanitized
        .right_size_px
        .clamp(140, viewport_width_px.max(140));

    let mut seen_panels: HashSet<LayoutPanelKind> = HashSet::new();
    for panel in &mut sanitized.region_panels {
        if matches!(*panel, LayoutPanelKind::None | LayoutPanelKind::Spacer) {
            continue;
        }
        if !seen_panels.insert(*panel) {
            *panel = LayoutPanelKind::None;
        }
    }

    sanitized
}

fn panel_regions_from_region_panels(
    region_panels: &[LayoutPanelKind; REGION_COUNT],
) -> PanelRegions {
    let mut result = PanelRegions::default();
    for (region, panel_kind) in region_panels.iter().enumerate() {
        match panel_kind {
            LayoutPanelKind::ControlBar => result.control_bar = region as i32,
            LayoutPanelKind::PlaylistSwitcher => result.playlist_switcher = region as i32,
            LayoutPanelKind::TrackList => result.track_list = region as i32,
            LayoutPanelKind::AlbumArtPane => result.album_art = region as i32,
            LayoutPanelKind::StatusBar => result.status_bar = region as i32,
            _ => {}
        }
    }
    result
}

fn reduce_pair_for_min_center(
    first: &mut i32,
    second: &mut i32,
    center: &mut i32,
    min_center: i32,
    first_min: i32,
    second_min: i32,
) {
    if *center >= min_center {
        return;
    }

    let mut deficit = min_center - *center;

    if *second > second_min {
        let reducible = (*second - second_min).min(deficit);
        *second -= reducible;
        deficit -= reducible;
    }

    if deficit > 0 && *first > first_min {
        let reducible = (*first - first_min).min(deficit);
        *first -= reducible;
        deficit -= reducible;
    }

    *center += min_center - (*center + deficit);
}

/// Computes effective region rectangles and splitter hit areas.
pub fn compute_layout_metrics(
    layout: &LayoutConfig,
    viewport_width_px: u32,
    viewport_height_px: u32,
) -> LayoutMetrics {
    let viewport_width = viewport_width_px.max(1) as i32;
    let viewport_height = viewport_height_px.max(1) as i32;

    let has_top = layout.region_panels[REGION_TOP] != LayoutPanelKind::None;
    let has_left = layout.region_panels[REGION_LEFT] != LayoutPanelKind::None;
    let has_center = layout.region_panels[REGION_CENTER] != LayoutPanelKind::None;
    let has_right = layout.region_panels[REGION_RIGHT] != LayoutPanelKind::None;
    let has_bottom = layout.region_panels[REGION_BOTTOM] != LayoutPanelKind::None;

    let middle_has_content = has_left || has_center || has_right;

    let top_min_h = min_height_for_panel(layout.region_panels[REGION_TOP], true) as i32;
    let bottom_min_h = min_height_for_panel(layout.region_panels[REGION_BOTTOM], false) as i32;

    let mut top_h = if has_top {
        (layout.top_size_px as i32).clamp(top_min_h, viewport_height)
    } else {
        0
    };
    let mut bottom_h = if has_bottom {
        (layout.bottom_size_px as i32).clamp(bottom_min_h, viewport_height)
    } else {
        0
    };

    let top_split = if has_top && middle_has_content {
        SPLITTER_THICKNESS_PX
    } else {
        0
    };
    let bottom_split = if has_bottom && middle_has_content {
        SPLITTER_THICKNESS_PX
    } else {
        0
    };

    let mut middle_h = viewport_height - top_h - bottom_h - top_split - bottom_split;
    if middle_has_content {
        let min_middle_h = if has_center { 140 } else { 96 };
        if middle_h < min_middle_h {
            reduce_pair_for_min_center(
                &mut top_h,
                &mut bottom_h,
                &mut middle_h,
                min_middle_h,
                if has_top { top_min_h } else { 0 },
                if has_bottom { bottom_min_h } else { 0 },
            );
        }
    }
    middle_h = middle_h.max(0);

    let mut left_w = if has_left {
        (layout.left_size_px as i32).clamp(140, viewport_width)
    } else {
        0
    };
    let mut right_w = if has_right {
        (layout.right_size_px as i32).clamp(140, viewport_width)
    } else {
        0
    };

    let left_split = if has_left && (has_center || has_right) {
        SPLITTER_THICKNESS_PX
    } else {
        0
    };
    let right_split = if has_right && (has_center || has_left) {
        SPLITTER_THICKNESS_PX
    } else {
        0
    };

    let mut center_w = viewport_width - left_w - right_w - left_split - right_split;
    if has_center && center_w < 220 {
        reduce_pair_for_min_center(
            &mut left_w,
            &mut right_w,
            &mut center_w,
            220,
            if has_left { 140 } else { 0 },
            if has_right { 140 } else { 0 },
        );
    }
    center_w = center_w.max(0);

    let mut region_x_px = [0; REGION_COUNT];
    let mut region_y_px = [0; REGION_COUNT];
    let mut region_width_px = [0; REGION_COUNT];
    let mut region_height_px = [0; REGION_COUNT];
    let mut region_visible = [false; REGION_COUNT];

    if has_top && top_h > 0 {
        region_x_px[REGION_TOP] = 0;
        region_y_px[REGION_TOP] = 0;
        region_width_px[REGION_TOP] = viewport_width;
        region_height_px[REGION_TOP] = top_h;
        region_visible[REGION_TOP] = true;
    }

    let middle_y = top_h + top_split;

    if has_left && left_w > 0 && middle_h > 0 {
        region_x_px[REGION_LEFT] = 0;
        region_y_px[REGION_LEFT] = middle_y;
        region_width_px[REGION_LEFT] = left_w;
        region_height_px[REGION_LEFT] = middle_h;
        region_visible[REGION_LEFT] = true;
    }

    if has_center && center_w > 0 && middle_h > 0 {
        region_x_px[REGION_CENTER] = left_w + left_split;
        region_y_px[REGION_CENTER] = middle_y;
        region_width_px[REGION_CENTER] = center_w;
        region_height_px[REGION_CENTER] = middle_h;
        region_visible[REGION_CENTER] = true;
    }

    if has_right && right_w > 0 && middle_h > 0 {
        region_x_px[REGION_RIGHT] = viewport_width - right_w;
        region_y_px[REGION_RIGHT] = middle_y;
        region_width_px[REGION_RIGHT] = right_w;
        region_height_px[REGION_RIGHT] = middle_h;
        region_visible[REGION_RIGHT] = true;
    }

    if has_bottom && bottom_h > 0 {
        region_x_px[REGION_BOTTOM] = 0;
        region_y_px[REGION_BOTTOM] = viewport_height - bottom_h;
        region_width_px[REGION_BOTTOM] = viewport_width;
        region_height_px[REGION_BOTTOM] = bottom_h;
        region_visible[REGION_BOTTOM] = true;
    }

    let splitter_top = if top_split > 0 {
        SplitterGeometry {
            visible: true,
            x: 0,
            y: top_h,
            width: viewport_width,
            height: top_split,
        }
    } else {
        SplitterGeometry::default()
    };

    let splitter_bottom = if bottom_split > 0 {
        SplitterGeometry {
            visible: true,
            x: 0,
            y: viewport_height - bottom_h - bottom_split,
            width: viewport_width,
            height: bottom_split,
        }
    } else {
        SplitterGeometry::default()
    };

    let splitter_left = if left_split > 0 && middle_h > 0 {
        SplitterGeometry {
            visible: true,
            x: left_w,
            y: middle_y,
            width: left_split,
            height: middle_h,
        }
    } else {
        SplitterGeometry::default()
    };

    let splitter_right = if right_split > 0 && middle_h > 0 {
        SplitterGeometry {
            visible: true,
            x: viewport_width - right_w - right_split,
            y: middle_y,
            width: right_split,
            height: middle_h,
        }
    } else {
        SplitterGeometry::default()
    };

    LayoutMetrics {
        region_x_px,
        region_y_px,
        region_width_px,
        region_height_px,
        region_visible,
        panel_regions: panel_regions_from_region_panels(&layout.region_panels),
        splitter_top,
        splitter_left,
        splitter_right,
        splitter_bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compute_layout_metrics, sanitize_layout_config, LayoutConfig, LayoutPanelKind,
        REGION_BOTTOM, REGION_CENTER, REGION_LEFT, REGION_TOP,
    };

    #[test]
    fn sanitize_layout_config_deduplicates_panels_and_clamps_sizes() {
        let config = LayoutConfig {
            region_panels: [
                LayoutPanelKind::ControlBar,
                LayoutPanelKind::TrackList,
                LayoutPanelKind::TrackList,
                LayoutPanelKind::AlbumArtPane,
                LayoutPanelKind::Spacer,
            ],
            top_size_px: 10,
            left_size_px: 20,
            right_size_px: 20_000,
            bottom_size_px: 20_000,
        };

        let sanitized = sanitize_layout_config(&config, 900, 650);
        assert_eq!(
            sanitized.region_panels[REGION_CENTER],
            LayoutPanelKind::None
        );
        assert_eq!(sanitized.top_size_px, 56);
        assert_eq!(sanitized.left_size_px, 140);
        assert_eq!(sanitized.right_size_px, 900);
        assert_eq!(sanitized.bottom_size_px, 650);
    }

    #[test]
    fn compute_layout_metrics_produces_non_negative_center_region() {
        let config = LayoutConfig::default();
        let metrics = compute_layout_metrics(&config, 900, 650);

        assert!(metrics.region_visible[REGION_TOP]);
        assert!(metrics.region_visible[REGION_LEFT]);
        assert!(metrics.region_visible[REGION_CENTER]);
        assert!(metrics.region_visible[REGION_BOTTOM]);
        assert!(metrics.region_width_px[REGION_CENTER] > 0);
        assert!(metrics.region_height_px[REGION_CENTER] > 0);
        assert_eq!(metrics.panel_regions.status_bar, REGION_BOTTOM as i32);
    }
}
