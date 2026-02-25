//! Theme preset catalog and runtime color resolution.

use std::{collections::HashMap, sync::OnceLock};

use slint::language::ColorScheme;

use crate::layout::{LayoutConfig, ThemeColorComponents, DEFAULT_COLOR_SCHEME_ID};

/// Stable identifier for custom user-defined color overrides.
pub const CUSTOM_COLOR_SCHEME_ID: &str = "custom";
/// Stable identifier for built-in roqtune light preset.
pub const ROQTUNE_LIGHT_SCHEME_ID: &str = "roqtune_light";

/// Names shown in the custom color editor and persisted custom value order.
pub const COLOR_COMPONENT_DEFINITIONS: [(&str, &str); 15] = [
    ("window_bg", "Window Background"),
    ("panel_bg", "Panel Background"),
    ("panel_bg_alt", "Panel Background (Alt)"),
    ("border", "Border"),
    ("text_primary", "Text Primary"),
    ("text_secondary", "Text Secondary"),
    ("text_muted", "Text Muted"),
    ("accent", "Accent"),
    ("accent_on", "Accent On"),
    ("warning", "Warning"),
    ("danger", "Danger"),
    ("success", "Success"),
    ("control_hover_bg", "Control Hover Background"),
    ("selection_bg", "Selection Background"),
    ("selection_border", "Selection Border"),
];

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSchemeMode {
    Dark,
    Light,
}

impl ThemeSchemeMode {
    /// Converts persisted scheme mode to Slint palette mode.
    pub fn to_slint_color_scheme(self) -> ColorScheme {
        match self {
            Self::Dark => ColorScheme::Dark,
            Self::Light => ColorScheme::Light,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ThemePreset {
    pub id: String,
    pub label: String,
    pub mode: ThemeSchemeMode,
    #[serde(flatten)]
    pub colors: ThemeColorComponents,
}

#[derive(Debug, serde::Deserialize)]
struct ThemePresetCatalogWire {
    schemes: Vec<ThemePreset>,
}

/// Parsed theme preset catalog and lookup indexes.
#[derive(Debug)]
pub struct ThemePresetCatalog {
    presets: Vec<ThemePreset>,
    index_by_id: HashMap<String, usize>,
}

impl ThemePresetCatalog {
    fn preset_by_id(&self, id: &str) -> Option<&ThemePreset> {
        let key = id.trim().to_ascii_lowercase();
        self.index_by_id
            .get(&key)
            .and_then(|index| self.presets.get(*index))
    }

    fn default_preset(&self) -> &ThemePreset {
        self.preset_by_id(DEFAULT_COLOR_SCHEME_ID)
            .or_else(|| self.presets.first())
            .expect("theme preset catalog must contain at least one scheme")
    }

    /// Returns immutable preset list in display order.
    pub fn presets(&self) -> &[ThemePreset] {
        &self.presets
    }
}

/// Resolved active theme for UI rendering.
#[derive(Debug, Clone)]
pub struct ResolvedTheme {
    pub mode: ThemeSchemeMode,
    pub colors: ThemeColorComponents,
}

static THEME_PRESET_CATALOG: OnceLock<ThemePresetCatalog> = OnceLock::new();

/// Returns the parsed built-in theme preset catalog.
pub fn theme_preset_catalog() -> &'static ThemePresetCatalog {
    THEME_PRESET_CATALOG.get_or_init(load_theme_preset_catalog)
}

fn load_theme_preset_catalog() -> ThemePresetCatalog {
    let wire: ThemePresetCatalogWire = toml::from_str(include_str!("../config/color_schemes.toml"))
        .expect("color scheme catalog should parse");

    let mut presets = Vec::new();
    let mut index_by_id = HashMap::new();
    for preset in wire.schemes {
        let id = preset.id.trim();
        let label = preset.label.trim();
        if id.is_empty() || label.is_empty() {
            continue;
        }
        let normalized_id = id.to_ascii_lowercase();
        if index_by_id.contains_key(&normalized_id) {
            continue;
        }
        index_by_id.insert(normalized_id, presets.len());
        presets.push(ThemePreset {
            id: id.to_string(),
            label: label.to_string(),
            mode: preset.mode,
            colors: sanitize_color_components(&preset.colors, &preset.colors),
        });
    }
    ThemePresetCatalog {
        presets,
        index_by_id,
    }
}

/// Returns `(id, label)` options for the settings combo, with custom last.
pub fn scheme_picker_options() -> Vec<(String, String)> {
    let catalog = theme_preset_catalog();
    let mut options: Vec<(String, String)> = catalog
        .presets()
        .iter()
        .map(|preset| (preset.id.clone(), preset.label.clone()))
        .collect();
    options.push((CUSTOM_COLOR_SCHEME_ID.to_string(), "Custom".to_string()));
    options
}

/// Resolves one selected scheme from persisted layout state.
pub fn resolve_theme(layout: &LayoutConfig) -> ResolvedTheme {
    let catalog = theme_preset_catalog();
    let default = catalog.default_preset();
    let selected = layout.color_scheme.trim();
    if selected.eq_ignore_ascii_case(CUSTOM_COLOR_SCHEME_ID) {
        let fallback = default.colors.clone();
        let colors = layout
            .custom_colors
            .as_ref()
            .map(|custom| sanitize_color_components(custom, &fallback))
            .unwrap_or(fallback);
        let mode = infer_mode_from_window_bg(&colors).unwrap_or(default.mode);
        return ResolvedTheme { mode, colors };
    }

    let preset = catalog.preset_by_id(selected).unwrap_or(default);
    ResolvedTheme {
        mode: preset.mode,
        colors: preset.colors.clone(),
    }
}

/// Selects the combo index for a persisted scheme id.
pub fn scheme_index_for_id(scheme_id: &str) -> i32 {
    let options = scheme_picker_options();
    options
        .iter()
        .position(|(id, _)| id.eq_ignore_ascii_case(scheme_id.trim()))
        .unwrap_or(0) as i32
}

/// Returns the persisted scheme id for one combo index.
pub fn scheme_id_for_index(index: i32) -> String {
    let options = scheme_picker_options();
    let idx = index.max(0) as usize;
    options
        .get(idx)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| DEFAULT_COLOR_SCHEME_ID.to_string())
}

/// Returns display labels for each custom color component in persisted order.
pub fn custom_color_component_labels() -> Vec<String> {
    COLOR_COMPONENT_DEFINITIONS
        .iter()
        .map(|(_, label)| (*label).to_string())
        .collect()
}

/// Converts colors to ordered hex values for settings UI models.
pub fn custom_color_values_for_ui(colors: &ThemeColorComponents) -> Vec<String> {
    COLOR_COMPONENT_DEFINITIONS
        .iter()
        .map(|(key, _)| color_component_value(colors, key))
        .collect()
}

/// Rebuilds custom colors from ordered settings values and validates them.
pub fn custom_colors_from_ui_values(
    values: &[String],
    fallback: &ThemeColorComponents,
) -> ThemeColorComponents {
    let mut candidate = fallback.clone();
    for (index, (key, _)) in COLOR_COMPONENT_DEFINITIONS.iter().enumerate() {
        if let Some(value) = values.get(index) {
            set_color_component_value(&mut candidate, key, value);
        }
    }
    sanitize_color_components(&candidate, fallback)
}

fn color_component_value(colors: &ThemeColorComponents, key: &str) -> String {
    match key {
        "window_bg" => colors.window_bg.clone(),
        "panel_bg" => colors.panel_bg.clone(),
        "panel_bg_alt" => colors.panel_bg_alt.clone(),
        "border" => colors.border.clone(),
        "text_primary" => colors.text_primary.clone(),
        "text_secondary" => colors.text_secondary.clone(),
        "text_muted" => colors.text_muted.clone(),
        "accent" => colors.accent.clone(),
        "accent_on" => colors.accent_on.clone(),
        "warning" => colors.warning.clone(),
        "danger" => colors.danger.clone(),
        "success" => colors.success.clone(),
        "control_hover_bg" => colors.control_hover_bg.clone(),
        "selection_bg" => colors.selection_bg.clone(),
        "selection_border" => colors.selection_border.clone(),
        _ => String::new(),
    }
}

fn set_color_component_value(colors: &mut ThemeColorComponents, key: &str, value: &str) {
    let normalized = normalize_hex_color(value).unwrap_or_else(|| value.trim().to_string());
    match key {
        "window_bg" => colors.window_bg = normalized,
        "panel_bg" => colors.panel_bg = normalized,
        "panel_bg_alt" => colors.panel_bg_alt = normalized,
        "border" => colors.border = normalized,
        "text_primary" => colors.text_primary = normalized,
        "text_secondary" => colors.text_secondary = normalized,
        "text_muted" => colors.text_muted = normalized,
        "accent" => colors.accent = normalized,
        "accent_on" => colors.accent_on = normalized,
        "warning" => colors.warning = normalized,
        "danger" => colors.danger = normalized,
        "success" => colors.success = normalized,
        "control_hover_bg" => colors.control_hover_bg = normalized,
        "selection_bg" => colors.selection_bg = normalized,
        "selection_border" => colors.selection_border = normalized,
        _ => {}
    }
}

/// Parses one hex color into Slint color.
pub fn parse_slint_color(value: &str) -> Option<slint::Color> {
    let normalized = normalize_hex_color(value)?;
    let bytes = normalized.as_bytes();
    if bytes.len() == 7 {
        let r = parse_hex_byte(&normalized[1..3])?;
        let g = parse_hex_byte(&normalized[3..5])?;
        let b = parse_hex_byte(&normalized[5..7])?;
        return Some(slint::Color::from_rgb_u8(r, g, b));
    }
    if bytes.len() == 9 {
        let r = parse_hex_byte(&normalized[1..3])?;
        let g = parse_hex_byte(&normalized[3..5])?;
        let b = parse_hex_byte(&normalized[5..7])?;
        let a = parse_hex_byte(&normalized[7..9])?;
        return Some(slint::Color::from_argb_u8(a, r, g, b));
    }
    None
}

fn sanitize_color_components(
    candidate: &ThemeColorComponents,
    fallback: &ThemeColorComponents,
) -> ThemeColorComponents {
    ThemeColorComponents {
        window_bg: normalized_or_fallback(&candidate.window_bg, &fallback.window_bg),
        panel_bg: normalized_or_fallback(&candidate.panel_bg, &fallback.panel_bg),
        panel_bg_alt: normalized_or_fallback(&candidate.panel_bg_alt, &fallback.panel_bg_alt),
        border: normalized_or_fallback(&candidate.border, &fallback.border),
        text_primary: normalized_or_fallback(&candidate.text_primary, &fallback.text_primary),
        text_secondary: normalized_or_fallback(&candidate.text_secondary, &fallback.text_secondary),
        text_muted: normalized_or_fallback(&candidate.text_muted, &fallback.text_muted),
        accent: normalized_or_fallback(&candidate.accent, &fallback.accent),
        accent_on: normalized_or_fallback(&candidate.accent_on, &fallback.accent_on),
        warning: normalized_or_fallback(&candidate.warning, &fallback.warning),
        danger: normalized_or_fallback(&candidate.danger, &fallback.danger),
        success: normalized_or_fallback(&candidate.success, &fallback.success),
        control_hover_bg: normalized_or_fallback(
            &candidate.control_hover_bg,
            &fallback.control_hover_bg,
        ),
        selection_bg: normalized_or_fallback(&candidate.selection_bg, &fallback.selection_bg),
        selection_border: normalized_or_fallback(
            &candidate.selection_border,
            &fallback.selection_border,
        ),
    }
}

fn normalized_or_fallback(value: &str, fallback: &str) -> String {
    normalize_hex_color(value).unwrap_or_else(|| fallback.to_string())
}

fn normalize_hex_color(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if !(trimmed.len() == 7 || trimmed.len() == 9) {
        return None;
    }
    if !trimmed.starts_with('#') {
        return None;
    }
    if !trimmed[1..].chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", trimmed[1..].to_ascii_uppercase()))
}

fn parse_hex_byte(value: &str) -> Option<u8> {
    u8::from_str_radix(value, 16).ok()
}

fn infer_mode_from_window_bg(colors: &ThemeColorComponents) -> Option<ThemeSchemeMode> {
    let normalized = normalize_hex_color(&colors.window_bg)?;
    if normalized.len() < 7 {
        return None;
    }
    let r = parse_hex_byte(&normalized[1..3])? as f32 / 255.0;
    let g = parse_hex_byte(&normalized[3..5])? as f32 / 255.0;
    let b = parse_hex_byte(&normalized[5..7])? as f32 / 255.0;
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luma >= 0.55 {
        Some(ThemeSchemeMode::Light)
    } else {
        Some(ThemeSchemeMode::Dark)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_slint_color, scheme_picker_options, theme_preset_catalog, CUSTOM_COLOR_SCHEME_ID,
    };

    #[test]
    fn test_theme_preset_catalog_contains_expected_compact_set() {
        let options: Vec<String> = theme_preset_catalog()
            .presets()
            .iter()
            .map(|preset| preset.id.clone())
            .collect();
        assert!(options.contains(&"roqtune_dark".to_string()));
        assert!(options.contains(&"roqtune_light".to_string()));
        assert!(options.contains(&"solarized_dark".to_string()));
        assert!(options.contains(&"solarized_light".to_string()));
        assert!(options.contains(&"dracula_dark".to_string()));
        assert!(options.contains(&"nord_dark".to_string()));
    }

    #[test]
    fn test_theme_picker_options_include_custom_last() {
        let options = scheme_picker_options();
        assert_eq!(
            options.last().map(|(id, _)| id.as_str()),
            Some(CUSTOM_COLOR_SCHEME_ID)
        );
    }

    #[test]
    fn test_parse_slint_color_accepts_rgb_and_rgba_hex() {
        assert!(parse_slint_color("#2A6DEF").is_some());
        assert!(parse_slint_color("#2A6DEFCC").is_some());
        assert!(parse_slint_color("invalid").is_none());
    }
}
