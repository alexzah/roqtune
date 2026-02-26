//! Shared template language parser/evaluator for playlist and metadata text.

use std::path::Path;

const DEFAULT_FONT_SIZE_PX: u32 = 13;

pub(crate) const DEFAULT_METADATA_PANEL_TEMPLATE: &str =
    "[size=title][b][color=text_primary]{title;file_name}[/color][/b][/size]\\n[size=body][color=text_secondary]{artist;album_artist}[/color][/size]\\n[size=body][color=text_muted]{album}[/color][/size]\\n[size=caption][color=text_muted]{date;year} {genre}[/color][/size]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteColor {
    Accent,
    AccentOn,
    Warning,
    Danger,
    Success,
    TextPrimary,
    TextSecondary,
    TextMuted,
    TextDisabled,
    Border,
    PanelBg,
    PanelBgAlt,
    WindowBg,
    SelectionBg,
    SelectionBorder,
    ControlHoverBg,
}

impl PaletteColor {
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::Accent => 1,
            Self::AccentOn => 2,
            Self::Warning => 3,
            Self::Danger => 4,
            Self::Success => 5,
            Self::TextPrimary => 6,
            Self::TextSecondary => 7,
            Self::TextMuted => 8,
            Self::TextDisabled => 9,
            Self::Border => 10,
            Self::PanelBg => 11,
            Self::PanelBgAlt => 12,
            Self::WindowBg => 13,
            Self::SelectionBg => 14,
            Self::SelectionBorder => 15,
            Self::ControlHoverBg => 16,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        let normalized = normalize_name(name);
        match normalized.as_str() {
            "accent" => Some(Self::Accent),
            "accent_on" | "accenton" => Some(Self::AccentOn),
            "warning" => Some(Self::Warning),
            "danger" => Some(Self::Danger),
            "success" => Some(Self::Success),
            "text_primary" | "textprimary" => Some(Self::TextPrimary),
            "text_secondary" | "textsecondary" => Some(Self::TextSecondary),
            "text_muted" | "textmuted" => Some(Self::TextMuted),
            "text_disabled" | "textdisabled" => Some(Self::TextDisabled),
            "border" => Some(Self::Border),
            "panel_bg" | "panelbg" => Some(Self::PanelBg),
            "panel_bg_alt" | "panelbgalt" => Some(Self::PanelBgAlt),
            "window_bg" | "windowbg" => Some(Self::WindowBg),
            "selection_bg" | "selectionbg" => Some(Self::SelectionBg),
            "selection_border" | "selectionborder" => Some(Self::SelectionBorder),
            "control_hover_bg" | "controlhoverbg" => Some(Self::ControlHoverBg),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RunColor {
    Palette(PaletteColor),
    Rgba { r: u8, g: u8, b: u8, a: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RichTextRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub font_size_px: u32,
    pub font_family: String,
    pub color: Option<RunColor>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RichTextLine {
    pub runs: Vec<RichTextRun>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RenderedText {
    pub plain_text: String,
    pub lines: Vec<RichTextLine>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TemplateMetrics {
    pub explicit_line_count: u32,
    pub max_font_size_px: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RenderOptions {
    pub base_font_size_px: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            base_font_size_px: DEFAULT_FONT_SIZE_PX,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontSizeRole {
    Micro,
    Caption,
    Body,
    BodyLarge,
    Title,
    H3,
    H2,
    H1,
    Spotlight,
}

impl FontSizeRole {
    fn from_name(name: &str) -> Option<Self> {
        let normalized = normalize_name(name);
        match normalized.as_str() {
            "micro" => Some(Self::Micro),
            "caption" => Some(Self::Caption),
            "body" => Some(Self::Body),
            "body_large" | "bodylarge" => Some(Self::BodyLarge),
            "title" => Some(Self::Title),
            "h3" => Some(Self::H3),
            "h2" => Some(Self::H2),
            "h1" => Some(Self::H1),
            "spotlight" => Some(Self::Spotlight),
            _ => None,
        }
    }

    fn scale_percent(self) -> u32 {
        match self {
            Self::Micro => 70,
            Self::Caption => 82,
            Self::Body => 100,
            Self::BodyLarge => 115,
            Self::Title => 130,
            Self::H3 => 150,
            Self::H2 => 175,
            Self::H1 => 205,
            Self::Spotlight => 245,
        }
    }

    fn resolve_px(self, base_font_size_px: u32) -> u32 {
        let base = base_font_size_px.max(1);
        let scaled = base.saturating_mul(self.scale_percent());
        scaled.saturating_add(50) / 100
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontSizeSpec {
    Px(u32),
    Role(FontSizeRole),
}

impl FontSizeSpec {
    fn resolve_px(self, options: RenderOptions) -> u32 {
        match self {
            Self::Px(size) => size.max(1),
            Self::Role(role) => role.resolve_px(options.base_font_size_px),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TemplateContext<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub album_artist: &'a str,
    pub date: &'a str,
    pub year: &'a str,
    pub genre: &'a str,
    pub track_number: &'a str,
    pub file_name: Option<&'a str>,
    pub path: Option<&'a str>,
}

impl<'a> TemplateContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_path_metadata(
        title: &'a str,
        artist: &'a str,
        album: &'a str,
        album_artist: &'a str,
        date: &'a str,
        year: &'a str,
        genre: &'a str,
        track_number: &'a str,
        path: Option<&'a Path>,
    ) -> Self {
        let file_name = path
            .and_then(|value| value.file_name())
            .and_then(|value| value.to_str());
        let path_text = path.and_then(|value| value.to_str());
        Self {
            title,
            artist,
            album,
            album_artist,
            date,
            year,
            genre,
            track_number,
            file_name,
            path: path_text,
        }
    }

    fn value_for_key(&self, key: &str) -> Option<String> {
        let normalized = normalize_name(key);
        match normalized.as_str() {
            "title" => Some(self.title.to_string()),
            "artist" => Some(self.artist.to_string()),
            "album" => Some(self.album.to_string()),
            "album_artist" | "albumartist" => Some(self.album_artist.to_string()),
            "date" => Some(self.date.to_string()),
            "year" => {
                if !self.year.is_empty() {
                    Some(self.year.to_string())
                } else if self.date.len() >= 4 {
                    Some(self.date[0..4].to_string())
                } else {
                    Some(String::new())
                }
            }
            "genre" => Some(self.genre.to_string()),
            "track" | "track_number" | "tracknumber" => Some(self.track_number.to_string()),
            "file_name" | "filename" | "file" => {
                Some(self.file_name.unwrap_or_default().to_string())
            }
            "path" => Some(self.path.unwrap_or_default().to_string()),
            "album_art" | "favorite" | "disc" | "disc_number" | "duration" => Some(String::new()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedTemplate {
    segments: Vec<TemplateSegment>,
    metrics: TemplateMetrics,
}

#[derive(Clone, Debug)]
enum TemplateSegment {
    Text(String),
    Placeholder { raw: String, fallbacks: Vec<String> },
    NewLine,
    OpenStyle(StyleTag),
    CloseStyle { kind: StyleKind, literal: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StyleKind {
    Bold,
    Italic,
    Underline,
    Size,
    Font,
    Color,
}

#[derive(Clone, Debug)]
enum StyleTag {
    Bold,
    Italic,
    Underline,
    Size(FontSizeSpec),
    Font(String),
    Color(RunColor),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct StyleState {
    bold: bool,
    italic: bool,
    underline: bool,
    font_size_px: u32,
    font_family: String,
    color: Option<RunColor>,
}

pub(crate) fn parse_template(source: &str) -> ParsedTemplate {
    let mut segments = Vec::new();
    let mut text_buffer = String::new();
    let mut chars = source.chars().peekable();
    let mut max_font_size_px = DEFAULT_FONT_SIZE_PX;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    match next {
                        'n' => {
                            flush_text(&mut segments, &mut text_buffer);
                            segments.push(TemplateSegment::NewLine);
                        }
                        '{' | '}' | '[' | ']' | '\\' => text_buffer.push(next),
                        _ => {
                            text_buffer.push('\\');
                            text_buffer.push(next);
                        }
                    }
                } else {
                    text_buffer.push('\\');
                }
            }
            '\n' => {
                flush_text(&mut segments, &mut text_buffer);
                segments.push(TemplateSegment::NewLine);
            }
            '{' => {
                flush_text(&mut segments, &mut text_buffer);
                if let Some(content) = read_until(&mut chars, '}') {
                    let trimmed = content.trim();
                    if trimmed.is_empty() {
                        segments.push(TemplateSegment::Text(format!("{{{content}}}")));
                        continue;
                    }
                    let mut fallbacks = Vec::new();
                    let mut invalid = false;
                    for part in content.split(';') {
                        let key = part.trim();
                        if key.is_empty() {
                            invalid = true;
                            break;
                        }
                        fallbacks.push(key.to_string());
                    }
                    if invalid || fallbacks.is_empty() {
                        segments.push(TemplateSegment::Text(format!("{{{content}}}")));
                    } else {
                        segments.push(TemplateSegment::Placeholder {
                            raw: content,
                            fallbacks,
                        });
                    }
                } else {
                    text_buffer.push('{');
                }
            }
            '[' => {
                flush_text(&mut segments, &mut text_buffer);
                if let Some(content) = read_until(&mut chars, ']') {
                    match parse_tag_segment(&content) {
                        Some(ParsedTag::NewLine) => {
                            segments.push(TemplateSegment::NewLine);
                        }
                        Some(ParsedTag::Open(style)) => {
                            if let StyleTag::Size(size_spec) = style {
                                max_font_size_px = max_font_size_px
                                    .max(size_spec.resolve_px(RenderOptions::default()));
                                segments
                                    .push(TemplateSegment::OpenStyle(StyleTag::Size(size_spec)));
                            } else {
                                segments.push(TemplateSegment::OpenStyle(style));
                            }
                        }
                        Some(ParsedTag::Close { kind, literal }) => {
                            segments.push(TemplateSegment::CloseStyle { kind, literal });
                        }
                        None => {
                            segments.push(TemplateSegment::Text(format!("[{content}]")));
                        }
                    }
                } else {
                    text_buffer.push('[');
                }
            }
            _ => text_buffer.push(ch),
        }
    }
    flush_text(&mut segments, &mut text_buffer);

    let explicit_line_count = segments.iter().fold(1u32, |count, segment| match segment {
        TemplateSegment::NewLine => count.saturating_add(1),
        _ => count,
    });
    ParsedTemplate {
        segments,
        metrics: TemplateMetrics {
            explicit_line_count,
            max_font_size_px,
        },
    }
}

pub(crate) fn render(
    parsed: &ParsedTemplate,
    context: &TemplateContext<'_>,
    render_options: RenderOptions,
) -> RenderedText {
    let mut lines = vec![RichTextLine { runs: Vec::new() }];
    let mut plain_text = String::new();
    let mut style_stack: Vec<StyleTag> = Vec::new();

    for segment in &parsed.segments {
        match segment {
            TemplateSegment::Text(text) => {
                append_text(
                    &mut lines,
                    &mut plain_text,
                    text,
                    &style_stack,
                    render_options,
                );
            }
            TemplateSegment::Placeholder { raw, fallbacks } => {
                if let Some(value) = resolve_placeholder(context, fallbacks) {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        &value,
                        &style_stack,
                        render_options,
                    );
                } else {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        &format!("{{{raw}}}"),
                        &style_stack,
                        render_options,
                    );
                }
            }
            TemplateSegment::NewLine => {
                plain_text.push('\n');
                lines.push(RichTextLine { runs: Vec::new() });
            }
            TemplateSegment::OpenStyle(style) => {
                style_stack.push(style.clone());
            }
            TemplateSegment::CloseStyle { kind, literal } => {
                if style_stack.last().map(style_kind) == Some(*kind) {
                    style_stack.pop();
                } else {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        literal,
                        &style_stack,
                        render_options,
                    );
                }
            }
        }
    }

    RenderedText { plain_text, lines }
}

pub(crate) fn render_template(source: &str, context: &TemplateContext<'_>) -> RenderedText {
    let parsed = parse_template(source);
    render(&parsed, context, RenderOptions::default())
}

pub(crate) fn render_template_with_options(
    source: &str,
    context: &TemplateContext<'_>,
    render_options: RenderOptions,
) -> RenderedText {
    let parsed = parse_template(source);
    render(&parsed, context, render_options)
}

pub(crate) fn template_metrics(source: &str) -> TemplateMetrics {
    parse_template(source).metrics
}

fn resolve_placeholder(context: &TemplateContext<'_>, fallbacks: &[String]) -> Option<String> {
    let mut recognized_any = false;
    for key in fallbacks {
        if let Some(value) = context.value_for_key(key) {
            recognized_any = true;
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    if recognized_any {
        Some(String::new())
    } else {
        None
    }
}

fn append_text(
    lines: &mut Vec<RichTextLine>,
    plain_text: &mut String,
    text: &str,
    style_stack: &[StyleTag],
    render_options: RenderOptions,
) {
    let state = style_state(style_stack, render_options);
    let mut first = true;
    for part in text.split('\n') {
        if !first {
            plain_text.push('\n');
            lines.push(RichTextLine { runs: Vec::new() });
        }
        first = false;
        if part.is_empty() {
            continue;
        }
        plain_text.push_str(part);
        let run = RichTextRun {
            text: part.to_string(),
            bold: state.bold,
            italic: state.italic,
            underline: state.underline,
            font_size_px: state.font_size_px,
            font_family: state.font_family.clone(),
            color: state.color.clone(),
        };
        if let Some(line) = lines.last_mut() {
            if let Some(previous) = line.runs.last_mut() {
                if previous.bold == run.bold
                    && previous.italic == run.italic
                    && previous.underline == run.underline
                    && previous.font_size_px == run.font_size_px
                    && previous.font_family == run.font_family
                    && previous.color == run.color
                {
                    previous.text.push_str(&run.text);
                    continue;
                }
            }
            line.runs.push(run);
        }
    }
}

fn style_state(style_stack: &[StyleTag], render_options: RenderOptions) -> StyleState {
    let mut state = StyleState {
        font_size_px: render_options.base_font_size_px.max(1),
        ..StyleState::default()
    };
    for style in style_stack {
        match style {
            StyleTag::Bold => state.bold = true,
            StyleTag::Italic => state.italic = true,
            StyleTag::Underline => state.underline = true,
            StyleTag::Size(size_spec) => state.font_size_px = size_spec.resolve_px(render_options),
            StyleTag::Font(font) => state.font_family = font.clone(),
            StyleTag::Color(color) => state.color = Some(color.clone()),
        }
    }
    state
}

fn style_kind(style: &StyleTag) -> StyleKind {
    match style {
        StyleTag::Bold => StyleKind::Bold,
        StyleTag::Italic => StyleKind::Italic,
        StyleTag::Underline => StyleKind::Underline,
        StyleTag::Size(_) => StyleKind::Size,
        StyleTag::Font(_) => StyleKind::Font,
        StyleTag::Color(_) => StyleKind::Color,
    }
}

fn normalize_name(value: &str) -> String {
    let mut normalized = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
        } else if ch == '_' || ch == '-' || ch.is_whitespace() {
            normalized.push('_');
        }
    }
    normalized
}

fn flush_text(segments: &mut Vec<TemplateSegment>, text_buffer: &mut String) {
    if text_buffer.is_empty() {
        return;
    }
    segments.push(TemplateSegment::Text(std::mem::take(text_buffer)));
}

fn read_until(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    terminal: char,
) -> Option<String> {
    let mut collected = String::new();
    for ch in chars.by_ref() {
        if ch == terminal {
            return Some(collected);
        }
        collected.push(ch);
    }
    None
}

enum ParsedTag {
    NewLine,
    Open(StyleTag),
    Close { kind: StyleKind, literal: String },
}

fn parse_tag_segment(content: &str) -> Option<ParsedTag> {
    let trimmed = content.trim();
    if trimmed.eq_ignore_ascii_case("br") {
        return Some(ParsedTag::NewLine);
    }

    if let Some(rest) = trimmed.strip_prefix('/') {
        let name = normalize_name(rest);
        return match name.as_str() {
            "b" => Some(ParsedTag::Close {
                kind: StyleKind::Bold,
                literal: format!("[/{rest}]"),
            }),
            "i" => Some(ParsedTag::Close {
                kind: StyleKind::Italic,
                literal: format!("[/{rest}]"),
            }),
            "u" => Some(ParsedTag::Close {
                kind: StyleKind::Underline,
                literal: format!("[/{rest}]"),
            }),
            "size" => Some(ParsedTag::Close {
                kind: StyleKind::Size,
                literal: format!("[/{rest}]"),
            }),
            "font" => Some(ParsedTag::Close {
                kind: StyleKind::Font,
                literal: format!("[/{rest}]"),
            }),
            "color" => Some(ParsedTag::Close {
                kind: StyleKind::Color,
                literal: format!("[/{rest}]"),
            }),
            _ => None,
        };
    }

    if let Some((name_raw, value_raw)) = trimmed.split_once('=') {
        let name = normalize_name(name_raw);
        let value = value_raw.trim();
        if value.is_empty() {
            return None;
        }
        return match name.as_str() {
            "size" => parse_size_spec(value).map(|size| ParsedTag::Open(StyleTag::Size(size))),
            "font" => Some(ParsedTag::Open(StyleTag::Font(value.to_string()))),
            "color" => parse_color(value).map(|color| ParsedTag::Open(StyleTag::Color(color))),
            _ => None,
        };
    }

    let name = normalize_name(trimmed);
    match name.as_str() {
        "b" => Some(ParsedTag::Open(StyleTag::Bold)),
        "i" => Some(ParsedTag::Open(StyleTag::Italic)),
        "u" => Some(ParsedTag::Open(StyleTag::Underline)),
        _ => None,
    }
}

fn parse_size_spec(value: &str) -> Option<FontSizeSpec> {
    if let Ok(size) = value.parse::<u32>() {
        return (size > 0).then_some(FontSizeSpec::Px(size));
    }
    FontSizeRole::from_name(value).map(FontSizeSpec::Role)
}

fn parse_color(value: &str) -> Option<RunColor> {
    if let Some((r, g, b, a)) = parse_hex_color(value) {
        return Some(RunColor::Rgba {
            r,
            g,
            b,
            a: (a as f32) / 255.0,
        });
    }
    PaletteColor::from_name(value).map(RunColor::Palette)
}

fn parse_hex_color(value: &str) -> Option<(u8, u8, u8, u8)> {
    let hex = value.strip_prefix('#')?;
    let expanded = match hex.len() {
        3 => {
            let mut out = String::with_capacity(6);
            for ch in hex.chars() {
                out.push(ch);
                out.push(ch);
            }
            out
        }
        4 => {
            let mut out = String::with_capacity(8);
            for ch in hex.chars() {
                out.push(ch);
                out.push(ch);
            }
            out
        }
        6 | 8 => hex.to_string(),
        _ => return None,
    };
    match expanded.len() {
        6 => {
            let r = u8::from_str_radix(&expanded[0..2], 16).ok()?;
            let g = u8::from_str_radix(&expanded[2..4], 16).ok()?;
            let b = u8::from_str_radix(&expanded[4..6], 16).ok()?;
            Some((r, g, b, 255))
        }
        8 => {
            let r = u8::from_str_radix(&expanded[0..2], 16).ok()?;
            let g = u8::from_str_radix(&expanded[2..4], 16).ok()?;
            let b = u8::from_str_radix(&expanded[4..6], 16).ok()?;
            let a = u8::from_str_radix(&expanded[6..8], 16).ok()?;
            Some((r, g, b, a))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        render_template, render_template_with_options, template_metrics, PaletteColor,
        RenderOptions, RunColor, TemplateContext,
    };

    fn context<'a>(title: &'a str) -> TemplateContext<'a> {
        TemplateContext {
            title,
            artist: "Artist",
            album: "Album",
            album_artist: "Album Artist",
            date: "2026-01-20",
            year: "2026",
            genre: "Rock",
            track_number: "7",
            file_name: Some("track.flac"),
            path: Some("/music/track.flac"),
        }
    }

    #[test]
    fn test_plain_text_passthrough() {
        let rendered = render_template("Now Playing:", &context("Song"));
        assert_eq!(rendered.plain_text, "Now Playing:");
    }

    #[test]
    fn test_newline_from_escape_and_tag() {
        let rendered = render_template(
            "Title: {title}\\nArtist: {artist}[br]Album: {album}",
            &context("Song"),
        );
        assert_eq!(
            rendered.plain_text,
            "Title: Song\nArtist: Artist\nAlbum: Album"
        );
        assert_eq!(rendered.lines.len(), 3);
    }

    #[test]
    fn test_escaping_special_characters() {
        let rendered = render_template("\\{artist\\} \\[b\\]x\\[/b\\] \\\\", &context("Song"));
        assert_eq!(rendered.plain_text, "{artist} [b]x[/b] \\");
    }

    #[test]
    fn test_nested_styles() {
        let rendered = render_template("[b]A [i]B[/i] C[/b]", &context("Song"));
        assert_eq!(rendered.plain_text, "A B C");
        assert!(rendered.lines[0].runs[0].bold);
        assert!(rendered.lines[0].runs[1].bold);
        assert!(rendered.lines[0].runs[1].italic);
    }

    #[test]
    fn test_size_font_and_colors() {
        let rendered = render_template(
            "[size=18][font=Inter]{title}[/font][/size] [color=accent]{artist}[/color] [color=#ffcc00]x[/color]",
            &context("Song"),
        );
        assert_eq!(rendered.lines[0].runs[0].font_size_px, 18);
        assert_eq!(rendered.lines[0].runs[0].font_family, "Inter");
        assert_eq!(
            rendered.lines[0].runs[2].color,
            Some(RunColor::Palette(PaletteColor::Accent))
        );
    }

    #[test]
    fn test_size_roles_use_default_base() {
        let rendered = render_template("[size=title]{title}[/size]", &context("Song"));
        assert_eq!(rendered.lines[0].runs[0].font_size_px, 17);
    }

    #[test]
    fn test_size_roles_are_responsive_with_base_font_option() {
        let rendered = render_template_with_options(
            "[size=title]{title}[/size] [size=caption]{artist}[/size]",
            &context("Song"),
            RenderOptions {
                base_font_size_px: 16,
            },
        );
        assert_eq!(rendered.lines[0].runs[0].font_size_px, 21);
        assert_eq!(rendered.lines[0].runs[2].font_size_px, 13);
    }

    #[test]
    fn test_numeric_size_stays_fixed_with_custom_base_font_option() {
        let rendered = render_template_with_options(
            "[size=18]{title}[/size]",
            &context("Song"),
            RenderOptions {
                base_font_size_px: 24,
            },
        );
        assert_eq!(rendered.lines[0].runs[0].font_size_px, 18);
    }

    #[test]
    fn test_fallback_chain() {
        let rendered = render_template("{missing;title;file_name}", &context("Song"));
        assert_eq!(rendered.plain_text, "Song");
    }

    #[test]
    fn test_unknown_and_mismatched_are_literal() {
        let rendered = render_template(
            "[unknown]x[/unknown] [b]x[/i] {bad field name}",
            &context("Song"),
        );
        assert_eq!(
            rendered.plain_text,
            "[unknown]x[/unknown] x[/i] {bad field name}"
        );
    }

    #[test]
    fn test_template_metrics_track_linebreaks_and_font_size() {
        let metrics = template_metrics("[size=18]{title}[/size]\\n{artist}[br]{album}");
        assert_eq!(metrics.explicit_line_count, 3);
        assert_eq!(metrics.max_font_size_px, 18);
    }

    #[test]
    fn test_template_metrics_supports_size_roles() {
        let metrics = template_metrics("[size=h2]{title}[/size]");
        assert_eq!(metrics.explicit_line_count, 1);
        assert_eq!(metrics.max_font_size_px, 23);
    }
}
