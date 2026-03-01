//! Shared template language parser/evaluator for playlist and metadata text.

use std::path::{Path, PathBuf};

use crate::protocol;

const DEFAULT_FONT_SIZE_PX: u32 = 13;

pub(crate) const DEFAULT_METADATA_PANEL_TEMPLATE: &str =
    "[size=title][b][color=text_primary]{title;file_name}[/color][/b][/size]\\n[size=body][color=text_secondary]{artist;album_artist}[/color][/size]\\n[size=body][color=text_muted]{album}[/color][/size]\\n[size=caption][color=text_muted]{date;year} • {genre}[/color][/size]";
pub(crate) const DEFAULT_STATUS_PANEL_TEMPLATE: &str =
    "[valign=center][halign=left][size=12][color=text_secondary][if=path]Now Playing: [if=artist]{artist} - [/if][if=title]{title}[else]Unknown[/if][if=selection_summary] | {selection_summary}[/if][else]{selection_summary}[/if][/color][/size][/halign][halign=right][size=11][color=text_muted][if=technical_format]Source: [if=technical_source_provider]{technical_source_provider} | [/if]{technical_format}[if=technical_bit_depth] ({technical_bit_depth} bit[/if][if=technical_sample_rate_hz], {technical_sample_rate_hz}[/if][if=technical_channels], {technical_channels}ch[/if][if=technical_bitrate_kbps], {technical_bitrate_kbps}kbps[/if][if=technical_bit_depth])[/if][else][if=technical_cast_state]Source: Unknown[/if][/if][if=technical_cast_state] | {technical_cast_state}[/if][if=technical_playback_mode] | [if=technical_output_format]{technical_playback_mode}: {technical_output_format}[if=technical_output_bit_depth] ({technical_output_bit_depth} bit[/if][if=technical_output_sample_rate_hz], {technical_output_sample_rate_hz}[/if][if=technical_output_channels], {technical_output_channels}ch[/if][if=technical_output_bitrate_kbps], {technical_output_bitrate_kbps}kbps[/if][if=technical_output_bit_depth])[/if][else]{technical_playback_mode}[/if][/if][if=technical_resampled] | Resample: {technical_resample_from_hz} -> {technical_resample_to_hz}[/if][if=technical_channel_transform][if=technical_resampled] / [/if][if=technical_resampled][else] | [/if]{technical_channel_transform}: {technical_channel_from_channels}ch -> {technical_channel_to_channels}ch[/if][if=technical_dithered][if=technical_resampled;technical_channel_transform] / [/if][if=technical_resampled;technical_channel_transform][else] | [/if]Dither[/if][/color][/size][/halign][/valign]";
pub(crate) const PLAYING_SYMBOL_PLAYING: &str = "▶️";
pub(crate) const PLAYING_SYMBOL_PAUSED: &str = "⏸️";
pub(crate) const FAVORITE_SYMBOL_ON: &str = "❤️";
pub(crate) const FAVORITE_SYMBOL_OFF: &str = "♥";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum HorizontalAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl HorizontalAlign {
    fn from_name(name: &str) -> Option<Self> {
        match normalize_name(name).as_str() {
            "left" | "start" => Some(Self::Left),
            "center" | "middle" => Some(Self::Center),
            "right" | "end" => Some(Self::Right),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum VerticalAlign {
    Top,
    #[default]
    Center,
    Bottom,
}

impl VerticalAlign {
    fn from_name(name: &str) -> Option<Self> {
        match normalize_name(name).as_str() {
            "top" => Some(Self::Top),
            "center" | "middle" => Some(Self::Center),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
}

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
    pub horizontal_align: HorizontalAlign,
    pub font_size_px: u32,
    pub font_family: String,
    pub color: Option<RunColor>,
    pub link: Option<protocol::MetadataLinkPayload>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RichTextLine {
    pub runs: Vec<RichTextRun>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RenderedText {
    pub plain_text: String,
    pub lines: Vec<RichTextLine>,
    pub vertical_align: VerticalAlign,
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
    pub playing: Option<&'a str>,
    pub favorite: Option<&'a str>,
    pub selection_summary: &'a str,
    pub technical_source_provider: &'a str,
    pub technical_format: &'a str,
    pub technical_bit_depth: &'a str,
    pub technical_sample_rate_hz: &'a str,
    pub technical_channels: &'a str,
    pub technical_bitrate_kbps: &'a str,
    pub technical_duration_ms: &'a str,
    pub technical_cast_state: &'a str,
    pub technical_playback_mode: &'a str,
    pub technical_output_format: &'a str,
    pub technical_output_bit_depth: &'a str,
    pub technical_output_sample_rate_hz: &'a str,
    pub technical_output_channels: &'a str,
    pub technical_output_bitrate_kbps: &'a str,
    pub technical_resampled: &'a str,
    pub technical_resample_from_hz: &'a str,
    pub technical_resample_to_hz: &'a str,
    pub technical_channel_transform: &'a str,
    pub technical_channel_from_channels: &'a str,
    pub technical_channel_to_channels: &'a str,
    pub technical_dithered: &'a str,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StatusTemplateFields<'a> {
    pub selection_summary: &'a str,
    pub technical_source_provider: &'a str,
    pub technical_format: &'a str,
    pub technical_bit_depth: &'a str,
    pub technical_sample_rate_hz: &'a str,
    pub technical_channels: &'a str,
    pub technical_bitrate_kbps: &'a str,
    pub technical_duration_ms: &'a str,
    pub technical_cast_state: &'a str,
    pub technical_playback_mode: &'a str,
    pub technical_output_format: &'a str,
    pub technical_output_bit_depth: &'a str,
    pub technical_output_sample_rate_hz: &'a str,
    pub technical_output_channels: &'a str,
    pub technical_output_bitrate_kbps: &'a str,
    pub technical_resampled: &'a str,
    pub technical_resample_from_hz: &'a str,
    pub technical_resample_to_hz: &'a str,
    pub technical_channel_transform: &'a str,
    pub technical_channel_from_channels: &'a str,
    pub technical_channel_to_channels: &'a str,
    pub technical_dithered: &'a str,
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
            playing: None,
            favorite: None,
            selection_summary: "",
            technical_source_provider: "",
            technical_format: "",
            technical_bit_depth: "",
            technical_sample_rate_hz: "",
            technical_channels: "",
            technical_bitrate_kbps: "",
            technical_duration_ms: "",
            technical_cast_state: "",
            technical_playback_mode: "",
            technical_output_format: "",
            technical_output_bit_depth: "",
            technical_output_sample_rate_hz: "",
            technical_output_channels: "",
            technical_output_bitrate_kbps: "",
            technical_resampled: "",
            technical_resample_from_hz: "",
            technical_resample_to_hz: "",
            technical_channel_transform: "",
            technical_channel_from_channels: "",
            technical_channel_to_channels: "",
            technical_dithered: "",
        }
    }

    pub(crate) fn with_indicator_symbols(
        mut self,
        playing: Option<&'a str>,
        favorite: Option<&'a str>,
    ) -> Self {
        self.playing = playing;
        self.favorite = favorite;
        self
    }

    pub(crate) fn with_status_fields(mut self, fields: StatusTemplateFields<'a>) -> Self {
        self.selection_summary = fields.selection_summary;
        self.technical_source_provider = fields.technical_source_provider;
        self.technical_format = fields.technical_format;
        self.technical_bit_depth = fields.technical_bit_depth;
        self.technical_sample_rate_hz = fields.technical_sample_rate_hz;
        self.technical_channels = fields.technical_channels;
        self.technical_bitrate_kbps = fields.technical_bitrate_kbps;
        self.technical_duration_ms = fields.technical_duration_ms;
        self.technical_cast_state = fields.technical_cast_state;
        self.technical_playback_mode = fields.technical_playback_mode;
        self.technical_output_format = fields.technical_output_format;
        self.technical_output_bit_depth = fields.technical_output_bit_depth;
        self.technical_output_sample_rate_hz = fields.technical_output_sample_rate_hz;
        self.technical_output_channels = fields.technical_output_channels;
        self.technical_output_bitrate_kbps = fields.technical_output_bitrate_kbps;
        self.technical_resampled = fields.technical_resampled;
        self.technical_resample_from_hz = fields.technical_resample_from_hz;
        self.technical_resample_to_hz = fields.technical_resample_to_hz;
        self.technical_channel_transform = fields.technical_channel_transform;
        self.technical_channel_from_channels = fields.technical_channel_from_channels;
        self.technical_channel_to_channels = fields.technical_channel_to_channels;
        self.technical_dithered = fields.technical_dithered;
        self
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
            "playing" => Some(self.playing.unwrap_or_default().to_string()),
            "favorite" => Some(self.favorite.unwrap_or_default().to_string()),
            "file_name" | "filename" | "file" => {
                Some(self.file_name.unwrap_or_default().to_string())
            }
            "path" => Some(self.path.unwrap_or_default().to_string()),
            "selection_summary" | "selectionsummary" => Some(self.selection_summary.to_string()),
            "technical_source_provider" | "technicalsourceprovider" => {
                Some(self.technical_source_provider.to_string())
            }
            "technical_format" | "technicalformat" => Some(self.technical_format.to_string()),
            "technical_bit_depth" | "technicalbitdepth" => {
                Some(self.technical_bit_depth.to_string())
            }
            "technical_sample_rate_hz" | "technicalsampleratehz" => {
                Some(self.technical_sample_rate_hz.to_string())
            }
            "technical_channels" | "technicalchannels" => Some(self.technical_channels.to_string()),
            "technical_bitrate_kbps" | "technicalbitratekbps" => {
                Some(self.technical_bitrate_kbps.to_string())
            }
            "technical_duration_ms" | "technicaldurationms" => {
                Some(self.technical_duration_ms.to_string())
            }
            "technical_cast_state" | "technicalcaststate" => {
                Some(self.technical_cast_state.to_string())
            }
            "technical_playback_mode" | "technicalplaybackmode" => {
                Some(self.technical_playback_mode.to_string())
            }
            "technical_output_format" | "technicaloutputformat" => {
                Some(self.technical_output_format.to_string())
            }
            "technical_output_bit_depth" | "technicaloutputbitdepth" => {
                Some(self.technical_output_bit_depth.to_string())
            }
            "technical_output_sample_rate_hz" | "technicaloutputsampleratehz" => {
                Some(self.technical_output_sample_rate_hz.to_string())
            }
            "technical_output_channels" | "technicaloutputchannels" => {
                Some(self.technical_output_channels.to_string())
            }
            "technical_output_bitrate_kbps" | "technicaloutputbitratekbps" => {
                Some(self.technical_output_bitrate_kbps.to_string())
            }
            "technical_resampled" | "technicalresampled" => {
                Some(self.technical_resampled.to_string())
            }
            "technical_resample_from_hz" | "technicalresamplefromhz" => {
                Some(self.technical_resample_from_hz.to_string())
            }
            "technical_resample_to_hz" | "technicalresampletohz" => {
                Some(self.technical_resample_to_hz.to_string())
            }
            "technical_channel_transform" | "technicalchanneltransform" => {
                Some(self.technical_channel_transform.to_string())
            }
            "technical_channel_from_channels" | "technicalchannelfromchannels" => {
                Some(self.technical_channel_from_channels.to_string())
            }
            "technical_channel_to_channels" | "technicalchanneltochannels" => {
                Some(self.technical_channel_to_channels.to_string())
            }
            "technical_dithered" | "technicaldithered" => Some(self.technical_dithered.to_string()),
            "album_art" | "disc" | "disc_number" | "duration" => Some(String::new()),
            _ => None,
        }
    }

    fn link_payload_for_key_value(
        &self,
        key: &str,
        value: &str,
    ) -> Option<protocol::MetadataLinkPayload> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        let normalized = normalize_name(key);
        let kind = match normalized.as_str() {
            "artist" | "album_artist" | "albumartist" => protocol::MetadataLinkKind::Artist,
            "album" => protocol::MetadataLinkKind::Album,
            "genre" => protocol::MetadataLinkKind::Genre,
            "year" | "date" => protocol::MetadataLinkKind::Decade,
            "title" => protocol::MetadataLinkKind::Title,
            _ => return None,
        };
        let album_artist = if self.album_artist.trim().is_empty() {
            self.artist.to_string()
        } else {
            self.album_artist.to_string()
        };
        Some(protocol::MetadataLinkPayload {
            kind,
            value: trimmed.to_string(),
            album: self.album.to_string(),
            album_artist,
            track_path: self.path.map(PathBuf::from),
        })
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
    IfOpen { fallbacks: Vec<String> },
    IfElse { literal: String },
    IfClose { literal: String },
    OpenStyle(StyleTag),
    CloseStyle { kind: StyleKind, literal: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StyleKind {
    Bold,
    Italic,
    Underline,
    HorizontalAlign,
    VerticalAlign,
    Size,
    Font,
    Color,
}

#[derive(Clone, Debug)]
enum StyleTag {
    Bold,
    Italic,
    Underline,
    HorizontalAlign(HorizontalAlign),
    VerticalAlign(VerticalAlign),
    Size(FontSizeSpec),
    Font(String),
    Color(RunColor),
}

#[derive(Clone, Debug, Default, PartialEq)]
struct StyleState {
    bold: bool,
    italic: bool,
    underline: bool,
    horizontal_align: HorizontalAlign,
    font_size_px: u32,
    font_family: String,
    color: Option<RunColor>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ConditionFrame {
    parent_active: bool,
    condition_true: bool,
    else_seen: bool,
}

impl ConditionFrame {
    fn active(self) -> bool {
        let branch_true = if self.else_seen {
            !self.condition_true
        } else {
            self.condition_true
        };
        self.parent_active && branch_true
    }
}

fn conditions_active(stack: &[ConditionFrame]) -> bool {
    stack.iter().copied().all(ConditionFrame::active)
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
                    if let Some(fallbacks) = parse_fallback_chain(&content) {
                        segments.push(TemplateSegment::Placeholder {
                            raw: content,
                            fallbacks,
                        });
                    } else {
                        segments.push(TemplateSegment::Text(format!("{{{content}}}")));
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
                        Some(ParsedTag::OpenIf { fallbacks }) => {
                            segments.push(TemplateSegment::IfOpen { fallbacks });
                        }
                        Some(ParsedTag::Else { literal }) => {
                            segments.push(TemplateSegment::IfElse { literal });
                        }
                        Some(ParsedTag::CloseIf { literal }) => {
                            segments.push(TemplateSegment::IfClose { literal });
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
    let mut condition_stack: Vec<ConditionFrame> = Vec::new();
    let mut vertical_align = VerticalAlign::Center;

    for segment in &parsed.segments {
        match segment {
            TemplateSegment::IfOpen { fallbacks } => {
                let parent_active = conditions_active(&condition_stack);
                let condition_true = resolve_placeholder(context, fallbacks)
                    .map(|resolved| !resolved.value.trim().is_empty())
                    .unwrap_or(false);
                condition_stack.push(ConditionFrame {
                    parent_active,
                    condition_true,
                    else_seen: false,
                });
            }
            TemplateSegment::IfElse { literal } => {
                let mut duplicate_else = false;
                if let Some(frame) = condition_stack.last_mut() {
                    if frame.else_seen {
                        duplicate_else = true;
                    } else {
                        frame.else_seen = true;
                    }
                } else {
                    duplicate_else = true;
                }
                if duplicate_else && conditions_active(&condition_stack) {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        literal,
                        &style_stack,
                        render_options,
                        None,
                    );
                }
            }
            TemplateSegment::IfClose { literal } => {
                if condition_stack.pop().is_none() && conditions_active(&condition_stack) {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        literal,
                        &style_stack,
                        render_options,
                        None,
                    );
                }
            }
            TemplateSegment::Text(text) => {
                if !conditions_active(&condition_stack) {
                    continue;
                }
                append_text(
                    &mut lines,
                    &mut plain_text,
                    text,
                    &style_stack,
                    render_options,
                    None,
                );
            }
            TemplateSegment::Placeholder { raw, fallbacks } => {
                if !conditions_active(&condition_stack) {
                    continue;
                }
                if let Some(resolved) = resolve_placeholder(context, fallbacks) {
                    let link_payload =
                        context.link_payload_for_key_value(&resolved.selected_key, &resolved.value);
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        &resolved.value,
                        &style_stack,
                        render_options,
                        link_payload.as_ref(),
                    );
                } else {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        &format!("{{{raw}}}"),
                        &style_stack,
                        render_options,
                        None,
                    );
                }
            }
            TemplateSegment::NewLine => {
                if !conditions_active(&condition_stack) {
                    continue;
                }
                plain_text.push('\n');
                lines.push(RichTextLine { runs: Vec::new() });
            }
            TemplateSegment::OpenStyle(style) => {
                if !conditions_active(&condition_stack) {
                    continue;
                }
                if let StyleTag::VerticalAlign(next_align) = style {
                    vertical_align = *next_align;
                }
                style_stack.push(style.clone());
            }
            TemplateSegment::CloseStyle { kind, literal } => {
                if !conditions_active(&condition_stack) {
                    continue;
                }
                if style_stack.last().map(style_kind) == Some(*kind) {
                    style_stack.pop();
                } else {
                    append_text(
                        &mut lines,
                        &mut plain_text,
                        literal,
                        &style_stack,
                        render_options,
                        None,
                    );
                }
            }
        }
    }

    RenderedText {
        plain_text,
        lines,
        vertical_align,
    }
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

struct ResolvedPlaceholder {
    selected_key: String,
    value: String,
}

fn resolve_placeholder(
    context: &TemplateContext<'_>,
    fallbacks: &[String],
) -> Option<ResolvedPlaceholder> {
    let mut recognized_any = false;
    let mut first_recognized_key: Option<String> = None;
    for key in fallbacks {
        if let Some(value) = context.value_for_key(key) {
            recognized_any = true;
            if first_recognized_key.is_none() {
                first_recognized_key = Some(key.clone());
            }
            if !value.is_empty() {
                return Some(ResolvedPlaceholder {
                    selected_key: key.clone(),
                    value,
                });
            }
        }
    }
    if recognized_any {
        Some(ResolvedPlaceholder {
            selected_key: first_recognized_key.unwrap_or_default(),
            value: String::new(),
        })
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
    link_payload: Option<&protocol::MetadataLinkPayload>,
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
            horizontal_align: state.horizontal_align,
            font_size_px: state.font_size_px,
            font_family: state.font_family.clone(),
            color: state.color.clone(),
            link: link_payload.cloned(),
        };
        if let Some(line) = lines.last_mut() {
            if let Some(previous) = line.runs.last_mut() {
                if previous.bold == run.bold
                    && previous.italic == run.italic
                    && previous.underline == run.underline
                    && previous.horizontal_align == run.horizontal_align
                    && previous.font_size_px == run.font_size_px
                    && previous.font_family == run.font_family
                    && previous.color == run.color
                    && previous.link == run.link
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
        horizontal_align: HorizontalAlign::Left,
        font_size_px: render_options.base_font_size_px.max(1),
        ..StyleState::default()
    };
    for style in style_stack {
        match style {
            StyleTag::Bold => state.bold = true,
            StyleTag::Italic => state.italic = true,
            StyleTag::Underline => state.underline = true,
            StyleTag::HorizontalAlign(align) => state.horizontal_align = *align,
            StyleTag::VerticalAlign(_) => {}
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
        StyleTag::HorizontalAlign(_) => StyleKind::HorizontalAlign,
        StyleTag::VerticalAlign(_) => StyleKind::VerticalAlign,
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
    OpenIf { fallbacks: Vec<String> },
    Else { literal: String },
    CloseIf { literal: String },
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
            "if" => Some(ParsedTag::CloseIf {
                literal: format!("[/{rest}]"),
            }),
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
            "halign" => Some(ParsedTag::Close {
                kind: StyleKind::HorizontalAlign,
                literal: format!("[/{rest}]"),
            }),
            "valign" => Some(ParsedTag::Close {
                kind: StyleKind::VerticalAlign,
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
            "if" => parse_fallback_chain(value).map(|fallbacks| ParsedTag::OpenIf { fallbacks }),
            "size" => parse_size_spec(value).map(|size| ParsedTag::Open(StyleTag::Size(size))),
            "font" => Some(ParsedTag::Open(StyleTag::Font(value.to_string()))),
            "color" => parse_color(value).map(|color| ParsedTag::Open(StyleTag::Color(color))),
            "halign" => parse_horizontal_align(value)
                .map(|align| ParsedTag::Open(StyleTag::HorizontalAlign(align))),
            "valign" => parse_vertical_align(value)
                .map(|align| ParsedTag::Open(StyleTag::VerticalAlign(align))),
            _ => None,
        };
    }

    let name = normalize_name(trimmed);
    match name.as_str() {
        "else" => Some(ParsedTag::Else {
            literal: format!("[{trimmed}]"),
        }),
        "b" => Some(ParsedTag::Open(StyleTag::Bold)),
        "i" => Some(ParsedTag::Open(StyleTag::Italic)),
        "u" => Some(ParsedTag::Open(StyleTag::Underline)),
        _ => None,
    }
}

fn parse_fallback_chain(value: &str) -> Option<Vec<String>> {
    let mut fallbacks = Vec::new();
    for part in value.split(';') {
        let key = part.trim();
        if key.is_empty() {
            return None;
        }
        fallbacks.push(key.to_string());
    }
    if fallbacks.is_empty() {
        None
    } else {
        Some(fallbacks)
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

fn parse_horizontal_align(value: &str) -> Option<HorizontalAlign> {
    HorizontalAlign::from_name(value)
}

fn parse_vertical_align(value: &str) -> Option<VerticalAlign> {
    VerticalAlign::from_name(value)
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
        render_template, render_template_with_options, template_metrics, HorizontalAlign,
        PaletteColor, RenderOptions, RunColor, StatusTemplateFields, TemplateContext,
        VerticalAlign, DEFAULT_STATUS_PANEL_TEMPLATE,
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
            playing: None,
            favorite: None,
            selection_summary: "",
            technical_source_provider: "",
            technical_format: "",
            technical_bit_depth: "",
            technical_sample_rate_hz: "",
            technical_channels: "",
            technical_bitrate_kbps: "",
            technical_duration_ms: "",
            technical_cast_state: "",
            technical_playback_mode: "",
            technical_output_format: "",
            technical_output_bit_depth: "",
            technical_output_sample_rate_hz: "",
            technical_output_channels: "",
            technical_output_bitrate_kbps: "",
            technical_resampled: "",
            technical_resample_from_hz: "",
            technical_resample_to_hz: "",
            technical_channel_transform: "",
            technical_channel_from_channels: "",
            technical_channel_to_channels: "",
            technical_dithered: "",
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
    fn test_alignment_tags_are_applied() {
        let rendered = render_template(
            "[valign=bottom][halign=right]{title}[/halign]",
            &context("Song"),
        );
        assert_eq!(rendered.vertical_align, VerticalAlign::Bottom);
        assert_eq!(
            rendered.lines[0].runs[0].horizontal_align,
            HorizontalAlign::Right
        );
    }

    #[test]
    fn test_last_valign_open_tag_wins_for_block() {
        let rendered = render_template(
            "[valign=top]{title}[/valign][valign=center]{artist}",
            &context("Song"),
        );
        assert_eq!(rendered.vertical_align, VerticalAlign::Center);
    }

    #[test]
    fn test_status_placeholders_render_from_context() {
        let rendered = render_template(
            "{selection_summary}|{technical_source_provider}|{technical_format}|{technical_bit_depth}|{technical_sample_rate_hz}|{technical_channels}|{technical_bitrate_kbps}|{technical_playback_mode}",
            &context("Song").with_status_fields(StatusTemplateFields {
                selection_summary: "2 tracks selected",
                technical_source_provider: "opensubsonic",
                technical_format: "FLAC",
                technical_bit_depth: "24",
                technical_sample_rate_hz: "96000",
                technical_channels: "2",
                technical_bitrate_kbps: "320",
                technical_playback_mode: "direct",
                ..StatusTemplateFields::default()
            }),
        );
        assert_eq!(
            rendered.plain_text,
            "2 tracks selected|opensubsonic|FLAC|24|96000|2|320|direct"
        );
    }

    #[test]
    fn test_if_condition_renders_true_branch() {
        let rendered = render_template("[if=title]{title}[else]No track[/if]", &context("Song"));
        assert_eq!(rendered.plain_text, "Song");
    }

    #[test]
    fn test_if_condition_renders_else_branch_when_false() {
        let rendered = render_template(
            "[if=title]{title}[else]No track[/if]",
            &TemplateContext {
                title: "",
                ..context("Song")
            },
        );
        assert_eq!(rendered.plain_text, "No track");
    }

    #[test]
    fn test_if_condition_supports_fallback_chains() {
        let rendered = render_template(
            "[if=artist;album_artist]{artist;album_artist}[else]Unknown artist[/if]",
            &TemplateContext {
                artist: "",
                album_artist: "Fallback Artist",
                ..context("Song")
            },
        );
        assert_eq!(rendered.plain_text, "Fallback Artist");
    }

    #[test]
    fn test_if_condition_renders_else_branch_with_empty_true_branch() {
        let rendered = render_template(
            "[if=technical_output_format][else]{technical_playback_mode}[/if]",
            &context("Song").with_status_fields(StatusTemplateFields {
                technical_playback_mode: "Direct play",
                technical_output_format: "",
                ..StatusTemplateFields::default()
            }),
        );
        assert_eq!(rendered.plain_text, "Direct play");
    }

    #[test]
    fn test_default_status_template_renders_direct_play_segment() {
        let rendered = render_template(
            DEFAULT_STATUS_PANEL_TEMPLATE,
            &context("Song").with_status_fields(StatusTemplateFields {
                technical_format: "FLAC",
                technical_bit_depth: "24",
                technical_sample_rate_hz: "192kHz",
                technical_channels: "2",
                technical_bitrate_kbps: "5501",
                technical_playback_mode: "Direct play",
                ..StatusTemplateFields::default()
            }),
        );
        assert!(rendered
            .plain_text
            .contains("Source: FLAC (24 bit, 192kHz, 2ch, 5501kbps)"));
        assert!(rendered.plain_text.contains(" | Direct play"));
    }

    #[test]
    fn test_default_status_template_renders_transform_segments() {
        let rendered = render_template(
            DEFAULT_STATUS_PANEL_TEMPLATE,
            &context("Song").with_status_fields(StatusTemplateFields {
                technical_format: "FLAC",
                technical_bit_depth: "24",
                technical_sample_rate_hz: "96kHz",
                technical_channels: "2",
                technical_bitrate_kbps: "2800",
                technical_resampled: "true",
                technical_resample_from_hz: "96kHz",
                technical_resample_to_hz: "48kHz",
                technical_channel_transform: "Downmix",
                technical_channel_from_channels: "2",
                technical_channel_to_channels: "2",
                ..StatusTemplateFields::default()
            }),
        );
        assert!(rendered.plain_text.contains(" | Resample: 96kHz -> 48kHz"));
        assert!(rendered.plain_text.contains(" / Downmix: 2ch -> 2ch"));
    }

    #[test]
    fn test_if_else_and_close_tags_render_literal_when_malformed() {
        let rendered = render_template("[if=]{title} [else]x[/if]", &context("Song"));
        assert_eq!(rendered.plain_text, "[if=]Song [else]x[/if]");
    }

    #[test]
    fn test_malformed_alignment_tags_render_as_literal_text() {
        let rendered = render_template(
            "[halign=diagonal]x[/halign] [valign=middle-ish]y[/valign]",
            &context("Song"),
        );
        assert_eq!(
            rendered.plain_text,
            "[halign=diagonal]x[/halign] [valign=middle-ish]y[/valign]"
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
    fn test_playing_and_favorite_placeholders_render_indicator_symbols() {
        let rendered = render_template(
            "{playing} {favorite}",
            &context("Song").with_indicator_symbols(
                Some(super::PLAYING_SYMBOL_PAUSED),
                Some(super::FAVORITE_SYMBOL_ON),
            ),
        );
        assert_eq!(
            rendered.plain_text,
            format!(
                "{} {}",
                super::PLAYING_SYMBOL_PAUSED,
                super::FAVORITE_SYMBOL_ON
            )
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
