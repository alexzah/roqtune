//! Split-tree layout data model, actions, and geometry.

use crate::config::{
    default_playlist_album_art_column_max_width_px, default_playlist_album_art_column_min_width_px,
    default_playlist_columns, ButtonClusterInstanceConfig, PlaylistColumnConfig,
};
use crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE;
use std::collections::{HashMap, HashSet};

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
/// Stable panel kind code for `LayoutPanelKind::MetadataViewer` (Text Panel).
pub const PANEL_CODE_METADATA_VIEWER: i32 = 8;
/// Stable panel kind code for `LayoutPanelKind::AlbumArtViewer` (Image Panel).
pub const PANEL_CODE_ALBUM_ART_VIEWER: i32 = 9;
/// Stable panel kind code for `LayoutPanelKind::Spacer`.
pub const PANEL_CODE_SPACER: i32 = 10;
/// Stable panel kind code for `LayoutPanelKind::StatusBar`.
pub const PANEL_CODE_STATUS_BAR: i32 = 11;
/// Stable panel kind code for `LayoutPanelKind::ImportButtonCluster`.
pub const PANEL_CODE_IMPORT_BUTTON_CLUSTER: i32 = 12;
/// Stable ID for the built-in default color scheme.
pub const DEFAULT_COLOR_SCHEME_ID: &str = "roqtune_dark";

const BUTTON_CLUSTER_BUTTON_SIZE_PX: u32 = 32;
const BUTTON_CLUSTER_BUTTON_SPACING_PX: u32 = 8;
const BUTTON_CLUSTER_HORIZONTAL_PADDING_PX: u32 = 16;
const BUTTON_CLUSTER_PANEL_HEIGHT_PX: u32 = 32;
const RELAXED_PANEL_MIN_EDGE_PX: u32 = 24;

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
    #[serde(rename = "text_panel", alias = "metadata_viewer")]
    MetadataViewer,
    #[serde(rename = "image_panel", alias = "album_art_viewer")]
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
            Self::ButtonCluster
            | Self::ImportButtonCluster
            | Self::TransportButtonCluster
            | Self::UtilityButtonCluster => button_cluster_panel_min_size_px(0),
            Self::VolumeSlider
            | Self::SeekBar
            | Self::PlaylistSwitcher
            | Self::TrackList
            | Self::MetadataViewer
            | Self::AlbumArtViewer
            | Self::Spacer => (RELAXED_PANEL_MIN_EDGE_PX, RELAXED_PANEL_MIN_EDGE_PX),
            Self::StatusBar => (RELAXED_PANEL_MIN_EDGE_PX, 20),
            Self::None => (0, 0),
            Self::ControlBar | Self::AlbumArtPane => {
                (RELAXED_PANEL_MIN_EDGE_PX, RELAXED_PANEL_MIN_EDGE_PX)
            }
        }
    }
}

fn button_cluster_panel_min_size_px(action_count: usize) -> (u32, u32) {
    let button_count = action_count.max(1) as u32;
    let content_width = button_count
        .saturating_mul(BUTTON_CLUSTER_BUTTON_SIZE_PX)
        .saturating_add(button_count.saturating_sub(1) * BUTTON_CLUSTER_BUTTON_SPACING_PX);
    (
        content_width.saturating_add(BUTTON_CLUSTER_HORIZONTAL_PADDING_PX),
        BUTTON_CLUSTER_PANEL_HEIGHT_PX,
    )
}

fn build_button_cluster_action_count_map(layout: &LayoutConfig) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for instance in &layout.button_cluster_instances {
        let leaf_id = instance.leaf_id.trim();
        if leaf_id.is_empty() {
            continue;
        }
        counts.insert(leaf_id.to_string(), instance.actions.len());
    }
    counts
}

fn panel_min_size_for_leaf(
    leaf_id: &str,
    panel: LayoutPanelKind,
    button_cluster_action_counts: &HashMap<String, usize>,
) -> (u32, u32) {
    match panel {
        LayoutPanelKind::ButtonCluster
        | LayoutPanelKind::ImportButtonCluster
        | LayoutPanelKind::TransportButtonCluster
        | LayoutPanelKind::UtilityButtonCluster => {
            let action_count = button_cluster_action_counts
                .get(leaf_id)
                .copied()
                .unwrap_or(0);
            button_cluster_panel_min_size_px(action_count)
        }
        _ => panel.min_size_px(),
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

/// Display-target resolution strategy for text/image panels.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewerPanelDisplayPriority {
    /// Follow the latest event source (selection or now playing).
    #[default]
    Default,
    /// Prefer current selection, then fallback to now playing.
    PreferSelection,
    /// Prefer now playing, then fallback to current selection.
    PreferNowPlaying,
    /// Only render selection context.
    SelectionOnly,
    /// Only render now-playing context.
    NowPlayingOnly,
}

/// Text payload source for text panels.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewerPanelMetadataSource {
    /// Render track metadata (title, artist, album, and tags).
    #[default]
    Track,
    /// Render album internet description from enrichment payloads.
    AlbumDescription,
    /// Render artist internet biography from enrichment payloads.
    ArtistBio,
    /// Render custom template text using track metadata placeholders.
    CustomText,
}

/// Image payload source for image panels.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewerPanelImageSource {
    /// Render track album art.
    #[default]
    AlbumArt,
    /// Render artist image from enrichment payloads.
    ArtistImage,
}

/// Mode policy for collection-aware panels (switcher + track list).
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CollectionPanelMode {
    /// Follow global collection mode (playlist/library).
    #[default]
    Both,
    /// Force playlist mode content only.
    PlaylistOnly,
    /// Force library mode content only.
    LibraryOnly,
}

/// Per-leaf configuration for collection-aware panels.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct CollectionPanelInstanceConfig {
    /// Layout leaf identifier that owns this panel.
    pub leaf_id: String,
    /// Content mode policy.
    #[serde(default)]
    pub mode: CollectionPanelMode,
}

/// Per-leaf text-panel configuration persisted with layout preferences.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct MetadataViewerPanelInstanceConfig {
    /// Layout leaf identifier that owns this viewer.
    pub leaf_id: String,
    /// Display-target resolution strategy.
    #[serde(default)]
    pub display_priority: ViewerPanelDisplayPriority,
    /// Text source strategy for text panels.
    #[serde(default, rename = "text_source", alias = "metadata_source")]
    pub metadata_source: ViewerPanelMetadataSource,
    /// Custom text template string used when `text_source = "custom_text"`.
    #[serde(
        default = "default_metadata_text_format",
        rename = "text_format",
        alias = "metadata_text_format"
    )]
    pub metadata_text_format: String,
}

/// Per-leaf image-panel configuration persisted with layout preferences.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct AlbumArtViewerPanelInstanceConfig {
    /// Layout leaf identifier that owns this viewer.
    pub leaf_id: String,
    /// Display-target resolution strategy.
    #[serde(default)]
    pub display_priority: ViewerPanelDisplayPriority,
    /// Image source strategy for image panels.
    #[serde(default)]
    pub image_source: ViewerPanelImageSource,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq, Default)]
struct LegacyViewerPanelInstanceConfig {
    pub leaf_id: String,
    #[serde(default)]
    pub display_priority: ViewerPanelDisplayPriority,
    #[serde(default)]
    pub metadata_source: ViewerPanelMetadataSource,
    #[serde(default)]
    pub metadata_use_custom_text: bool,
    #[serde(default = "default_metadata_text_format")]
    pub metadata_text_format: String,
    #[serde(default)]
    pub image_source: ViewerPanelImageSource,
}

/// Persistent per-column width override shared across playlists.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct PlaylistColumnWidthOverrideConfig {
    /// Stable column key (`{title}` or `custom:Name|Format`).
    pub column_key: String,
    /// Width override in logical pixels.
    pub width_px: u32,
}

/// Named theme color components used across the UI.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ThemeColorComponents {
    pub window_bg: String,
    pub panel_bg: String,
    pub panel_bg_alt: String,
    pub border: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_muted: String,
    pub accent: String,
    pub accent_on: String,
    pub warning: String,
    pub danger: String,
    pub success: String,
    #[serde(default)]
    pub control_hover_bg: String,
    #[serde(default)]
    pub selection_bg: String,
    #[serde(default)]
    pub selection_border: String,
}

/// Persistent layout preferences.
/// This is the only persisted home for layout-owned settings (`layout.toml`).
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutConfig {
    /// Layout schema version.
    pub version: u32,
    /// Built-in Album Art playlist-column width minimum.
    pub playlist_album_art_column_min_width_px: u32,
    /// Built-in Album Art playlist-column width maximum.
    pub playlist_album_art_column_max_width_px: u32,
    /// Selected color scheme identifier from preset catalog, or `custom`.
    pub color_scheme: String,
    /// User-customized color component values used when `color_scheme = "custom"`.
    pub custom_colors: Option<ThemeColorComponents>,
    /// Ordered playlist column list (built-ins + custom columns), shared by all playlists.
    /// Do not reintroduce per-playlist column ordering from playlist storage.
    pub playlist_columns: Vec<PlaylistColumnConfig>,
    /// Global playlist column width overrides.
    pub playlist_column_width_overrides: Vec<PlaylistColumnWidthOverrideConfig>,
    /// Per-leaf button-cluster settings.
    pub button_cluster_instances: Vec<ButtonClusterInstanceConfig>,
    /// Per-leaf mode settings for collection-aware panels.
    pub collection_panel_instances: Vec<CollectionPanelInstanceConfig>,
    /// Per-leaf text-panel settings.
    pub metadata_viewer_panel_instances: Vec<MetadataViewerPanelInstanceConfig>,
    /// Per-leaf image-panel settings.
    pub album_art_viewer_panel_instances: Vec<AlbumArtViewerPanelInstanceConfig>,
    /// Root node of the split-tree.
    pub root: LayoutNode,
    /// Runtime-only leaf selection used in edit mode.
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
            color_scheme: default_color_scheme_id(),
            custom_colors: None,
            playlist_columns: default_playlist_columns(),
            playlist_column_width_overrides: Vec::new(),
            button_cluster_instances: Vec::new(),
            collection_panel_instances: Vec::new(),
            metadata_viewer_panel_instances: Vec::new(),
            album_art_viewer_panel_instances: Vec::new(),
            root: build_default_layout_root(),
            selected_leaf_id: None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct LayoutPlaylistColumnWire {
    #[serde(flatten)]
    column: PlaylistColumnConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    width_px: Option<u32>,
}

fn default_playlist_columns_wire() -> Vec<LayoutPlaylistColumnWire> {
    default_playlist_columns()
        .into_iter()
        .map(|column| LayoutPlaylistColumnWire {
            column,
            width_px: None,
        })
        .collect()
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct LayoutConfigWire {
    #[serde(default = "default_layout_version", rename = "version")]
    _version: u32,
    #[serde(default = "default_playlist_album_art_column_min_width_px")]
    playlist_album_art_column_min_width_px: u32,
    #[serde(default = "default_playlist_album_art_column_max_width_px")]
    playlist_album_art_column_max_width_px: u32,
    #[serde(default = "default_color_scheme_id")]
    color_scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custom_colors: Option<ThemeColorComponents>,
    #[serde(default = "default_playlist_columns_wire")]
    playlist_columns: Vec<LayoutPlaylistColumnWire>,
    // Legacy top-level key kept for backwards-compatibility while migrating to
    // per-column `width_px` values in `[[playlist_columns]]`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    playlist_column_width_overrides: Vec<PlaylistColumnWidthOverrideConfig>,
    #[serde(default)]
    button_cluster_instances: Vec<ButtonClusterInstanceConfig>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    collection_panel_instances: Vec<CollectionPanelInstanceConfig>,
    #[serde(default)]
    #[serde(alias = "metadata_viewer_panel_instances")]
    text_panel_instances: Vec<MetadataViewerPanelInstanceConfig>,
    #[serde(default)]
    #[serde(alias = "album_art_viewer_panel_instances")]
    image_panel_instances: Vec<AlbumArtViewerPanelInstanceConfig>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    viewer_panel_instances: Vec<LegacyViewerPanelInstanceConfig>,
    #[serde(default = "default_layout_root_for_wire")]
    root: LayoutNode,
}

impl<'de> serde::Deserialize<'de> for LayoutConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v2 = LayoutConfigWire::deserialize(deserializer)?;
        let mut playlist_columns = Vec::with_capacity(v2.playlist_columns.len());
        let mut playlist_column_width_overrides = Vec::new();
        let mut seen_override_keys = HashSet::new();
        for wire in v2.playlist_columns {
            let key = playlist_column_key(&wire.column);
            if let Some(width_px) = wire.width_px {
                if seen_override_keys.insert(key.clone()) {
                    playlist_column_width_overrides.push(PlaylistColumnWidthOverrideConfig {
                        column_key: key,
                        width_px,
                    });
                }
            }
            playlist_columns.push(wire.column);
        }
        for legacy_override in v2.playlist_column_width_overrides {
            let key = legacy_override.column_key.trim();
            if key.is_empty() || !seen_override_keys.insert(key.to_string()) {
                continue;
            }
            playlist_column_width_overrides.push(PlaylistColumnWidthOverrideConfig {
                column_key: key.to_string(),
                width_px: legacy_override.width_px,
            });
        }
        let mut metadata_viewer_panel_instances = v2.text_panel_instances;
        let mut album_art_viewer_panel_instances = v2.image_panel_instances;
        if metadata_viewer_panel_instances.is_empty()
            && album_art_viewer_panel_instances.is_empty()
            && !v2.viewer_panel_instances.is_empty()
        {
            let mut panel_kind_by_leaf: HashMap<String, LayoutPanelKind> = HashMap::new();
            collect_leaf_panel_kinds(&v2.root, &mut panel_kind_by_leaf);
            for legacy in v2.viewer_panel_instances {
                let leaf_id = legacy.leaf_id.trim();
                if leaf_id.is_empty() {
                    continue;
                }
                match panel_kind_by_leaf.get(leaf_id).copied() {
                    Some(LayoutPanelKind::MetadataViewer) => {
                        metadata_viewer_panel_instances.push(MetadataViewerPanelInstanceConfig {
                            leaf_id: leaf_id.to_string(),
                            display_priority: legacy.display_priority,
                            metadata_source: if legacy.metadata_use_custom_text {
                                ViewerPanelMetadataSource::CustomText
                            } else {
                                legacy.metadata_source
                            },
                            metadata_text_format: if legacy.metadata_text_format.trim().is_empty() {
                                default_metadata_text_format()
                            } else {
                                legacy.metadata_text_format
                            },
                        });
                    }
                    Some(LayoutPanelKind::AlbumArtViewer) => {
                        album_art_viewer_panel_instances.push(AlbumArtViewerPanelInstanceConfig {
                            leaf_id: leaf_id.to_string(),
                            display_priority: legacy.display_priority,
                            image_source: legacy.image_source,
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(LayoutConfig {
            version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px: v2.playlist_album_art_column_min_width_px,
            playlist_album_art_column_max_width_px: v2.playlist_album_art_column_max_width_px,
            color_scheme: {
                let trimmed = v2.color_scheme.trim();
                if trimmed.is_empty() {
                    default_color_scheme_id()
                } else {
                    trimmed.to_string()
                }
            },
            custom_colors: v2.custom_colors,
            playlist_columns,
            playlist_column_width_overrides,
            button_cluster_instances: v2.button_cluster_instances,
            collection_panel_instances: v2.collection_panel_instances,
            metadata_viewer_panel_instances,
            album_art_viewer_panel_instances,
            root: v2.root,
            selected_leaf_id: None,
        })
    }
}

impl serde::Serialize for LayoutConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut override_widths_by_key: HashMap<String, u32> = HashMap::new();
        for override_item in &self.playlist_column_width_overrides {
            let key = override_item.column_key.trim();
            if key.is_empty() || override_widths_by_key.contains_key(key) {
                continue;
            }
            override_widths_by_key.insert(key.to_string(), override_item.width_px);
        }

        let playlist_columns = self
            .playlist_columns
            .iter()
            .cloned()
            .map(|column| {
                let key = playlist_column_key(&column);
                let width_px = override_widths_by_key.get(&key).copied();
                LayoutPlaylistColumnWire { column, width_px }
            })
            .collect();

        LayoutConfigWire {
            _version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px: self.playlist_album_art_column_min_width_px,
            playlist_album_art_column_max_width_px: self.playlist_album_art_column_max_width_px,
            color_scheme: if self.color_scheme.trim().is_empty() {
                default_color_scheme_id()
            } else {
                self.color_scheme.clone()
            },
            custom_colors: self.custom_colors.clone(),
            playlist_columns,
            playlist_column_width_overrides: Vec::new(),
            button_cluster_instances: self.button_cluster_instances.clone(),
            collection_panel_instances: self.collection_panel_instances.clone(),
            text_panel_instances: self.metadata_viewer_panel_instances.clone(),
            image_panel_instances: self.album_art_viewer_panel_instances.clone(),
            viewer_panel_instances: Vec::new(),
            root: self.root.clone(),
        }
        .serialize(serializer)
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

fn default_color_scheme_id() -> String {
    DEFAULT_COLOR_SCHEME_ID.to_string()
}

fn default_metadata_text_format() -> String {
    DEFAULT_METADATA_PANEL_TEMPLATE.to_string()
}

fn default_layout_root_for_wire() -> LayoutNode {
    build_default_layout_root()
}

fn collect_leaf_panel_kinds(node: &LayoutNode, out: &mut HashMap<String, LayoutPanelKind>) {
    match node {
        LayoutNode::Leaf { id, panel } => {
            out.insert(id.clone(), *panel);
        }
        LayoutNode::Split { first, second, .. } => {
            collect_leaf_panel_kinds(first, out);
            collect_leaf_panel_kinds(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn playlist_column_key(column: &PlaylistColumnConfig) -> String {
    if column.custom {
        format!("custom:{}|{}", column.name.trim(), column.format.trim())
    } else {
        column.format.trim().to_ascii_lowercase()
    }
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

fn min_size_for_node(
    node: &LayoutNode,
    splitter_thickness_px: i32,
    button_cluster_action_counts: &HashMap<String, usize>,
) -> (u32, u32) {
    match node {
        LayoutNode::Empty => (0, 0),
        LayoutNode::Leaf { id, panel } => {
            panel_min_size_for_leaf(id, *panel, button_cluster_action_counts)
        }
        LayoutNode::Split {
            axis,
            first,
            second,
            ..
        } => {
            let (first_min_w, first_min_h) =
                min_size_for_node(first, splitter_thickness_px, button_cluster_action_counts);
            let (second_min_w, second_min_h) =
                min_size_for_node(second, splitter_thickness_px, button_cluster_action_counts);
            let splitter = splitter_thickness_px.max(0) as u32;
            match axis {
                SplitAxis::Vertical => (
                    first_min_w
                        .saturating_add(second_min_w)
                        .saturating_add(splitter),
                    first_min_h.max(second_min_h),
                ),
                SplitAxis::Horizontal => (
                    first_min_w.max(second_min_w),
                    first_min_h
                        .saturating_add(second_min_h)
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
                    ratio.clamp(0.0, 1.0)
                } else {
                    0.5
                },
                min_first_px,
                min_second_px,
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
        color_scheme: if config.color_scheme.trim().is_empty() {
            default_color_scheme_id()
        } else {
            config.color_scheme.clone()
        },
        custom_colors: config.custom_colors.clone(),
        playlist_columns: config.playlist_columns.clone(),
        playlist_column_width_overrides: config.playlist_column_width_overrides.clone(),
        button_cluster_instances: config.button_cluster_instances.clone(),
        collection_panel_instances: config.collection_panel_instances.clone(),
        metadata_viewer_panel_instances: config.metadata_viewer_panel_instances.clone(),
        album_art_viewer_panel_instances: config.album_art_viewer_panel_instances.clone(),
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
                min_first_px: 0,
                min_second_px: 0,
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
                    ratio.clamp(0.0, 1.0)
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
            color_scheme: if layout.color_scheme.trim().is_empty() {
                default_color_scheme_id()
            } else {
                layout.color_scheme.clone()
            },
            custom_colors: layout.custom_colors.clone(),
            playlist_columns: layout.playlist_columns.clone(),
            playlist_column_width_overrides: layout.playlist_column_width_overrides.clone(),
            button_cluster_instances: layout.button_cluster_instances.clone(),
            collection_panel_instances: layout.collection_panel_instances.clone(),
            metadata_viewer_panel_instances: layout.metadata_viewer_panel_instances.clone(),
            album_art_viewer_panel_instances: layout.album_art_viewer_panel_instances.clone(),
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
    button_cluster_action_counts: &HashMap<String, usize>,
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
            first,
            second,
            ..
        } => {
            let (first_min_w, first_min_h) =
                min_size_for_node(first, splitter_thickness_px, button_cluster_action_counts);
            let (second_min_w, second_min_h) =
                min_size_for_node(second, splitter_thickness_px, button_cluster_action_counts);
            let splitter = splitter_thickness_px.max(0);
            match axis {
                SplitAxis::Vertical => {
                    let total = (rect.width - splitter).max(0);
                    let min_first = first_min_w as i32;
                    let min_second = second_min_w as i32;
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
                        button_cluster_action_counts,
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
                        button_cluster_action_counts,
                        out,
                    );
                }
                SplitAxis::Horizontal => {
                    let total = (rect.height - splitter).max(0);
                    let min_first = first_min_h as i32;
                    let min_second = second_min_h as i32;
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
                        button_cluster_action_counts,
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
                        button_cluster_action_counts,
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
    let button_cluster_action_counts = build_button_cluster_action_count_map(layout);
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
        &button_cluster_action_counts,
        &mut out,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{
        compute_tree_layout_metrics, delete_leaf, first_leaf_id, replace_leaf_panel,
        sanitize_layout_config, set_split_ratio, split_leaf, LayoutConfig, LayoutNode,
        LayoutPanelKind, LayoutSplitterItem, PlaylistColumnWidthOverrideConfig, SplitAxis,
        DEFAULT_COLOR_SCHEME_ID, LAYOUT_VERSION, SPLITTER_THICKNESS_PX,
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
    fn button_cluster_min_size_tracks_configured_action_count() {
        let config = LayoutConfig {
            version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px:
                crate::config::default_playlist_album_art_column_min_width_px(),
            playlist_album_art_column_max_width_px:
                crate::config::default_playlist_album_art_column_max_width_px(),
            color_scheme: DEFAULT_COLOR_SCHEME_ID.to_string(),
            custom_colors: None,
            playlist_columns: crate::config::default_playlist_columns(),
            playlist_column_width_overrides: Vec::new(),
            button_cluster_instances: vec![crate::config::ButtonClusterInstanceConfig {
                leaf_id: "cluster".to_string(),
                actions: vec![1, 2, 3, 4],
            }],
            collection_panel_instances: Vec::new(),
            metadata_viewer_panel_instances: Vec::new(),
            album_art_viewer_panel_instances: Vec::new(),
            root: LayoutNode::Split {
                id: "split".to_string(),
                axis: SplitAxis::Vertical,
                ratio: 0.1,
                min_first_px: 0,
                min_second_px: 0,
                first: Box::new(LayoutNode::Leaf {
                    id: "cluster".to_string(),
                    panel: LayoutPanelKind::ButtonCluster,
                }),
                second: Box::new(LayoutNode::Leaf {
                    id: "spacer".to_string(),
                    panel: LayoutPanelKind::Spacer,
                }),
            },
            selected_leaf_id: None,
        };

        let metrics = compute_tree_layout_metrics(&config, 420, 120, SPLITTER_THICKNESS_PX);
        let cluster_leaf = metrics
            .leaves
            .iter()
            .find(|leaf| leaf.id == "cluster")
            .expect("cluster leaf should exist");

        // 4 buttons * 32 + 3 gaps * 8 + 16 horizontal padding.
        assert!(cluster_leaf.width >= 168);
        assert!(cluster_leaf.height >= 32);
    }

    #[test]
    fn split_level_minimums_do_not_override_panel_adaptive_minimums() {
        let config = LayoutConfig {
            version: LAYOUT_VERSION,
            playlist_album_art_column_min_width_px:
                crate::config::default_playlist_album_art_column_min_width_px(),
            playlist_album_art_column_max_width_px:
                crate::config::default_playlist_album_art_column_max_width_px(),
            color_scheme: DEFAULT_COLOR_SCHEME_ID.to_string(),
            custom_colors: None,
            playlist_columns: crate::config::default_playlist_columns(),
            playlist_column_width_overrides: Vec::new(),
            button_cluster_instances: Vec::new(),
            collection_panel_instances: Vec::new(),
            metadata_viewer_panel_instances: Vec::new(),
            album_art_viewer_panel_instances: Vec::new(),
            root: LayoutNode::Split {
                id: "split".to_string(),
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                min_first_px: 500,
                min_second_px: 500,
                first: Box::new(LayoutNode::Leaf {
                    id: "left".to_string(),
                    panel: LayoutPanelKind::Spacer,
                }),
                second: Box::new(LayoutNode::Leaf {
                    id: "right".to_string(),
                    panel: LayoutPanelKind::Spacer,
                }),
            },
            selected_leaf_id: None,
        };

        let metrics = compute_tree_layout_metrics(&config, 360, 120, SPLITTER_THICKNESS_PX);
        let splitter = metrics
            .splitters
            .first()
            .expect("splitter should exist for split layout");

        assert!(splitter.min_ratio < 0.25);
        assert!(splitter.max_ratio > 0.75);
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
            color_scheme: DEFAULT_COLOR_SCHEME_ID.to_string(),
            custom_colors: None,
            playlist_columns: crate::config::default_playlist_columns(),
            playlist_column_width_overrides: Vec::new(),
            button_cluster_instances: Vec::new(),
            collection_panel_instances: Vec::new(),
            metadata_viewer_panel_instances: Vec::new(),
            album_art_viewer_panel_instances: Vec::new(),
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

    #[test]
    fn test_layout_serializes_column_width_overrides_inline_with_columns() {
        let mut layout = LayoutConfig::default();
        layout
            .playlist_column_width_overrides
            .push(PlaylistColumnWidthOverrideConfig {
                column_key: "{title}".to_string(),
                width_px: 321,
            });

        let serialized = toml::to_string(&layout).expect("layout should serialize");
        assert!(serialized.contains("width_px = 321"));
        assert!(
            !serialized.contains("playlist_column_width_overrides"),
            "legacy top-level width override key should not be emitted"
        );
    }

    #[test]
    fn test_layout_deserializes_legacy_top_level_width_overrides() {
        let mut legacy_layout = include_str!("../config/layout.system.toml").to_string();
        legacy_layout.push_str(
            "\n[[playlist_column_width_overrides]]\ncolumn_key = \"{title}\"\nwidth_px = 222\n",
        );

        let parsed: LayoutConfig =
            toml::from_str(&legacy_layout).expect("legacy layout should parse");
        let title_width = parsed
            .playlist_column_width_overrides
            .iter()
            .find(|entry| entry.column_key == "{title}")
            .map(|entry| entry.width_px);
        assert_eq!(title_width, Some(222));
    }
}
