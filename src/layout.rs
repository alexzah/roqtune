//! Split-tree layout data model, migration, actions, and geometry.

use crate::config::{
    default_playlist_album_art_column_max_width_px, default_playlist_album_art_column_min_width_px,
    default_playlist_columns, PlaylistColumnConfig,
};
use std::collections::HashSet;

/// Current persisted layout schema version.
pub const LAYOUT_VERSION: u32 = 1;
/// Divider thickness used between split-tree children.
pub const SPLITTER_THICKNESS_PX: i32 = 2;
/// Stable panel kind code for `LayoutPanelKind::None`.
pub const PANEL_CODE_NONE: i32 = 0;
/// Stable panel kind code for `LayoutPanelKind::ButtonCluster`.
pub const PANEL_CODE_BUTTON_CLUSTER: i32 = 1;
/// Stable panel kind code for `LayoutPanelKind::TransportButtonCluster`.
pub const PANEL_CODE_TRANSPORT_BUTTON_CLUSTER: i32 = 2;
/// Stable panel kind code for `LayoutPanelKind::UtilityButtonCluster`.
pub const PANEL_CODE_UTILITY_BUTTON_CLUSTER: i32 = 3;
/// Stable panel kind code for `LayoutPanelKind::VolumeSlider`.
pub const PANEL_CODE_VOLUME_SLIDER: i32 = 4;
/// Stable panel kind code for `LayoutPanelKind::SeekBar`.
pub const PANEL_CODE_SEEK_BAR: i32 = 5;
/// Stable panel kind code for `LayoutPanelKind::PlaylistSwitcher`.
pub const PANEL_CODE_PLAYLIST_SWITCHER: i32 = 6;
/// Stable panel kind code for `LayoutPanelKind::TrackList`.
pub const PANEL_CODE_TRACK_LIST: i32 = 7;
/// Stable panel kind code for `LayoutPanelKind::MetadataViewer`.
pub const PANEL_CODE_METADATA_VIEWER: i32 = 8;
/// Stable panel kind code for `LayoutPanelKind::AlbumArtViewer`.
pub const PANEL_CODE_ALBUM_ART_VIEWER: i32 = 9;
/// Stable panel kind code for `LayoutPanelKind::Spacer`.
pub const PANEL_CODE_SPACER: i32 = 10;
/// Stable panel kind code for `LayoutPanelKind::StatusBar`.
pub const PANEL_CODE_STATUS_BAR: i32 = 11;
/// Stable panel kind code for `LayoutPanelKind::ImportButtonCluster`.
pub const PANEL_CODE_IMPORT_BUTTON_CLUSTER: i32 = 12;

/// User-selectable panel kind assignable to a layout leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPanelKind {
    None,
    ButtonCluster,
    ImportButtonCluster,
    TransportButtonCluster,
    UtilityButtonCluster,
    VolumeSlider,
    SeekBar,
    PlaylistSwitcher,
    TrackList,
    MetadataViewer,
    AlbumArtViewer,
    Spacer,
    StatusBar,
    ControlBar,
    AlbumArtPane,
}

impl LayoutPanelKind {
    /// Converts this panel kind to a stable integer code consumed by Slint.
    pub fn to_code(self) -> i32 {
        match self {
            Self::None => PANEL_CODE_NONE,
            Self::ButtonCluster => PANEL_CODE_BUTTON_CLUSTER,
            Self::TransportButtonCluster => PANEL_CODE_TRANSPORT_BUTTON_CLUSTER,
            Self::UtilityButtonCluster => PANEL_CODE_UTILITY_BUTTON_CLUSTER,
            Self::VolumeSlider => PANEL_CODE_VOLUME_SLIDER,
            Self::SeekBar => PANEL_CODE_SEEK_BAR,
            Self::PlaylistSwitcher => PANEL_CODE_PLAYLIST_SWITCHER,
            Self::TrackList => PANEL_CODE_TRACK_LIST,
            Self::MetadataViewer => PANEL_CODE_METADATA_VIEWER,
            Self::AlbumArtViewer => PANEL_CODE_ALBUM_ART_VIEWER,
            Self::Spacer => PANEL_CODE_SPACER,
            Self::StatusBar => PANEL_CODE_STATUS_BAR,
            Self::ImportButtonCluster => PANEL_CODE_IMPORT_BUTTON_CLUSTER,
            Self::ControlBar => PANEL_CODE_TRANSPORT_BUTTON_CLUSTER,
            Self::AlbumArtPane => PANEL_CODE_ALBUM_ART_VIEWER,
        }
    }

    /// Builds a panel kind from a Slint integer code.
    pub fn from_code(code: i32) -> Self {
        match code {
            PANEL_CODE_BUTTON_CLUSTER => Self::ButtonCluster,
            PANEL_CODE_TRANSPORT_BUTTON_CLUSTER => Self::TransportButtonCluster,
            PANEL_CODE_UTILITY_BUTTON_CLUSTER => Self::UtilityButtonCluster,
            PANEL_CODE_VOLUME_SLIDER => Self::VolumeSlider,
            PANEL_CODE_SEEK_BAR => Self::SeekBar,
            PANEL_CODE_PLAYLIST_SWITCHER => Self::PlaylistSwitcher,
            PANEL_CODE_TRACK_LIST => Self::TrackList,
            PANEL_CODE_METADATA_VIEWER => Self::MetadataViewer,
            PANEL_CODE_ALBUM_ART_VIEWER => Self::AlbumArtViewer,
            PANEL_CODE_SPACER => Self::Spacer,
            PANEL_CODE_STATUS_BAR => Self::StatusBar,
            PANEL_CODE_IMPORT_BUTTON_CLUSTER => Self::ImportButtonCluster,
            _ => Self::None,
        }
    }

    fn min_size_px(self) -> (u32, u32) {
        match self {
            Self::ButtonCluster => (32, 32),
            Self::ImportButtonCluster => (32, 32),
            Self::TransportButtonCluster => (180, 32),
            Self::UtilityButtonCluster => (150, 32),
            Self::VolumeSlider => (110, 26),
            Self::SeekBar => (220, 32),
            Self::PlaylistSwitcher => (140, 120),
            Self::TrackList => (280, 120),
            Self::MetadataViewer => (160, 80),
            Self::AlbumArtViewer => (160, 120),
            Self::StatusBar => (220, 20),
            Self::Spacer => (24, 24),
            Self::None => (0, 0),
            Self::ControlBar => (420, 56),
            Self::AlbumArtPane => (160, 120),
        }
    }
}

/// Split orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

impl SplitAxis {
    /// Converts this axis to a stable integer code consumed by Slint.
    pub fn to_code(self) -> i32 {
        match self {
            Self::Horizontal => 0,
            Self::Vertical => 1,
        }
    }
}

/// One node in the recursive split-tree.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "node_type", rename_all = "snake_case")]
pub enum LayoutNode {
    Empty,
    Leaf {
        id: String,
        panel: LayoutPanelKind,
    },
    Split {
        id: String,
        axis: SplitAxis,
        ratio: f32,
        min_first_px: u32,
        min_second_px: u32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// Persistent layout preferences.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LayoutConfig {
    /// Layout schema version.
    pub version: u32,
    /// Built-in Album Art playlist-column width minimum.
    pub playlist_album_art_column_min_width_px: u32,
    /// Built-in Album Art playlist-column width maximum.
    pub playlist_album_art_column_max_width_px: u32,
    /// Ordered playlist column list (built-ins + custom columns).
    pub playlist_columns: Vec<PlaylistColumnConfig>,
    /// Root node of the split-tree.
    pub root: LayoutNode,
    /// Runtime-only leaf selection used in edit mode.
    #[serde(skip)]
    pub selected_leaf_id: Option<String>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px: default_playlist_album_art_column_min_width_px(
            ),
            playlist_album_art_column_max_width_px: default_playlist_album_art_column_max_width_px(
            ),
            playlist_columns: default_playlist_columns(),
            root: build_default_layout_root(),
            selected_leaf_id: None,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum LayoutConfigWire {
    V2(LayoutConfigV2Wire),
    Legacy(LegacyLayoutConfig),
}

#[derive(Debug, serde::Deserialize)]
struct LayoutConfigV2Wire {
    #[serde(default = "default_layout_version", rename = "version")]
    _version: u32,
    #[serde(default = "default_playlist_album_art_column_min_width_px")]
    playlist_album_art_column_min_width_px: u32,
    #[serde(default = "default_playlist_album_art_column_max_width_px")]
    playlist_album_art_column_max_width_px: u32,
    #[serde(default = "default_playlist_columns")]
    playlist_columns: Vec<PlaylistColumnConfig>,
    #[serde(default = "default_layout_root_for_wire")]
    root: LayoutNode,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LegacyLayoutConfig {
    #[serde(default = "default_region_panels")]
    region_panels: [LayoutPanelKind; 5],
    #[serde(default = "legacy_default_top_size_px")]
    top_size_px: u32,
    #[serde(default = "legacy_default_left_size_px")]
    left_size_px: u32,
    #[serde(default = "legacy_default_right_size_px")]
    right_size_px: u32,
    #[serde(default = "legacy_default_bottom_size_px")]
    bottom_size_px: u32,
}

impl<'de> serde::Deserialize<'de> for LayoutConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = LayoutConfigWire::deserialize(deserializer)?;
        let config = match wire {
            LayoutConfigWire::V2(v2) => LayoutConfig {
                version: LAYOUT_VERSION,
                playlist_album_art_column_min_width_px: v2.playlist_album_art_column_min_width_px,
                playlist_album_art_column_max_width_px: v2.playlist_album_art_column_max_width_px,
                playlist_columns: v2.playlist_columns,
                root: v2.root,
                selected_leaf_id: None,
            },
            LayoutConfigWire::Legacy(legacy) => migrate_legacy_layout(&legacy),
        };
        Ok(config)
    }
}

/// One flattened leaf placement used by Slint.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutLeafItem {
    pub id: String,
    pub panel: LayoutPanelKind,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One flattened splitter placement used by Slint.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutSplitterItem {
    pub id: String,
    pub axis: SplitAxis,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub ratio: f32,
    pub min_ratio: f32,
    pub max_ratio: f32,
    pub track_start_px: i32,
    pub track_length_px: i32,
}

/// Flattened layout geometry output for rendering/editing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TreeLayoutMetrics {
    pub leaves: Vec<LayoutLeafItem>,
    pub splitters: Vec<LayoutSplitterItem>,
}

fn default_layout_version() -> u32 {
    LAYOUT_VERSION
}

fn default_layout_root_for_wire() -> LayoutNode {
    build_default_layout_root()
}

fn legacy_default_top_size_px() -> u32 {
    74
}

fn legacy_default_left_size_px() -> u32 {
    220
}

fn legacy_default_right_size_px() -> u32 {
    230
}

fn legacy_default_bottom_size_px() -> u32 {
    24
}

fn default_region_panels() -> [LayoutPanelKind; 5] {
    [
        LayoutPanelKind::ControlBar,
        LayoutPanelKind::PlaylistSwitcher,
        LayoutPanelKind::TrackList,
        LayoutPanelKind::AlbumArtPane,
        LayoutPanelKind::StatusBar,
    ]
}

fn default_control_stack_subtree(next: &mut dyn FnMut() -> String) -> LayoutNode {
    let left_import = LayoutNode::Leaf {
        id: next(),
        panel: LayoutPanelKind::ButtonCluster,
    };
    let center_transport = LayoutNode::Leaf {
        id: next(),
        panel: LayoutPanelKind::ButtonCluster,
    };
    let right_utility = LayoutNode::Leaf {
        id: next(),
        panel: LayoutPanelKind::ButtonCluster,
    };
    let right_volume = LayoutNode::Leaf {
        id: next(),
        panel: LayoutPanelKind::VolumeSlider,
    };
    let seek = LayoutNode::Leaf {
        id: next(),
        panel: LayoutPanelKind::SeekBar,
    };

    let utility_volume = LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Vertical,
        ratio: 0.70,
        min_first_px: 120,
        min_second_px: 90,
        first: Box::new(right_utility),
        second: Box::new(right_volume),
    };
    let transport_right = LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Vertical,
        ratio: 0.62,
        min_first_px: 180,
        min_second_px: 110,
        first: Box::new(center_transport),
        second: Box::new(utility_volume),
    };
    let top_row = LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Vertical,
        ratio: 0.08,
        min_first_px: 32,
        min_second_px: 240,
        first: Box::new(left_import),
        second: Box::new(transport_right),
    };
    LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Horizontal,
        ratio: 0.62,
        min_first_px: 32,
        min_second_px: 28,
        first: Box::new(top_row),
        second: Box::new(seek),
    }
}

fn default_album_subtree(next: &mut dyn FnMut() -> String) -> LayoutNode {
    LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Horizontal,
        ratio: 0.32,
        min_first_px: 72,
        min_second_px: 120,
        first: Box::new(LayoutNode::Leaf {
            id: next(),
            panel: LayoutPanelKind::MetadataViewer,
        }),
        second: Box::new(LayoutNode::Leaf {
            id: next(),
            panel: LayoutPanelKind::AlbumArtViewer,
        }),
    }
}

fn build_default_layout_root() -> LayoutNode {
    let mut id = 1u32;
    let mut next = || {
        let current = id;
        id = id.saturating_add(1);
        format!("n{}", current)
    };

    let album = default_album_subtree(&mut next);

    let middle = LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Vertical,
        ratio: 0.28,
        min_first_px: 140,
        min_second_px: 220,
        first: Box::new(LayoutNode::Leaf {
            id: next(),
            panel: LayoutPanelKind::PlaylistSwitcher,
        }),
        second: Box::new(LayoutNode::Split {
            id: next(),
            axis: SplitAxis::Vertical,
            ratio: 0.68,
            min_first_px: 220,
            min_second_px: 140,
            first: Box::new(LayoutNode::Leaf {
                id: next(),
                panel: LayoutPanelKind::TrackList,
            }),
            second: Box::new(album),
        }),
    };

    let controls = default_control_stack_subtree(&mut next);

    LayoutNode::Split {
        id: next(),
        axis: SplitAxis::Horizontal,
        ratio: 0.16,
        min_first_px: 64,
        min_second_px: 120,
        first: Box::new(controls),
        second: Box::new(LayoutNode::Split {
            id: next(),
            axis: SplitAxis::Horizontal,
            ratio: 0.96,
            min_first_px: 120,
            min_second_px: 20,
            first: Box::new(middle),
            second: Box::new(LayoutNode::Leaf {
                id: next(),
                panel: LayoutPanelKind::StatusBar,
            }),
        }),
    }
}

fn migrate_legacy_layout(legacy: &LegacyLayoutConfig) -> LayoutConfig {
    let mut id_counter: u32 = 1;
    let mut next_id = || {
        let id = format!("m{}", id_counter);
        id_counter = id_counter.saturating_add(1);
        id
    };
    let leaf = |panel: LayoutPanelKind, next_id: &mut dyn FnMut() -> String| -> LayoutNode {
        if panel == LayoutPanelKind::None {
            LayoutNode::Empty
        } else {
            LayoutNode::Leaf {
                id: next_id(),
                panel,
            }
        }
    };
    let combine = |axis: SplitAxis,
                   ratio: f32,
                   first: LayoutNode,
                   second: LayoutNode,
                   next_id: &mut dyn FnMut() -> String| {
        match (&first, &second) {
            (LayoutNode::Empty, LayoutNode::Empty) => LayoutNode::Empty,
            (LayoutNode::Empty, _) => second,
            (_, LayoutNode::Empty) => first,
            _ => LayoutNode::Split {
                id: next_id(),
                axis,
                ratio: ratio.clamp(0.05, 0.95),
                min_first_px: 24,
                min_second_px: 24,
                first: Box::new(first),
                second: Box::new(second),
            },
        }
    };

    let top = leaf(legacy.region_panels[0], &mut next_id);
    let left = leaf(legacy.region_panels[1], &mut next_id);
    let center = leaf(legacy.region_panels[2], &mut next_id);
    let right = leaf(legacy.region_panels[3], &mut next_id);
    let bottom = leaf(legacy.region_panels[4], &mut next_id);

    let middle_right_total = ((900u32
        .saturating_sub(legacy.left_size_px)
        .saturating_sub(SPLITTER_THICKNESS_PX as u32))
    .max(200)) as f32;
    let center_width = middle_right_total - legacy.right_size_px as f32;
    let center_ratio = if middle_right_total > 0.0 {
        (center_width / middle_right_total).clamp(0.05, 0.95)
    } else {
        0.67
    };
    let center_right = combine(
        SplitAxis::Vertical,
        center_ratio,
        center,
        right,
        &mut next_id,
    );

    let middle_total = (900u32.saturating_sub(SPLITTER_THICKNESS_PX as u32)).max(200) as f32;
    let left_ratio = if middle_total > 0.0 {
        (legacy.left_size_px as f32 / middle_total).clamp(0.05, 0.95)
    } else {
        0.28
    };
    let middle = combine(
        SplitAxis::Vertical,
        left_ratio,
        left,
        center_right,
        &mut next_id,
    );

    let upper_total = ((650u32
        .saturating_sub(legacy.bottom_size_px)
        .saturating_sub(SPLITTER_THICKNESS_PX as u32))
    .max(120)) as f32;
    let top_ratio = if upper_total > 0.0 {
        (legacy.top_size_px as f32 / upper_total).clamp(0.05, 0.95)
    } else {
        0.12
    };
    let top_middle = combine(SplitAxis::Horizontal, top_ratio, top, middle, &mut next_id);

    let bottom_total = 650f32.max(120.0);
    let upper_ratio =
        ((bottom_total - legacy.bottom_size_px as f32) / bottom_total).clamp(0.05, 0.95);
    let root = combine(
        SplitAxis::Horizontal,
        upper_ratio,
        top_middle,
        bottom,
        &mut next_id,
    );

    LayoutConfig {
        version: LAYOUT_VERSION,
        playlist_album_art_column_min_width_px: default_playlist_album_art_column_min_width_px(),
        playlist_album_art_column_max_width_px: default_playlist_album_art_column_max_width_px(),
        playlist_columns: default_playlist_columns(),
        root,
        selected_leaf_id: None,
    }
}

struct IdGenerator {
    next: u64,
    used: HashSet<String>,
}

impl IdGenerator {
    fn from_root(root: &LayoutNode) -> Self {
        let mut used = HashSet::new();
        collect_ids(root, &mut used);
        Self { next: 1, used }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        loop {
            let candidate = format!("{}{}", prefix, self.next);
            self.next = self.next.saturating_add(1);
            if self.used.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
}

fn collect_ids(node: &LayoutNode, out: &mut HashSet<String>) {
    match node {
        LayoutNode::Empty => {}
        LayoutNode::Leaf { id, .. } | LayoutNode::Split { id, .. } => {
            out.insert(id.clone());
        }
    }
    if let LayoutNode::Split { first, second, .. } = node {
        collect_ids(first, out);
        collect_ids(second, out);
    }
}

fn min_size_for_node(node: &LayoutNode, splitter_thickness_px: i32) -> (u32, u32) {
    match node {
        LayoutNode::Empty => (0, 0),
        LayoutNode::Leaf { panel, .. } => panel.min_size_px(),
        LayoutNode::Split {
            axis,
            min_first_px,
            min_second_px,
            first,
            second,
            ..
        } => {
            let (first_min_w, first_min_h) = min_size_for_node(first, splitter_thickness_px);
            let (second_min_w, second_min_h) = min_size_for_node(second, splitter_thickness_px);
            let splitter = splitter_thickness_px.max(0) as u32;
            match axis {
                SplitAxis::Vertical => (
                    first_min_w
                        .max(*min_first_px)
                        .saturating_add(second_min_w.max(*min_second_px))
                        .saturating_add(splitter),
                    first_min_h.max(second_min_h),
                ),
                SplitAxis::Horizontal => (
                    first_min_w.max(second_min_w),
                    first_min_h
                        .max(*min_first_px)
                        .saturating_add(second_min_h.max(*min_second_px))
                        .saturating_add(splitter),
                ),
            }
        }
    }
}

fn compact_node(node: LayoutNode) -> LayoutNode {
    match node {
        LayoutNode::Split {
            id,
            axis,
            ratio,
            min_first_px,
            min_second_px,
            first,
            second,
        } => {
            let first = compact_node(*first);
            let second = compact_node(*second);
            match (&first, &second) {
                (LayoutNode::Empty, LayoutNode::Empty) => LayoutNode::Empty,
                (LayoutNode::Empty, _) => second,
                (_, LayoutNode::Empty) => first,
                _ => LayoutNode::Split {
                    id,
                    axis,
                    ratio,
                    min_first_px,
                    min_second_px,
                    first: Box::new(first),
                    second: Box::new(second),
                },
            }
        }
        other => other,
    }
}

fn normalize_node(
    node: LayoutNode,
    id_gen: &mut IdGenerator,
    valid_ids: &mut HashSet<String>,
) -> LayoutNode {
    match node {
        LayoutNode::Empty => LayoutNode::Empty,
        LayoutNode::Leaf { mut id, panel } => {
            let mapped_panel = match panel {
                LayoutPanelKind::ControlBar => {
                    let mut next = || id_gen.next_id("l");
                    return normalize_node(
                        default_control_stack_subtree(&mut next),
                        id_gen,
                        valid_ids,
                    );
                }
                LayoutPanelKind::AlbumArtPane => {
                    let mut next = || id_gen.next_id("l");
                    return normalize_node(default_album_subtree(&mut next), id_gen, valid_ids);
                }
                LayoutPanelKind::ImportButtonCluster
                | LayoutPanelKind::TransportButtonCluster
                | LayoutPanelKind::UtilityButtonCluster => LayoutPanelKind::ButtonCluster,
                other => other,
            };
            if mapped_panel == LayoutPanelKind::None {
                return LayoutNode::Empty;
            }
            if id.trim().is_empty() || !valid_ids.insert(id.clone()) {
                id = id_gen.next_id("l");
                valid_ids.insert(id.clone());
            }
            LayoutNode::Leaf {
                id,
                panel: mapped_panel,
            }
        }
        LayoutNode::Split {
            mut id,
            axis,
            ratio,
            min_first_px,
            min_second_px,
            first,
            second,
        } => {
            let first = normalize_node(*first, id_gen, valid_ids);
            let second = normalize_node(*second, id_gen, valid_ids);
            let normalized = LayoutNode::Split {
                id: {
                    if id.trim().is_empty() || !valid_ids.insert(id.clone()) {
                        id = id_gen.next_id("s");
                        valid_ids.insert(id.clone());
                    }
                    id
                },
                axis,
                ratio: if ratio.is_finite() {
                    ratio.clamp(0.05, 0.95)
                } else {
                    0.5
                },
                min_first_px: min_first_px.max(16),
                min_second_px: min_second_px.max(16),
                first: Box::new(first),
                second: Box::new(second),
            };
            compact_node(normalized)
        }
    }
}

/// Sanitizes persisted layout configuration and resolves invalid nodes.
pub fn sanitize_layout_config(
    config: &LayoutConfig,
    _viewport_width_px: u32,
    _viewport_height_px: u32,
) -> LayoutConfig {
    let mut id_gen = IdGenerator::from_root(&config.root);
    let mut valid_ids = HashSet::new();
    let root = normalize_node(config.root.clone(), &mut id_gen, &mut valid_ids);
    let selected = config
        .selected_leaf_id
        .as_ref()
        .and_then(|candidate| find_leaf_id(&root, candidate).map(|_| candidate.clone()));
    LayoutConfig {
        version: LAYOUT_VERSION,
        playlist_album_art_column_min_width_px: config.playlist_album_art_column_min_width_px,
        playlist_album_art_column_max_width_px: config.playlist_album_art_column_max_width_px,
        playlist_columns: config.playlist_columns.clone(),
        root,
        selected_leaf_id: selected,
    }
}

fn find_leaf_id<'a>(node: &'a LayoutNode, leaf_id: &str) -> Option<&'a LayoutNode> {
    match node {
        LayoutNode::Leaf { id, .. } if id == leaf_id => Some(node),
        LayoutNode::Split { first, second, .. } => {
            find_leaf_id(first, leaf_id).or_else(|| find_leaf_id(second, leaf_id))
        }
        _ => None,
    }
}

fn panel_for_leaf_id(node: &LayoutNode, leaf_id: &str) -> Option<LayoutPanelKind> {
    match node {
        LayoutNode::Leaf { id, panel } if id == leaf_id => Some(*panel),
        LayoutNode::Split { first, second, .. } => {
            panel_for_leaf_id(first, leaf_id).or_else(|| panel_for_leaf_id(second, leaf_id))
        }
        _ => None,
    }
}

fn replace_leaf_panel_mut(node: &mut LayoutNode, leaf_id: &str, panel: LayoutPanelKind) -> bool {
    match node {
        LayoutNode::Leaf {
            id,
            panel: existing_panel,
        } if id == leaf_id => {
            *existing_panel = panel;
            true
        }
        LayoutNode::Split { first, second, .. } => {
            replace_leaf_panel_mut(first, leaf_id, panel)
                || replace_leaf_panel_mut(second, leaf_id, panel)
        }
        _ => false,
    }
}

/// Replaces one leaf panel. Returns `None` when the action is invalid.
pub fn replace_leaf_panel(
    layout: &LayoutConfig,
    leaf_id: &str,
    panel: LayoutPanelKind,
) -> Option<LayoutConfig> {
    if panel == LayoutPanelKind::None {
        return delete_leaf(layout, leaf_id);
    }
    let existing_panel = panel_for_leaf_id(&layout.root, leaf_id)?;
    if existing_panel == panel {
        return Some(sanitize_layout_config(layout, 0, 0));
    }

    let mut next = layout.clone();
    if !replace_leaf_panel_mut(&mut next.root, leaf_id, panel) {
        return None;
    }

    Some(sanitize_layout_config(&next, 0, 0))
}

fn split_leaf_mut(
    node: &mut LayoutNode,
    leaf_id: &str,
    axis: SplitAxis,
    new_panel: LayoutPanelKind,
    id_gen: &mut IdGenerator,
) -> bool {
    match node {
        LayoutNode::Leaf { id, .. } if id == leaf_id => {
            let existing = node.clone();
            let new_sibling = if new_panel == LayoutPanelKind::None {
                LayoutNode::Empty
            } else {
                LayoutNode::Leaf {
                    id: id_gen.next_id("l"),
                    panel: new_panel,
                }
            };
            *node = LayoutNode::Split {
                id: id_gen.next_id("s"),
                axis,
                ratio: 0.5,
                min_first_px: 24,
                min_second_px: 24,
                first: Box::new(existing),
                second: Box::new(new_sibling),
            };
            true
        }
        LayoutNode::Split { first, second, .. } => {
            split_leaf_mut(first, leaf_id, axis, new_panel, id_gen)
                || split_leaf_mut(second, leaf_id, axis, new_panel, id_gen)
        }
        _ => false,
    }
}

/// Splits one leaf into two children.
pub fn split_leaf(
    layout: &LayoutConfig,
    leaf_id: &str,
    axis: SplitAxis,
    new_panel: LayoutPanelKind,
) -> Option<LayoutConfig> {
    let mut next = layout.clone();
    let mut id_gen = IdGenerator::from_root(&layout.root);
    if !split_leaf_mut(&mut next.root, leaf_id, axis, new_panel, &mut id_gen) {
        return None;
    }
    Some(sanitize_layout_config(&next, 0, 0))
}

fn delete_leaf_mut(node: &mut LayoutNode, leaf_id: &str) -> bool {
    match node {
        LayoutNode::Leaf { id, .. } if id == leaf_id => {
            *node = LayoutNode::Empty;
            true
        }
        LayoutNode::Split { first, second, .. } => {
            delete_leaf_mut(first, leaf_id) || delete_leaf_mut(second, leaf_id)
        }
        _ => false,
    }
}

/// Deletes one leaf from the tree.
pub fn delete_leaf(layout: &LayoutConfig, leaf_id: &str) -> Option<LayoutConfig> {
    let mut next = layout.clone();
    if !delete_leaf_mut(&mut next.root, leaf_id) {
        return None;
    }
    next.root = compact_node(next.root);
    Some(sanitize_layout_config(&next, 0, 0))
}

fn set_split_ratio_mut(node: &mut LayoutNode, split_id: &str, ratio: f32) -> bool {
    match node {
        LayoutNode::Split {
            id,
            ratio: split_ratio,
            first,
            second,
            ..
        } => {
            if id == split_id {
                *split_ratio = if ratio.is_finite() {
                    ratio.clamp(0.01, 0.99)
                } else {
                    *split_ratio
                };
                true
            } else {
                set_split_ratio_mut(first, split_id, ratio)
                    || set_split_ratio_mut(second, split_id, ratio)
            }
        }
        _ => false,
    }
}

/// Sets one splitter ratio by id.
pub fn set_split_ratio(layout: &LayoutConfig, split_id: &str, ratio: f32) -> Option<LayoutConfig> {
    let mut next = layout.clone();
    if !set_split_ratio_mut(&mut next.root, split_id, ratio) {
        return None;
    }
    Some(sanitize_layout_config(&next, 0, 0))
}

/// Returns the first leaf id in depth-first traversal order.
pub fn first_leaf_id(node: &LayoutNode) -> Option<String> {
    match node {
        LayoutNode::Leaf { id, .. } => Some(id.clone()),
        LayoutNode::Split { first, second, .. } => {
            first_leaf_id(first).or_else(|| first_leaf_id(second))
        }
        LayoutNode::Empty => None,
    }
}

/// Creates a one-leaf root when layout is currently empty.
pub fn add_root_leaf_if_empty(layout: &LayoutConfig, panel: LayoutPanelKind) -> LayoutConfig {
    if !matches!(layout.root, LayoutNode::Empty) {
        return layout.clone();
    }
    let panel = if panel == LayoutPanelKind::None {
        LayoutPanelKind::TrackList
    } else {
        panel
    };
    let root = LayoutNode::Leaf {
        id: "l1".to_string(),
        panel,
    };
    sanitize_layout_config(
        &LayoutConfig {
            version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px: layout.playlist_album_art_column_min_width_px,
            playlist_album_art_column_max_width_px: layout.playlist_album_art_column_max_width_px,
            playlist_columns: layout.playlist_columns.clone(),
            root,
            selected_leaf_id: None,
        },
        0,
        0,
    )
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn clamp_ratio(
    requested: f32,
    total_px: i32,
    min_first_px: i32,
    min_second_px: i32,
) -> (f32, i32, i32) {
    if total_px <= 0 {
        return (0.5, 0, 0);
    }
    let min_first = min_first_px.clamp(0, total_px);
    let min_second = min_second_px.clamp(0, total_px);
    let max_first = (total_px - min_second).max(0);
    let requested_first = (requested.clamp(0.0, 1.0) * total_px as f32).round() as i32;
    let first = requested_first.clamp(min_first.min(max_first), max_first);
    let second = total_px - first;
    let ratio = if total_px > 0 {
        first as f32 / total_px as f32
    } else {
        0.5
    };
    (ratio, first, second)
}

fn layout_node_recursive(
    node: &LayoutNode,
    rect: Rect,
    selected_leaf_id: Option<&str>,
    splitter_thickness_px: i32,
    out: &mut TreeLayoutMetrics,
) {
    match node {
        LayoutNode::Empty => {}
        LayoutNode::Leaf { id, panel } => {
            if *panel == LayoutPanelKind::None {
                return;
            }
            if rect.width <= 0 || rect.height <= 0 {
                return;
            }
            out.leaves.push(LayoutLeafItem {
                id: id.clone(),
                panel: *panel,
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            });
            let _ = selected_leaf_id;
        }
        LayoutNode::Split {
            id,
            axis,
            ratio,
            min_first_px,
            min_second_px,
            first,
            second,
        } => {
            let (first_min_w, first_min_h) = min_size_for_node(first, splitter_thickness_px);
            let (second_min_w, second_min_h) = min_size_for_node(second, splitter_thickness_px);
            let splitter = splitter_thickness_px.max(0);
            match axis {
                SplitAxis::Vertical => {
                    let total = (rect.width - splitter).max(0);
                    let min_first = first_min_w.max(*min_first_px) as i32;
                    let min_second = second_min_w.max(*min_second_px) as i32;
                    let (effective_ratio, first_w, second_w) =
                        clamp_ratio(*ratio, total, min_first, min_second);
                    let split_x = rect.x + first_w;
                    let min_ratio = if total > 0 {
                        (min_first as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let max_ratio = if total > 0 {
                        ((total - min_second).max(0) as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    out.splitters.push(LayoutSplitterItem {
                        id: id.clone(),
                        axis: *axis,
                        x: split_x,
                        y: rect.y,
                        width: splitter,
                        height: rect.height.max(0),
                        ratio: effective_ratio,
                        min_ratio,
                        max_ratio,
                        track_start_px: rect.x,
                        track_length_px: total.max(1),
                    });
                    layout_node_recursive(
                        first,
                        Rect {
                            x: rect.x,
                            y: rect.y,
                            width: first_w,
                            height: rect.height,
                        },
                        selected_leaf_id,
                        splitter_thickness_px,
                        out,
                    );
                    layout_node_recursive(
                        second,
                        Rect {
                            x: split_x + splitter,
                            y: rect.y,
                            width: second_w,
                            height: rect.height,
                        },
                        selected_leaf_id,
                        splitter_thickness_px,
                        out,
                    );
                }
                SplitAxis::Horizontal => {
                    let total = (rect.height - splitter).max(0);
                    let min_first = first_min_h.max(*min_first_px) as i32;
                    let min_second = second_min_h.max(*min_second_px) as i32;
                    let (effective_ratio, first_h, second_h) =
                        clamp_ratio(*ratio, total, min_first, min_second);
                    let split_y = rect.y + first_h;
                    let min_ratio = if total > 0 {
                        (min_first as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let max_ratio = if total > 0 {
                        ((total - min_second).max(0) as f32 / total as f32).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    out.splitters.push(LayoutSplitterItem {
                        id: id.clone(),
                        axis: *axis,
                        x: rect.x,
                        y: split_y,
                        width: rect.width.max(0),
                        height: splitter,
                        ratio: effective_ratio,
                        min_ratio,
                        max_ratio,
                        track_start_px: rect.y,
                        track_length_px: total.max(1),
                    });
                    layout_node_recursive(
                        first,
                        Rect {
                            x: rect.x,
                            y: rect.y,
                            width: rect.width,
                            height: first_h,
                        },
                        selected_leaf_id,
                        splitter_thickness_px,
                        out,
                    );
                    layout_node_recursive(
                        second,
                        Rect {
                            x: rect.x,
                            y: split_y + splitter,
                            width: rect.width,
                            height: second_h,
                        },
                        selected_leaf_id,
                        splitter_thickness_px,
                        out,
                    );
                }
            }
        }
    }
}

/// Computes flattened leaf/splitter geometry from the split-tree.
pub fn compute_tree_layout_metrics(
    layout: &LayoutConfig,
    viewport_width_px: u32,
    viewport_height_px: u32,
    splitter_thickness_px: i32,
) -> TreeLayoutMetrics {
    let mut out = TreeLayoutMetrics::default();
    let rect = Rect {
        x: 0,
        y: 0,
        width: viewport_width_px.max(1) as i32,
        height: viewport_height_px.max(1) as i32,
    };
    layout_node_recursive(
        &layout.root,
        rect,
        layout.selected_leaf_id.as_deref(),
        splitter_thickness_px,
        &mut out,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{
        compute_tree_layout_metrics, delete_leaf, first_leaf_id, replace_leaf_panel,
        sanitize_layout_config, set_split_ratio, split_leaf, LayoutConfig, LayoutNode,
        LayoutPanelKind, LayoutSplitterItem, SplitAxis, LAYOUT_VERSION, SPLITTER_THICKNESS_PX,
    };

    fn splitter_by_id<'a>(splitters: &'a [LayoutSplitterItem], id: &str) -> &'a LayoutSplitterItem {
        splitters
            .iter()
            .find(|splitter| splitter.id == id)
            .expect("splitter should exist")
    }

    fn leaf_id_for_panel(node: &LayoutNode, panel: LayoutPanelKind) -> Option<String> {
        match node {
            LayoutNode::Leaf {
                id,
                panel: existing_panel,
            } if *existing_panel == panel => Some(id.clone()),
            LayoutNode::Split { first, second, .. } => {
                leaf_id_for_panel(first, panel).or_else(|| leaf_id_for_panel(second, panel))
            }
            _ => None,
        }
    }

    #[test]
    fn default_layout_is_tree_version_one() {
        let config = LayoutConfig::default();
        assert_eq!(config.version, LAYOUT_VERSION);
        assert!(first_leaf_id(&config.root).is_some());
    }

    #[test]
    fn split_leaf_creates_new_split_node() {
        let config = LayoutConfig::default();
        let leaf_id = first_leaf_id(&config.root).expect("leaf expected");
        let updated = split_leaf(
            &config,
            &leaf_id,
            SplitAxis::Vertical,
            LayoutPanelKind::Spacer,
        )
        .expect("split should succeed");
        let metrics = compute_tree_layout_metrics(&updated, 900, 650, SPLITTER_THICKNESS_PX);
        assert!(!metrics.splitters.is_empty());
        assert!(metrics.leaves.len() >= 2);
    }

    #[test]
    fn delete_leaf_can_produce_empty_layout() {
        let mut config = LayoutConfig::default();
        // Repeatedly delete first leaf until empty.
        while let Some(id) = first_leaf_id(&config.root) {
            config = delete_leaf(&config, &id).expect("delete should succeed");
        }
        let metrics = compute_tree_layout_metrics(&config, 900, 650, SPLITTER_THICKNESS_PX);
        assert!(metrics.leaves.is_empty());
    }

    #[test]
    fn replace_leaf_allows_duplicate_functional_panels() {
        let config = LayoutConfig::default();
        let transport_leaf = leaf_id_for_panel(&config.root, LayoutPanelKind::ButtonCluster)
            .expect("button cluster leaf should exist");
        let updated = replace_leaf_panel(&config, &transport_leaf, LayoutPanelKind::TrackList)
            .expect("replace should allow duplicate functional panels");
        let metrics = compute_tree_layout_metrics(&updated, 900, 650, SPLITTER_THICKNESS_PX);
        let track_count = metrics
            .leaves
            .iter()
            .filter(|leaf| leaf.panel == LayoutPanelKind::TrackList)
            .count();
        assert!(track_count >= 2);
    }

    #[test]
    fn splitter_ratio_updates_are_clamped_by_solver() {
        let config = LayoutConfig::default();
        let metrics = compute_tree_layout_metrics(&config, 900, 650, SPLITTER_THICKNESS_PX);
        let splitter = metrics
            .splitters
            .first()
            .expect("default layout should have splitters");
        let updated = set_split_ratio(&config, &splitter.id, 0.99).expect("ratio update");
        let updated_metrics =
            compute_tree_layout_metrics(&updated, 900, 650, SPLITTER_THICKNESS_PX);
        let updated_splitter = splitter_by_id(&updated_metrics.splitters, &splitter.id);
        assert!(updated_splitter.ratio <= updated_splitter.max_ratio + f32::EPSILON);
        assert!(updated_splitter.ratio >= updated_splitter.min_ratio - f32::EPSILON);
    }

    #[test]
    fn sanitize_layout_config_preserves_duplicate_panels() {
        let config = LayoutConfig {
            version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px:
                crate::config::default_playlist_album_art_column_min_width_px(),
            playlist_album_art_column_max_width_px:
                crate::config::default_playlist_album_art_column_max_width_px(),
            playlist_columns: crate::config::default_playlist_columns(),
            root: LayoutNode::Split {
                id: "split-root".to_string(),
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                min_first_px: 24,
                min_second_px: 24,
                first: Box::new(LayoutNode::Leaf {
                    id: "leaf-a".to_string(),
                    panel: LayoutPanelKind::TrackList,
                }),
                second: Box::new(LayoutNode::Leaf {
                    id: "leaf-b".to_string(),
                    panel: LayoutPanelKind::TrackList,
                }),
            },
            selected_leaf_id: None,
        };

        let sanitized = sanitize_layout_config(&config, 900, 650);
        let metrics = compute_tree_layout_metrics(&sanitized, 900, 650, SPLITTER_THICKNESS_PX);
        let track_count = metrics
            .leaves
            .iter()
            .filter(|leaf| leaf.panel == LayoutPanelKind::TrackList)
            .count();
        assert_eq!(track_count, 2);
    }

    #[test]
    fn test_layout_system_template_parses() {
        let _parsed: LayoutConfig = toml::from_str(include_str!("../config/layout.system.toml"))
            .expect("layout system template should parse");
    }
}
