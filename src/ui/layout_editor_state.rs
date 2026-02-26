//! Layout-editor state transforms, model syncing, and utility helpers.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{Arc, Mutex},
};

use slint::{Model, ModelRc, VecModel};

use crate::{
    config::{ButtonClusterInstanceConfig, Config, UiConfig},
    layout::{
        self, compute_tree_layout_metrics, sanitize_layout_config,
        AlbumArtViewerPanelInstanceConfig, LayoutConfig, LayoutNode, LayoutPanelKind,
        MetadataViewerPanelInstanceConfig, ViewerPanelDisplayPriority, ViewerPanelImageSource,
        ViewerPanelMetadataSource, SPLITTER_THICKNESS_PX,
    },
    AppWindow, LayoutAlbumArtViewerPanelModel, LayoutButtonClusterPanelModel,
    LayoutMetadataViewerPanelModel, LayoutSplitterModel, RichTextBlock, RichTextLine,
    IMPORT_CLUSTER_PRESET, TRANSPORT_CLUSTER_PRESET, UTILITY_CLUSTER_PRESET,
};

fn collect_button_cluster_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, panel } => {
            if *panel == LayoutPanelKind::ButtonCluster {
                out.push(id.clone());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_button_cluster_leaf_ids(first, out);
            collect_button_cluster_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn collect_metadata_viewer_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, panel } => {
            if *panel == LayoutPanelKind::MetadataViewer {
                out.push(id.clone());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_metadata_viewer_leaf_ids(first, out);
            collect_metadata_viewer_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn collect_album_art_viewer_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, panel } => {
            if *panel == LayoutPanelKind::AlbumArtViewer {
                out.push(id.clone());
            }
        }
        LayoutNode::Split { first, second, .. } => {
            collect_album_art_viewer_leaf_ids(first, out);
            collect_album_art_viewer_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn button_cluster_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_button_cluster_leaf_ids(&layout.root, &mut ids);
    ids
}

fn metadata_viewer_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_metadata_viewer_leaf_ids(&layout.root, &mut ids);
    ids
}

fn album_art_viewer_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_album_art_viewer_leaf_ids(&layout.root, &mut ids);
    ids
}

fn collect_all_leaf_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Leaf { id, .. } => out.push(id.clone()),
        LayoutNode::Split { first, second, .. } => {
            collect_all_leaf_ids(first, out);
            collect_all_leaf_ids(second, out);
        }
        LayoutNode::Empty => {}
    }
}

fn layout_leaf_ids(layout: &LayoutConfig) -> Vec<String> {
    let mut ids = Vec::new();
    collect_all_leaf_ids(&layout.root, &mut ids);
    ids
}

/// Returns leaf IDs that exist in `next_layout` but not `previous_layout`.
pub(crate) fn newly_added_leaf_ids(
    previous_layout: &LayoutConfig,
    next_layout: &LayoutConfig,
) -> Vec<String> {
    let previous_ids: HashSet<String> = layout_leaf_ids(previous_layout).into_iter().collect();
    layout_leaf_ids(next_layout)
        .into_iter()
        .filter(|id| !previous_ids.contains(id))
        .collect()
}

fn sanitize_button_actions(actions: &[i32]) -> Vec<i32> {
    actions
        .iter()
        .copied()
        .filter(|action| (1..=12).contains(action))
        .collect()
}

/// Returns default button actions for a button-cluster slot index.
pub(crate) fn default_button_cluster_actions_by_index(index: usize) -> Vec<i32> {
    match index {
        0 => IMPORT_CLUSTER_PRESET.to_vec(),
        1 => TRANSPORT_CLUSTER_PRESET.to_vec(),
        2 => UTILITY_CLUSTER_PRESET.to_vec(),
        _ => Vec::new(),
    }
}

/// Sanitizes button-cluster instances against the active layout leaf set.
pub(crate) fn sanitize_button_cluster_instances(
    layout: &LayoutConfig,
    instances: &[ButtonClusterInstanceConfig],
) -> Vec<ButtonClusterInstanceConfig> {
    let leaf_ids = button_cluster_leaf_ids(layout);
    let had_existing_instances = !instances.is_empty();
    let mut actions_by_leaf: HashMap<&str, Vec<i32>> = HashMap::new();
    for instance in instances {
        let leaf_id = instance.leaf_id.trim();
        if leaf_id.is_empty() {
            continue;
        }
        actions_by_leaf.insert(leaf_id, sanitize_button_actions(&instance.actions));
    }

    leaf_ids
        .iter()
        .enumerate()
        .map(|(index, leaf_id)| ButtonClusterInstanceConfig {
            leaf_id: leaf_id.clone(),
            actions: actions_by_leaf
                .get(leaf_id.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    if had_existing_instances {
                        Vec::new()
                    } else {
                        default_button_cluster_actions_by_index(index)
                    }
                }),
        })
        .collect()
}

/// Sanitizes metadata-viewer instances against the active layout leaf set.
pub(crate) fn sanitize_metadata_viewer_panel_instances(
    layout: &LayoutConfig,
    instances: &[MetadataViewerPanelInstanceConfig],
) -> Vec<MetadataViewerPanelInstanceConfig> {
    let leaf_ids = metadata_viewer_leaf_ids(layout);
    let mut priority_by_leaf: HashMap<&str, ViewerPanelDisplayPriority> = HashMap::new();
    let mut metadata_source_by_leaf: HashMap<&str, ViewerPanelMetadataSource> = HashMap::new();
    let mut metadata_text_format_by_leaf: HashMap<&str, String> = HashMap::new();
    for instance in instances {
        let leaf_id = instance.leaf_id.trim();
        if leaf_id.is_empty() {
            continue;
        }
        priority_by_leaf.insert(leaf_id, instance.display_priority);
        metadata_source_by_leaf.insert(leaf_id, instance.metadata_source);
        metadata_text_format_by_leaf.insert(leaf_id, instance.metadata_text_format.clone());
    }

    leaf_ids
        .into_iter()
        .map(|leaf_id| MetadataViewerPanelInstanceConfig {
            display_priority: priority_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            metadata_source: metadata_source_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            metadata_text_format: metadata_text_format_by_leaf
                .get(leaf_id.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE.to_string()
                }),
            leaf_id,
        })
        .collect()
}

/// Sanitizes album-art-viewer instances against the active layout leaf set.
pub(crate) fn sanitize_album_art_viewer_panel_instances(
    layout: &LayoutConfig,
    instances: &[AlbumArtViewerPanelInstanceConfig],
) -> Vec<AlbumArtViewerPanelInstanceConfig> {
    let leaf_ids = album_art_viewer_leaf_ids(layout);
    let mut priority_by_leaf: HashMap<&str, ViewerPanelDisplayPriority> = HashMap::new();
    let mut image_source_by_leaf: HashMap<&str, ViewerPanelImageSource> = HashMap::new();
    for instance in instances {
        let leaf_id = instance.leaf_id.trim();
        if leaf_id.is_empty() {
            continue;
        }
        priority_by_leaf.insert(leaf_id, instance.display_priority);
        image_source_by_leaf.insert(leaf_id, instance.image_source);
    }

    leaf_ids
        .into_iter()
        .map(|leaf_id| AlbumArtViewerPanelInstanceConfig {
            display_priority: priority_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            image_source: image_source_by_leaf
                .get(leaf_id.as_str())
                .copied()
                .unwrap_or_default(),
            leaf_id,
        })
        .collect()
}

/// Computes sidebar width from current window width with responsive clamping.
pub(crate) fn sidebar_width_from_window(window_width_px: u32) -> i32 {
    let proportional = ((window_width_px as f32) * 0.24).round() as u32;
    proportional.clamp(180, 300) as i32
}

/// Returns a non-zero workspace-size snapshot from shared state.
pub(crate) fn workspace_size_snapshot(workspace_size: &Arc<Mutex<(u32, u32)>>) -> (u32, u32) {
    let (width, height) = *workspace_size
        .lock()
        .expect("layout workspace size lock poisoned");
    (width.max(1), height.max(1))
}

/// Quantizes splitter ratios to two-decimal precision for stable persistence.
pub(crate) fn quantize_splitter_ratio_to_precision(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return ratio;
    }
    (ratio * 100.0).round() / 100.0
}

/// Returns available layout-panel options shown in the layout editor UI.
pub(crate) fn layout_panel_options() -> Vec<slint::SharedString> {
    vec![
        "None".into(),
        "Button Cluster".into(),
        "Transport Button Cluster".into(),
        "Utility Button Cluster".into(),
        "Volume Slider".into(),
        "Seek Bar".into(),
        "Playlist Switcher".into(),
        "Track List".into(),
        "Text Panel".into(),
        "Image Panel".into(),
        "Spacer".into(),
        "Status Bar".into(),
        "Import Button Cluster".into(),
    ]
}

/// Resolves a panel selection code into a concrete layout panel and optional preset actions.
pub(crate) fn resolve_layout_panel_selection(
    panel_code: i32,
) -> (LayoutPanelKind, Option<Vec<i32>>) {
    if panel_code == LayoutPanelKind::TransportButtonCluster.to_code() {
        return (
            LayoutPanelKind::ButtonCluster,
            Some(TRANSPORT_CLUSTER_PRESET.to_vec()),
        );
    }
    if panel_code == LayoutPanelKind::UtilityButtonCluster.to_code() {
        return (
            LayoutPanelKind::ButtonCluster,
            Some(UTILITY_CLUSTER_PRESET.to_vec()),
        );
    }
    if panel_code == LayoutPanelKind::ImportButtonCluster.to_code() {
        return (
            LayoutPanelKind::ButtonCluster,
            Some(IMPORT_CLUSTER_PRESET.to_vec()),
        );
    }
    (LayoutPanelKind::from_code(panel_code), None)
}

/// Returns button-cluster action IDs configured for a leaf.
pub(crate) fn button_cluster_actions_for_leaf(
    instances: &[ButtonClusterInstanceConfig],
    leaf_id: &str,
) -> Vec<i32> {
    instances
        .iter()
        .find(|instance| instance.leaf_id == leaf_id)
        .map(|instance| instance.actions.clone())
        .unwrap_or_default()
}

/// Inserts or updates button-cluster actions for a leaf.
pub(crate) fn upsert_button_cluster_actions_for_leaf(
    instances: &mut Vec<ButtonClusterInstanceConfig>,
    leaf_id: &str,
    actions: Vec<i32>,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.actions = sanitize_button_actions(&actions);
        return;
    }
    instances.push(ButtonClusterInstanceConfig {
        leaf_id: leaf_id.to_string(),
        actions: sanitize_button_actions(&actions),
    });
}

/// Removes any button-cluster instance bound to a leaf.
pub(crate) fn remove_button_cluster_instance_for_leaf(
    instances: &mut Vec<ButtonClusterInstanceConfig>,
    leaf_id: &str,
) {
    instances.retain(|instance| instance.leaf_id != leaf_id);
}

/// Inserts or updates metadata-viewer display priority for a leaf.
pub(crate) fn upsert_metadata_viewer_panel_display_priority_for_leaf(
    instances: &mut Vec<MetadataViewerPanelInstanceConfig>,
    leaf_id: &str,
    display_priority: ViewerPanelDisplayPriority,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.display_priority = display_priority;
        return;
    }
    instances.push(MetadataViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority,
        metadata_source: ViewerPanelMetadataSource::default(),
        metadata_text_format: crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE.to_string(),
    });
}

/// Inserts or updates metadata-viewer metadata source for a leaf.
pub(crate) fn upsert_metadata_viewer_panel_metadata_source_for_leaf(
    instances: &mut Vec<MetadataViewerPanelInstanceConfig>,
    leaf_id: &str,
    metadata_source: ViewerPanelMetadataSource,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.metadata_source = metadata_source;
        return;
    }
    instances.push(MetadataViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority: ViewerPanelDisplayPriority::default(),
        metadata_source,
        metadata_text_format: crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE.to_string(),
    });
}

/// Inserts or updates metadata-viewer template text for a leaf.
pub(crate) fn upsert_metadata_viewer_panel_text_format_for_leaf(
    instances: &mut Vec<MetadataViewerPanelInstanceConfig>,
    leaf_id: &str,
    metadata_text_format: String,
) {
    let normalized_format = if metadata_text_format.trim().is_empty() {
        crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE.to_string()
    } else {
        metadata_text_format
    };
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.metadata_text_format = normalized_format;
        return;
    }
    instances.push(MetadataViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority: ViewerPanelDisplayPriority::default(),
        metadata_source: ViewerPanelMetadataSource::CustomText,
        metadata_text_format: normalized_format,
    });
}

/// Inserts or updates album-art-viewer display priority for a leaf.
pub(crate) fn upsert_album_art_viewer_panel_display_priority_for_leaf(
    instances: &mut Vec<AlbumArtViewerPanelInstanceConfig>,
    leaf_id: &str,
    display_priority: ViewerPanelDisplayPriority,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.display_priority = display_priority;
        return;
    }
    instances.push(AlbumArtViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority,
        image_source: ViewerPanelImageSource::default(),
    });
}

/// Inserts or updates album-art-viewer image source for a leaf.
pub(crate) fn upsert_album_art_viewer_panel_image_source_for_leaf(
    instances: &mut Vec<AlbumArtViewerPanelInstanceConfig>,
    leaf_id: &str,
    image_source: ViewerPanelImageSource,
) {
    if let Some(existing) = instances
        .iter_mut()
        .find(|instance| instance.leaf_id == leaf_id)
    {
        existing.image_source = image_source;
        return;
    }
    instances.push(AlbumArtViewerPanelInstanceConfig {
        leaf_id: leaf_id.to_string(),
        display_priority: ViewerPanelDisplayPriority::default(),
        image_source,
    });
}

/// Applies control-cluster action menu models for the target leaf.
pub(crate) fn apply_control_cluster_menu_actions_for_leaf(
    ui: &AppWindow,
    config: &Config,
    leaf_id: &str,
) {
    let actions =
        button_cluster_actions_for_leaf(&config.ui.layout.button_cluster_instances, leaf_id);
    ui.set_control_cluster_menu_actions(ModelRc::from(Rc::new(VecModel::from(actions))));
}

fn apply_button_cluster_views_to_ui(
    ui: &AppWindow,
    metrics: &layout::TreeLayoutMetrics,
    config: &Config,
) {
    let views: Vec<LayoutButtonClusterPanelModel> = metrics
        .leaves
        .iter()
        .filter(|leaf| leaf.panel == LayoutPanelKind::ButtonCluster)
        .map(|leaf| LayoutButtonClusterPanelModel {
            leaf_id: leaf.id.clone().into(),
            x_px: leaf.x,
            y_px: leaf.y,
            width_px: leaf.width,
            height_px: leaf.height,
            visible: true,
            actions: ModelRc::from(Rc::new(VecModel::from(button_cluster_actions_for_leaf(
                &config.ui.layout.button_cluster_instances,
                &leaf.id,
            )))),
        })
        .collect();
    ui.set_layout_button_cluster_panels(ModelRc::from(Rc::new(VecModel::from(views))));
}

/// Converts a viewer display-priority enum to its UI code.
pub(crate) fn viewer_panel_display_priority_to_code(priority: ViewerPanelDisplayPriority) -> i32 {
    match priority {
        ViewerPanelDisplayPriority::Default => 0,
        ViewerPanelDisplayPriority::PreferSelection => 1,
        ViewerPanelDisplayPriority::PreferNowPlaying => 2,
        ViewerPanelDisplayPriority::SelectionOnly => 3,
        ViewerPanelDisplayPriority::NowPlayingOnly => 4,
    }
}

/// Converts a UI display-priority code into its enum form.
pub(crate) fn viewer_panel_display_priority_from_code(priority: i32) -> ViewerPanelDisplayPriority {
    match priority {
        1 => ViewerPanelDisplayPriority::PreferSelection,
        2 => ViewerPanelDisplayPriority::PreferNowPlaying,
        3 => ViewerPanelDisplayPriority::SelectionOnly,
        4 => ViewerPanelDisplayPriority::NowPlayingOnly,
        _ => ViewerPanelDisplayPriority::Default,
    }
}

/// Converts a viewer metadata-source enum to its UI code.
pub(crate) fn viewer_panel_metadata_source_to_code(source: ViewerPanelMetadataSource) -> i32 {
    match source {
        ViewerPanelMetadataSource::Track => 0,
        ViewerPanelMetadataSource::AlbumDescription => 1,
        ViewerPanelMetadataSource::ArtistBio => 2,
        ViewerPanelMetadataSource::CustomText => 3,
    }
}

/// Converts a UI metadata-source code into its enum form.
pub(crate) fn viewer_panel_metadata_source_from_code(source: i32) -> ViewerPanelMetadataSource {
    match source {
        1 => ViewerPanelMetadataSource::AlbumDescription,
        2 => ViewerPanelMetadataSource::ArtistBio,
        3 => ViewerPanelMetadataSource::CustomText,
        _ => ViewerPanelMetadataSource::Track,
    }
}

/// Converts a viewer image-source enum to its UI code.
pub(crate) fn viewer_panel_image_source_to_code(source: ViewerPanelImageSource) -> i32 {
    match source {
        ViewerPanelImageSource::AlbumArt => 0,
        ViewerPanelImageSource::ArtistImage => 1,
    }
}

/// Converts a UI image-source code into its enum form.
pub(crate) fn viewer_panel_image_source_from_code(source: i32) -> ViewerPanelImageSource {
    match source {
        1 => ViewerPanelImageSource::ArtistImage,
        _ => ViewerPanelImageSource::AlbumArt,
    }
}

fn metadata_viewer_panel_instance_for_leaf<'a>(
    instances: &'a [MetadataViewerPanelInstanceConfig],
    leaf_id: &str,
) -> Option<&'a MetadataViewerPanelInstanceConfig> {
    instances
        .iter()
        .find(|instance| instance.leaf_id == leaf_id)
}

fn album_art_viewer_panel_instance_for_leaf<'a>(
    instances: &'a [AlbumArtViewerPanelInstanceConfig],
    leaf_id: &str,
) -> Option<&'a AlbumArtViewerPanelInstanceConfig> {
    instances
        .iter()
        .find(|instance| instance.leaf_id == leaf_id)
}

/// Returns metadata-viewer display-priority for a leaf with a default fallback.
pub(crate) fn metadata_viewer_panel_display_priority_for_leaf(
    instances: &[MetadataViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelDisplayPriority {
    metadata_viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.display_priority)
        .unwrap_or_default()
}

/// Returns metadata-viewer metadata-source for a leaf with a default fallback.
pub(crate) fn metadata_viewer_panel_metadata_source_for_leaf(
    instances: &[MetadataViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelMetadataSource {
    metadata_viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.metadata_source)
        .unwrap_or_default()
}

/// Returns metadata-viewer template text for a leaf with a default fallback.
pub(crate) fn metadata_viewer_panel_text_format_for_leaf(
    instances: &[MetadataViewerPanelInstanceConfig],
    leaf_id: &str,
) -> String {
    metadata_viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.metadata_text_format.clone())
        .unwrap_or_else(|| crate::text_template::DEFAULT_METADATA_PANEL_TEMPLATE.to_string())
}

/// Returns album-art-viewer display-priority for a leaf with a default fallback.
pub(crate) fn album_art_viewer_panel_display_priority_for_leaf(
    instances: &[AlbumArtViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelDisplayPriority {
    album_art_viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.display_priority)
        .unwrap_or_default()
}

/// Returns album-art-viewer image-source for a leaf with a default fallback.
pub(crate) fn album_art_viewer_panel_image_source_for_leaf(
    instances: &[AlbumArtViewerPanelInstanceConfig],
    leaf_id: &str,
) -> ViewerPanelImageSource {
    album_art_viewer_panel_instance_for_leaf(instances, leaf_id)
        .map(|instance| instance.image_source)
        .unwrap_or_default()
}

fn apply_viewer_panel_views_to_ui(
    ui: &AppWindow,
    metrics: &layout::TreeLayoutMetrics,
    config: &Config,
) {
    let default_art_source = ui.get_current_cover_art();
    let default_has_art = ui.get_current_cover_art_available();
    let default_display_text = RichTextBlock {
        plain_text: "".into(),
        lines: ModelRc::from(Rc::new(VecModel::from(vec![RichTextLine {
            runs: ModelRc::from(Rc::new(VecModel::from(Vec::new()))),
        }]))),
    };
    let existing_metadata_by_leaf: HashMap<String, LayoutMetadataViewerPanelModel> = ui
        .get_layout_metadata_viewer_panels()
        .iter()
        .map(|panel| (panel.leaf_id.to_string(), panel))
        .collect();
    let existing_album_art_by_leaf: HashMap<String, LayoutAlbumArtViewerPanelModel> = ui
        .get_layout_album_art_viewer_panels()
        .iter()
        .map(|panel| (panel.leaf_id.to_string(), panel))
        .collect();

    let metadata_views: Vec<LayoutMetadataViewerPanelModel> = metrics
        .leaves
        .iter()
        .filter(|leaf| leaf.panel == LayoutPanelKind::MetadataViewer)
        .map(|leaf| {
            let display_priority = viewer_panel_display_priority_to_code(
                metadata_viewer_panel_display_priority_for_leaf(
                    &config.ui.layout.metadata_viewer_panel_instances,
                    &leaf.id,
                ),
            );
            let metadata_source = viewer_panel_metadata_source_to_code(
                metadata_viewer_panel_metadata_source_for_leaf(
                    &config.ui.layout.metadata_viewer_panel_instances,
                    &leaf.id,
                ),
            );
            let metadata_text_format = metadata_viewer_panel_text_format_for_leaf(
                &config.ui.layout.metadata_viewer_panel_instances,
                &leaf.id,
            );
            if let Some(existing) = existing_metadata_by_leaf.get(&leaf.id) {
                return LayoutMetadataViewerPanelModel {
                    leaf_id: leaf.id.clone().into(),
                    x_px: leaf.x,
                    y_px: leaf.y,
                    width_px: leaf.width,
                    height_px: leaf.height,
                    visible: true,
                    display_priority,
                    metadata_source,
                    metadata_text_format: metadata_text_format.clone().into(),
                    display_text: existing.display_text.clone(),
                };
            }
            LayoutMetadataViewerPanelModel {
                leaf_id: leaf.id.clone().into(),
                x_px: leaf.x,
                y_px: leaf.y,
                width_px: leaf.width,
                height_px: leaf.height,
                visible: true,
                display_priority,
                metadata_source,
                metadata_text_format: metadata_text_format.into(),
                display_text: default_display_text.clone(),
            }
        })
        .collect();

    let album_art_views: Vec<LayoutAlbumArtViewerPanelModel> = metrics
        .leaves
        .iter()
        .filter(|leaf| leaf.panel == LayoutPanelKind::AlbumArtViewer)
        .map(|leaf| {
            let display_priority = viewer_panel_display_priority_to_code(
                album_art_viewer_panel_display_priority_for_leaf(
                    &config.ui.layout.album_art_viewer_panel_instances,
                    &leaf.id,
                ),
            );
            let image_source =
                viewer_panel_image_source_to_code(album_art_viewer_panel_image_source_for_leaf(
                    &config.ui.layout.album_art_viewer_panel_instances,
                    &leaf.id,
                ));
            if let Some(existing) = existing_album_art_by_leaf.get(&leaf.id) {
                return LayoutAlbumArtViewerPanelModel {
                    leaf_id: leaf.id.clone().into(),
                    x_px: leaf.x,
                    y_px: leaf.y,
                    width_px: leaf.width,
                    height_px: leaf.height,
                    visible: true,
                    display_priority,
                    image_source,
                    art_source: existing.art_source.clone(),
                    has_art: existing.has_art,
                };
            }
            LayoutAlbumArtViewerPanelModel {
                leaf_id: leaf.id.clone().into(),
                x_px: leaf.x,
                y_px: leaf.y,
                width_px: leaf.width,
                height_px: leaf.height,
                visible: true,
                display_priority,
                image_source,
                art_source: default_art_source.clone(),
                has_art: default_has_art,
            }
        })
        .collect();

    ui.set_layout_metadata_viewer_panels(ModelRc::from(Rc::new(VecModel::from(metadata_views))));
    ui.set_layout_album_art_viewer_panels(ModelRc::from(Rc::new(VecModel::from(album_art_views))));
}

/// Returns a copy of `previous` with `layout` installed and sanitized.
pub(crate) fn with_updated_layout(previous: &Config, layout: LayoutConfig) -> Config {
    crate::sanitize_config(Config {
        output: previous.output.clone(),
        cast: previous.cast.clone(),
        ui: UiConfig {
            show_layout_edit_intro: previous.ui.show_layout_edit_intro,
            show_tooltips: previous.ui.show_tooltips,
            auto_scroll_to_playing_track: previous.ui.auto_scroll_to_playing_track,
            legacy_dark_mode: previous.ui.legacy_dark_mode,
            playlist_album_art_column_min_width_px: previous
                .ui
                .playlist_album_art_column_min_width_px,
            playlist_album_art_column_max_width_px: previous
                .ui
                .playlist_album_art_column_max_width_px,
            layout,
            playlist_columns: previous.ui.playlist_columns.clone(),
            window_width: previous.ui.window_width,
            window_height: previous.ui.window_height,
            volume: previous.ui.volume,
            playback_order: previous.ui.playback_order,
            repeat_mode: previous.ui.repeat_mode,
        },
        library: previous.library.clone(),
        buffering: previous.buffering.clone(),
        integrations: previous.integrations.clone(),
    })
}

/// Pushes a layout snapshot onto a bounded stack.
pub(crate) fn push_layout_snapshot(stack: &Arc<Mutex<Vec<LayoutConfig>>>, layout: &LayoutConfig) {
    let mut stack = stack.lock().expect("layout history stack lock poisoned");
    stack.push(layout.clone());
    if stack.len() > 128 {
        let overflow = stack.len() - 128;
        stack.drain(0..overflow);
    }
}

fn clear_layout_snapshot_stack(stack: &Arc<Mutex<Vec<LayoutConfig>>>) {
    stack
        .lock()
        .expect("layout history stack lock poisoned")
        .clear();
}

/// Pushes a layout snapshot to undo history and clears redo history.
pub(crate) fn push_layout_undo_snapshot(
    undo_stack: &Arc<Mutex<Vec<LayoutConfig>>>,
    redo_stack: &Arc<Mutex<Vec<LayoutConfig>>>,
    layout: &LayoutConfig,
) {
    push_layout_snapshot(undo_stack, layout);
    clear_layout_snapshot_stack(redo_stack);
}

/// Pops the most recent layout snapshot from a stack.
pub(crate) fn pop_layout_snapshot(stack: &Arc<Mutex<Vec<LayoutConfig>>>) -> Option<LayoutConfig> {
    stack
        .lock()
        .expect("layout history stack lock poisoned")
        .pop()
}

/// Reuses an existing `VecModel` when possible, otherwise replaces it with a new one.
pub(crate) fn update_or_replace_vec_model<T: Clone + 'static>(
    current_model: ModelRc<T>,
    next_values: Vec<T>,
) -> ModelRc<T> {
    if let Some(vec_model) = current_model.as_any().downcast_ref::<VecModel<T>>() {
        if vec_model.row_count() == next_values.len() {
            for (index, value) in next_values.into_iter().enumerate() {
                vec_model.set_row_data(index, value);
            }
        } else {
            vec_model.set_vec(next_values);
        }
        current_model
    } else {
        ModelRc::from(Rc::new(VecModel::from(next_values)))
    }
}

/// Applies layout-derived models and editor state to the root UI.
pub(crate) fn apply_layout_to_ui(
    ui: &AppWindow,
    config: &Config,
    workspace_width_px: u32,
    workspace_height_px: u32,
) {
    let sanitized_layout =
        sanitize_layout_config(&config.ui.layout, workspace_width_px, workspace_height_px);
    let splitter_thickness_px = if ui.get_layout_edit_mode() {
        SPLITTER_THICKNESS_PX
    } else {
        0
    };
    let metrics = compute_tree_layout_metrics(
        &sanitized_layout,
        workspace_width_px,
        workspace_height_px,
        splitter_thickness_px,
    );

    let leaf_ids: Vec<slint::SharedString> = metrics
        .leaves
        .iter()
        .map(|leaf| leaf.id.clone().into())
        .collect();
    let region_panel_codes: Vec<i32> = metrics
        .leaves
        .iter()
        .map(|leaf| leaf.panel.to_code())
        .collect();
    let region_x: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.x).collect();
    let region_y: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.y).collect();
    let region_widths: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.width).collect();
    let region_heights: Vec<i32> = metrics.leaves.iter().map(|leaf| leaf.height).collect();
    let region_visible: Vec<bool> = vec![true; metrics.leaves.len()];

    let selected_leaf_index = sanitized_layout
        .selected_leaf_id
        .as_ref()
        .and_then(|selected| metrics.leaves.iter().position(|leaf| &leaf.id == selected))
        .map(|index| index as i32)
        .unwrap_or(-1);

    let splitter_model: Vec<LayoutSplitterModel> = metrics
        .splitters
        .iter()
        .map(|splitter| LayoutSplitterModel {
            id: splitter.id.clone().into(),
            axis: splitter.axis.to_code(),
            x_px: splitter.x,
            y_px: splitter.y,
            width_px: splitter.width,
            height_px: splitter.height,
            ratio: splitter.ratio,
            min_ratio: splitter.min_ratio,
            max_ratio: splitter.max_ratio,
            track_start_px: splitter.track_start_px,
            track_length_px: splitter.track_length_px,
        })
        .collect();

    ui.set_layout_leaf_ids(update_or_replace_vec_model(
        ui.get_layout_leaf_ids(),
        leaf_ids,
    ));
    ui.set_layout_region_panel_kinds(update_or_replace_vec_model(
        ui.get_layout_region_panel_kinds(),
        region_panel_codes,
    ));
    ui.set_layout_region_x_px(update_or_replace_vec_model(
        ui.get_layout_region_x_px(),
        region_x,
    ));
    ui.set_layout_region_y_px(update_or_replace_vec_model(
        ui.get_layout_region_y_px(),
        region_y,
    ));
    ui.set_layout_region_width_px(update_or_replace_vec_model(
        ui.get_layout_region_width_px(),
        region_widths,
    ));
    ui.set_layout_region_height_px(update_or_replace_vec_model(
        ui.get_layout_region_height_px(),
        region_heights,
    ));
    ui.set_layout_region_visible(update_or_replace_vec_model(
        ui.get_layout_region_visible(),
        region_visible,
    ));
    ui.set_layout_splitters(update_or_replace_vec_model(
        ui.get_layout_splitters(),
        splitter_model,
    ));
    ui.set_layout_selected_leaf_index(selected_leaf_index);
    apply_button_cluster_views_to_ui(ui, &metrics, config);
    apply_viewer_panel_views_to_ui(ui, &metrics, config);
}

#[cfg(test)]
mod tests {
    use std::{any::Any, rc::Rc};

    use slint::{Model, ModelRc, ModelTracker, VecModel};

    use super::{
        default_button_cluster_actions_by_index, quantize_splitter_ratio_to_precision,
        sidebar_width_from_window, update_or_replace_vec_model,
    };

    #[test]
    fn test_update_or_replace_vec_model_reuses_existing_vec_model_when_len_matches() {
        let current: ModelRc<i32> = ModelRc::from(Rc::new(VecModel::from(vec![1, 2])));
        let original_ptr = current
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected") as *const VecModel<i32>;

        let updated = update_or_replace_vec_model(current.clone(), vec![9, 8]);
        let updated_model = updated
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected");
        let updated_ptr = updated_model as *const VecModel<i32>;

        assert_eq!(original_ptr, updated_ptr);
        assert_eq!(updated_model.row_count(), 2);
        assert_eq!(updated_model.row_data(0), Some(9));
        assert_eq!(updated_model.row_data(1), Some(8));
    }

    #[test]
    fn test_update_or_replace_vec_model_reuses_existing_vec_model_when_len_changes() {
        let current: ModelRc<i32> = ModelRc::from(Rc::new(VecModel::from(vec![1, 2])));
        let original_ptr = current
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected") as *const VecModel<i32>;

        let updated = update_or_replace_vec_model(current.clone(), vec![7, 6, 5]);
        let updated_model = updated
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected");
        let updated_ptr = updated_model as *const VecModel<i32>;

        assert_eq!(original_ptr, updated_ptr);
        assert_eq!(updated_model.row_count(), 3);
        assert_eq!(updated_model.row_data(0), Some(7));
        assert_eq!(updated_model.row_data(1), Some(6));
        assert_eq!(updated_model.row_data(2), Some(5));
    }

    #[test]
    fn test_update_or_replace_vec_model_falls_back_to_vec_model_for_non_vec_model_inputs() {
        struct StaticModel {
            values: Vec<i32>,
        }

        impl Model for StaticModel {
            type Data = i32;

            fn row_count(&self) -> usize {
                self.values.len()
            }

            fn row_data(&self, row: usize) -> Option<Self::Data> {
                self.values.get(row).copied()
            }

            fn model_tracker(&self) -> &dyn ModelTracker {
                &()
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        let current: ModelRc<i32> = ModelRc::new(StaticModel { values: vec![1, 2] });
        assert!(current.as_any().downcast_ref::<VecModel<i32>>().is_none());

        let updated = update_or_replace_vec_model(current, vec![4, 3]);
        let updated_model = updated
            .as_any()
            .downcast_ref::<VecModel<i32>>()
            .expect("VecModel expected after fallback creation");
        assert_eq!(updated_model.row_count(), 2);
        assert_eq!(updated_model.row_data(0), Some(4));
        assert_eq!(updated_model.row_data(1), Some(3));
    }

    #[test]
    fn test_quantize_splitter_ratio_to_precision_rounds_to_two_decimals() {
        assert_eq!(quantize_splitter_ratio_to_precision(0.1234), 0.12);
        assert_eq!(quantize_splitter_ratio_to_precision(0.1251), 0.13);
        assert_eq!(quantize_splitter_ratio_to_precision(0.9999), 1.0);
    }

    #[test]
    fn test_quantize_splitter_ratio_to_precision_keeps_non_finite_values() {
        assert!(quantize_splitter_ratio_to_precision(f32::INFINITY).is_infinite());
        assert!(quantize_splitter_ratio_to_precision(f32::NEG_INFINITY).is_infinite());
        assert!(quantize_splitter_ratio_to_precision(f32::NAN).is_nan());
    }

    #[test]
    fn test_sidebar_width_from_window_is_responsive_and_clamped() {
        assert_eq!(sidebar_width_from_window(600), 180);
        assert_eq!(sidebar_width_from_window(900), 216);
        assert_eq!(sidebar_width_from_window(2_000), 300);
    }

    #[test]
    fn test_default_utility_button_cluster_preset_excludes_layout_editor_action() {
        assert_eq!(
            default_button_cluster_actions_by_index(2),
            vec![7, 8, 11, 10],
            "Utility preset should not include layout editor by default"
        );
    }
}
