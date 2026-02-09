//! UI adapter and presentation-state manager.
//!
//! This component bridges event-bus messages to Slint model updates, performs
//! metadata/cover-art lookup, and owns playlist table presentation behavior.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::time::{Duration, Instant};
use std::{
    collections::hash_map::DefaultHasher,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, Receiver as StdReceiver, Sender as StdSender},
    thread,
};

use governor::state::NotKeyed;
use id3::{Tag, TagLike};
use log::{debug, info, warn};
use slint::{Model, ModelRc, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{
    config::{self, PlaylistColumnConfig},
    protocol, AppWindow, TrackRowData,
};
use governor::{Quota, RateLimiter};

/// Shared UI models that are created in `main` and attached to the Slint window.
pub struct UiState {
    /// Track table model consumed directly by Slint.
    pub track_model: Rc<VecModel<TrackRowData>>,
}

/// Consumes bus messages and applies corresponding UI state updates.
pub struct UiManager {
    ui: slint::Weak<AppWindow>,
    bus_receiver: Receiver<protocol::Message>,
    bus_sender: Sender<protocol::Message>,
    cover_art_lookup_tx: StdSender<CoverArtLookupRequest>,
    last_cover_art_lookup_path: Option<PathBuf>,
    active_playlist_id: String,
    playlist_ids: Vec<String>,
    track_ids: Vec<String>,
    track_paths: Vec<PathBuf>,
    track_metadata: Vec<TrackMetadata>,
    selected_indices: Vec<usize>,
    drag_indices: Vec<usize>,
    is_dragging: bool,
    pressed_index: Option<usize>,
    progress_rl:
        RateLimiter<NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>,
    // Cached progress values to avoid unnecessary UI updates
    last_elapsed_ms: u64,
    last_total_ms: u64,
    current_playing_track_path: Option<PathBuf>,
    current_playing_track_metadata: Option<protocol::DetailedMetadata>,
    playlist_columns: Vec<PlaylistColumnConfig>,
    playlist_column_content_targets_px: Vec<u32>,
    playlist_column_widths_px: Vec<u32>,
    playlist_column_width_overrides_px: HashMap<String, u32>,
    playlist_columns_available_width_px: u32,
    playlist_columns_content_width_px: u32,
    playback_active: bool,
    processed_message_count: u64,
    lagged_message_count: u64,
    last_message_at: Instant,
    last_progress_at: Option<Instant>,
    last_health_log_at: Instant,
}

/// Normalized track metadata snapshot used for row rendering and side panel display.
#[derive(Clone)]
struct TrackMetadata {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    date: String,
    year: String,
    genre: String,
    track_number: String,
}

/// Width policy used by adaptive playlist column sizing.
#[derive(Clone, Copy, Debug)]
struct ColumnWidthProfile {
    min_px: u32,
    preferred_px: u32,
    max_px: u32,
}

/// Cover-art lookup request payload used by the internal worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverArtLookupRequest {
    track_path: Option<PathBuf>,
}

fn fit_column_widths_to_available_space(
    widths: &mut [u32],
    min_widths: &[u32],
    _max_widths: &[u32],
    available_width_px: u32,
) {
    let mut used_width: u32 = widths.iter().copied().sum();
    if used_width > available_width_px {
        let mut deficit = used_width - available_width_px;
        while deficit > 0 {
            let adjustable_indices: Vec<usize> = widths
                .iter()
                .enumerate()
                .filter_map(|(index, width)| (*width > min_widths[index]).then_some(index))
                .collect();
            if adjustable_indices.is_empty() {
                break;
            }
            let step = (deficit / adjustable_indices.len() as u32).max(1);
            let mut reduced_this_round = 0u32;
            for index in adjustable_indices {
                let shrink_capacity = widths[index] - min_widths[index];
                if shrink_capacity == 0 {
                    continue;
                }
                let shrink_by = shrink_capacity.min(step).min(deficit);
                widths[index] -= shrink_by;
                deficit -= shrink_by;
                reduced_this_round += shrink_by;
                if deficit == 0 {
                    break;
                }
            }
            if reduced_this_round == 0 {
                break;
            }
        }
    }
    used_width = widths.iter().copied().sum();
    debug!(
        "Playlist column sizing fitted: available={} used={} columns={} (shrink-only)",
        available_width_px,
        used_width,
        widths.len()
    );
}

impl UiManager {
    fn coalesce_cover_art_requests(
        mut latest: CoverArtLookupRequest,
        request_rx: &StdReceiver<CoverArtLookupRequest>,
    ) -> CoverArtLookupRequest {
        while let Ok(next) = request_rx.try_recv() {
            latest = next;
        }
        latest
    }

    /// Creates a UI manager and starts an internal cover-art lookup worker thread.
    pub fn new(
        ui: slint::Weak<AppWindow>,
        bus_receiver: Receiver<protocol::Message>,
        bus_sender: Sender<protocol::Message>,
    ) -> Self {
        let (cover_art_lookup_tx, cover_art_lookup_rx) = mpsc::channel::<CoverArtLookupRequest>();
        let cover_art_bus_sender = bus_sender.clone();
        thread::spawn(move || {
            while let Ok(request) = cover_art_lookup_rx.recv() {
                let latest_request =
                    UiManager::coalesce_cover_art_requests(request, &cover_art_lookup_rx);
                let cover_art_path = latest_request
                    .track_path
                    .as_ref()
                    .and_then(UiManager::find_cover_art);
                let _ = cover_art_bus_sender.send(protocol::Message::Playback(
                    protocol::PlaybackMessage::CoverArtChanged(cover_art_path),
                ));
            }
        });

        Self {
            ui: ui.clone(),
            bus_receiver,
            bus_sender,
            cover_art_lookup_tx,
            last_cover_art_lookup_path: None,
            active_playlist_id: String::new(),
            playlist_ids: Vec::new(),
            track_ids: Vec::new(),
            track_paths: Vec::new(),
            track_metadata: Vec::new(),
            selected_indices: Vec::new(),
            drag_indices: Vec::new(),
            is_dragging: false,
            pressed_index: None,
            progress_rl: RateLimiter::direct(
                Quota::with_period(Duration::from_millis(30)).unwrap(),
            ),
            last_elapsed_ms: 0,
            last_total_ms: 0,
            current_playing_track_path: None,
            current_playing_track_metadata: None,
            playlist_columns: config::default_playlist_columns(),
            playlist_column_content_targets_px: Vec::new(),
            playlist_column_widths_px: Vec::new(),
            playlist_column_width_overrides_px: HashMap::new(),
            playlist_columns_available_width_px: 0,
            playlist_columns_content_width_px: 0,
            playback_active: false,
            processed_message_count: 0,
            lagged_message_count: 0,
            last_message_at: Instant::now(),
            last_progress_at: None,
            last_health_log_at: Instant::now(),
        }
    }

    fn on_message_received(&mut self) {
        let now = Instant::now();
        self.processed_message_count = self.processed_message_count.saturating_add(1);
        self.log_health_if_due(now);
        self.last_message_at = now;
    }

    fn on_message_lagged(&mut self) {
        self.lagged_message_count = self.lagged_message_count.saturating_add(1);
        let now = Instant::now();
        if now.duration_since(self.last_health_log_at) >= Duration::from_secs(5) {
            warn!(
                "UiManager: bus lagged. total_lagged={}, processed={}",
                self.lagged_message_count, self.processed_message_count
            );
            self.last_health_log_at = now;
        }
    }

    fn log_health_if_due(&mut self, now: Instant) {
        if now.duration_since(self.last_health_log_at) < Duration::from_secs(5) {
            return;
        }

        let since_last_message_ms = now.duration_since(self.last_message_at).as_millis();
        let since_last_progress_ms = self
            .last_progress_at
            .map(|last| now.duration_since(last).as_millis() as u64);
        info!(
            "UiManager health: processed={}, lagged={}, playback_active={}, since_last_message_ms={}, since_last_progress_ms={:?}",
            self.processed_message_count,
            self.lagged_message_count,
            self.playback_active,
            since_last_message_ms,
            since_last_progress_ms
        );

        if self.playback_active {
            if let Some(last_progress_at) = self.last_progress_at {
                if now.duration_since(last_progress_at) > Duration::from_secs(2) {
                    warn!(
                        "UiManager: playback is active but no progress message for {}ms",
                        now.duration_since(last_progress_at).as_millis()
                    );
                }
            } else {
                warn!("UiManager: playback is active but no progress message received yet");
            }
        }

        self.last_health_log_at = now;
    }

    fn find_cover_art(track_path: &PathBuf) -> Option<PathBuf> {
        let parent = track_path.parent()?;
        let names = ["cover", "front", "folder", "album", "art"];
        let extensions = ["jpg", "jpeg", "png", "webp"];

        // Priority 1: External files
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut found_files = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let file_stem_lower = file_stem.to_lowercase();
                        if names.iter().any(|&n| file_stem_lower == n) {
                            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                                let ext_lower = ext.to_lowercase();
                                if extensions.iter().any(|&e| ext_lower == e) {
                                    found_files.push(path);
                                }
                            }
                        }
                    }
                }
            }
            // Sort to have some deterministic behavior if multiple files exist
            found_files.sort();
            if let Some(found) = found_files.into_iter().next() {
                return Some(found);
            }
        }

        // Priority 2: Embedded art
        Self::extract_embedded_art(track_path)
    }

    fn extract_embedded_art(track_path: &PathBuf) -> Option<PathBuf> {
        let mut hasher = DefaultHasher::new();
        track_path.hash(&mut hasher);
        let hash = hasher.finish();
        let cache_dir = dirs::cache_dir()?.join("music_player").join("covers");
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).ok()?;
        }

        let cache_path = cache_dir.join(format!("{:x}.png", hash));

        // If already cached, return it
        if cache_path.exists() {
            return Some(cache_path);
        }

        // Try extracting from ID3
        if let Ok(tag) = Tag::read_from_path(track_path) {
            if let Some(pic) = tag.pictures().next() {
                if let Ok(mut file) = std::fs::File::create(&cache_path) {
                    if file.write_all(&pic.data).is_ok() {
                        return Some(cache_path);
                    }
                }
            }
        }

        // Try extracting from FLAC
        if let Ok(tag) = metaflac::Tag::read_from_path(track_path) {
            if let Some(pic) = tag.pictures().next() {
                if let Ok(mut file) = std::fs::File::create(&cache_path) {
                    if file.write_all(&pic.data).is_ok() {
                        return Some(cache_path);
                    }
                }
            }
        }

        None
    }

    fn update_cover_art(&mut self, track_path: Option<&PathBuf>) {
        let requested_track_path = track_path.cloned();
        if self.last_cover_art_lookup_path == requested_track_path {
            return;
        }
        self.last_cover_art_lookup_path = requested_track_path.clone();
        let _ = self.cover_art_lookup_tx.send(CoverArtLookupRequest {
            track_path: requested_track_path,
        });
    }

    fn to_detailed_metadata(track_metadata: &TrackMetadata) -> protocol::DetailedMetadata {
        protocol::DetailedMetadata {
            title: track_metadata.title.clone(),
            artist: track_metadata.artist.clone(),
            album: track_metadata.album.clone(),
            date: track_metadata.date.clone(),
            genre: track_metadata.genre.clone(),
        }
    }

    fn normalize_metadata_key(key: &str) -> String {
        let mut normalized = String::new();
        for ch in key.chars() {
            if ch.is_ascii_alphanumeric() {
                normalized.push(ch.to_ascii_lowercase());
            } else if ch == '_' || ch == '-' || ch.is_whitespace() {
                normalized.push('_');
            }
        }
        normalized
    }

    fn metadata_value(track_metadata: &TrackMetadata, key: &str) -> String {
        let normalized = Self::normalize_metadata_key(key);
        match normalized.as_str() {
            "title" => track_metadata.title.clone(),
            "artist" => track_metadata.artist.clone(),
            "album" => track_metadata.album.clone(),
            "album_artist" | "albumartist" => track_metadata.album_artist.clone(),
            "date" => track_metadata.date.clone(),
            "year" => {
                if !track_metadata.year.is_empty() {
                    track_metadata.year.clone()
                } else if track_metadata.date.len() >= 4 {
                    track_metadata.date[0..4].to_string()
                } else {
                    String::new()
                }
            }
            "genre" => track_metadata.genre.clone(),
            "track" | "track_number" | "tracknumber" => track_metadata.track_number.clone(),
            _ => String::new(),
        }
    }

    fn render_column_value(track_metadata: &TrackMetadata, format_string: &str) -> String {
        let mut rendered = String::new();
        let mut chars = format_string.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '{' {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    rendered.push('{');
                    continue;
                }

                let mut token = String::new();
                let mut found_closing = false;
                for token_ch in chars.by_ref() {
                    if token_ch == '}' {
                        found_closing = true;
                        break;
                    }
                    token.push(token_ch);
                }

                if found_closing {
                    let value = Self::metadata_value(track_metadata, token.trim());
                    rendered.push_str(&value);
                } else {
                    rendered.push('{');
                    rendered.push_str(&token);
                }
            } else if ch == '}' {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                rendered.push('}');
            } else {
                rendered.push(ch);
            }
        }

        rendered
    }

    fn build_playlist_row_values(
        track_metadata: &TrackMetadata,
        playlist_columns: &[PlaylistColumnConfig],
    ) -> Vec<String> {
        playlist_columns
            .iter()
            .filter(|column| column.enabled)
            .map(|column| Self::render_column_value(track_metadata, &column.format))
            .collect()
    }

    fn visible_playlist_columns(&self) -> Vec<&PlaylistColumnConfig> {
        self.playlist_columns
            .iter()
            .filter(|column| column.enabled)
            .collect()
    }

    fn normalize_column_format(format: &str) -> String {
        format.trim().to_ascii_lowercase()
    }

    fn playlist_column_key(column: &PlaylistColumnConfig) -> String {
        if column.custom {
            format!("custom:{}|{}", column.name.trim(), column.format.trim())
        } else {
            Self::normalize_column_format(&column.format)
        }
    }

    fn column_width_profile(column: &PlaylistColumnConfig) -> ColumnWidthProfile {
        let normalized_format = column.format.trim().to_ascii_lowercase();
        let normalized_name = column.name.trim().to_ascii_lowercase();

        if normalized_format == "{track}"
            || normalized_format == "{track_number}"
            || normalized_format == "{tracknumber}"
            || normalized_name == "track #"
            || normalized_name == "track"
        {
            return ColumnWidthProfile {
                min_px: 52,
                preferred_px: 68,
                max_px: 90,
            };
        }

        if normalized_format == "{disc}" || normalized_format == "{disc_number}" {
            return ColumnWidthProfile {
                min_px: 50,
                preferred_px: 64,
                max_px: 84,
            };
        }

        if normalized_format == "{year}"
            || normalized_format == "{date}"
            || normalized_name == "year"
        {
            return ColumnWidthProfile {
                min_px: 64,
                preferred_px: 78,
                max_px: 104,
            };
        }

        if normalized_format == "{title}" || normalized_name == "title" {
            return ColumnWidthProfile {
                min_px: 140,
                preferred_px: 230,
                max_px: 440,
            };
        }

        if normalized_format == "{artist}"
            || normalized_format == "{album_artist}"
            || normalized_format == "{albumartist}"
            || normalized_name == "artist"
            || normalized_name == "album artist"
        {
            return ColumnWidthProfile {
                min_px: 120,
                preferred_px: 190,
                max_px: 320,
            };
        }

        if normalized_format == "{album}" || normalized_name == "album" {
            return ColumnWidthProfile {
                min_px: 140,
                preferred_px: 220,
                max_px: 360,
            };
        }

        if normalized_format == "{genre}" || normalized_name == "genre" {
            return ColumnWidthProfile {
                min_px: 100,
                preferred_px: 140,
                max_px: 210,
            };
        }

        if normalized_name == "duration"
            || normalized_name == "time"
            || normalized_format == "{duration}"
        {
            return ColumnWidthProfile {
                min_px: 78,
                preferred_px: 92,
                max_px: 120,
            };
        }

        if column.custom {
            return ColumnWidthProfile {
                min_px: 110,
                preferred_px: 180,
                max_px: 340,
            };
        }

        ColumnWidthProfile {
            min_px: 100,
            preferred_px: 170,
            max_px: 300,
        }
    }

    fn refresh_playlist_column_content_targets(&mut self) {
        const SAMPLE_LIMIT: usize = 256;
        const MAX_MEASURED_CHARS: usize = 80;
        const CHAR_WIDTH_PX: u32 = 7;
        const CONTENT_PADDING_PX: u32 = 24;

        let visible_columns = self.visible_playlist_columns();
        let total_rows = self.track_metadata.len();
        let stride = if total_rows > SAMPLE_LIMIT {
            ((total_rows as f32) / (SAMPLE_LIMIT as f32)).ceil() as usize
        } else {
            1
        };

        let mut targets = Vec::with_capacity(visible_columns.len());
        for column in visible_columns {
            let profile = Self::column_width_profile(column);
            let mut char_width_samples = Vec::new();

            let header_chars = column.name.chars().take(MAX_MEASURED_CHARS).count() as u32;
            char_width_samples.push(header_chars);

            if total_rows > 0 {
                for metadata in self
                    .track_metadata
                    .iter()
                    .step_by(stride)
                    .take(SAMPLE_LIMIT)
                {
                    let rendered_value = Self::render_column_value(metadata, &column.format);
                    let measured_chars =
                        rendered_value.chars().take(MAX_MEASURED_CHARS).count() as u32;
                    char_width_samples.push(measured_chars);
                }
            }

            char_width_samples.sort_unstable();
            let percentile_index = ((char_width_samples.len() - 1) * 9) / 10;
            let percentile_chars = char_width_samples
                .get(percentile_index)
                .copied()
                .unwrap_or(0);
            let measured_width_px = percentile_chars
                .saturating_mul(CHAR_WIDTH_PX)
                .saturating_add(CONTENT_PADDING_PX);
            let preferred = if total_rows == 0 {
                profile.preferred_px
            } else {
                measured_width_px.max(profile.min_px)
            };
            targets.push(preferred.clamp(profile.min_px, profile.max_px));
        }

        self.playlist_column_content_targets_px = targets;
    }

    fn compute_playlist_column_widths(&self, available_width_px: u32) -> Vec<u32> {
        const COLUMN_SPACING_PX: u32 = 10;
        let visible_columns = self.visible_playlist_columns();
        if visible_columns.is_empty() {
            return Vec::new();
        }

        let mut min_widths = Vec::with_capacity(visible_columns.len());
        let mut max_widths = Vec::with_capacity(visible_columns.len());
        let mut widths = Vec::with_capacity(visible_columns.len());

        for (index, column) in visible_columns.iter().enumerate() {
            let profile = Self::column_width_profile(column);
            let override_width = self
                .playlist_column_width_overrides_px
                .get(&Self::playlist_column_key(column))
                .copied();
            let target = override_width.unwrap_or_else(|| {
                self.playlist_column_content_targets_px
                    .get(index)
                    .copied()
                    .unwrap_or(profile.preferred_px)
            });
            let target = target.clamp(profile.min_px, profile.max_px);
            min_widths.push(profile.min_px);
            max_widths.push(profile.max_px);
            widths.push(target);
        }

        let spacing_total =
            COLUMN_SPACING_PX.saturating_mul((widths.len().saturating_sub(1)) as u32);
        let preferred_total: u32 = widths.iter().copied().sum();
        let available_for_columns = if available_width_px == 0 {
            preferred_total
        } else {
            available_width_px.saturating_sub(spacing_total)
        };

        fit_column_widths_to_available_space(
            &mut widths,
            &min_widths,
            &max_widths,
            available_for_columns,
        );

        widths
    }

    fn apply_playlist_column_layout(&mut self) {
        if self.playlist_column_content_targets_px.len() != self.visible_playlist_columns().len() {
            self.refresh_playlist_column_content_targets();
        }

        let widths = self.compute_playlist_column_widths(self.playlist_columns_available_width_px);
        let content_width = widths
            .iter()
            .copied()
            .sum::<u32>()
            .saturating_add(10u32.saturating_mul((widths.len().saturating_sub(1)) as u32));

        if widths == self.playlist_column_widths_px
            && content_width == self.playlist_columns_content_width_px
        {
            return;
        }

        self.playlist_column_widths_px = widths.clone();
        self.playlist_columns_content_width_px = content_width;

        let widths_i32: Vec<i32> = widths
            .into_iter()
            .map(|width| width.min(i32::MAX as u32) as i32)
            .collect();
        let mut gap_positions: Vec<i32> = Vec::with_capacity(widths_i32.len() + 1);
        let mut cursor_px: i32 = 0;
        gap_positions.push(0);
        for (index, width_px) in widths_i32.iter().enumerate() {
            cursor_px = cursor_px.saturating_add((*width_px).max(0));
            if index + 1 < widths_i32.len() {
                cursor_px = cursor_px.saturating_add(10);
            }
            gap_positions.push(cursor_px);
        }
        let content_width_i32 = content_width.min(i32::MAX as u32) as i32;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_playlist_column_widths_px(ModelRc::from(Rc::new(VecModel::from(widths_i32))));
            ui.set_playlist_column_gap_positions_px(ModelRc::from(Rc::new(VecModel::from(
                gap_positions,
            ))));
            ui.set_playlist_columns_content_width_px(content_width_i32);
        });
    }

    fn status_text_from_track_metadata(track_metadata: &TrackMetadata) -> slint::SharedString {
        let title = if track_metadata.title.is_empty() {
            "Unknown".to_string()
        } else {
            track_metadata.title.clone()
        };
        if !track_metadata.artist.is_empty() {
            format!("{} - {}", track_metadata.artist, title).into()
        } else {
            title.into()
        }
    }

    fn resolve_display_target(
        selected_indices: &[usize],
        track_paths: &[PathBuf],
        track_metadata: &[TrackMetadata],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) -> (Option<PathBuf>, Option<protocol::DetailedMetadata>) {
        if let Some(&selected_index) = selected_indices.first() {
            let selected_path = track_paths.get(selected_index).cloned();
            let selected_metadata = track_metadata
                .get(selected_index)
                .map(Self::to_detailed_metadata);
            return (selected_path, selected_metadata);
        }

        let playing_path = playing_track_path.cloned();
        if let Some(path) = playing_track_path {
            if let Some(index) = track_paths.iter().position(|candidate| candidate == path) {
                if let Some(metadata) = track_metadata.get(index) {
                    return (playing_path, Some(Self::to_detailed_metadata(metadata)));
                }
            }
        }

        (playing_path, playing_track_metadata.cloned())
    }

    fn update_display_for_selection(
        &mut self,
        selected_indices: &[usize],
        playing_track_path: Option<&PathBuf>,
        playing_track_metadata: Option<&protocol::DetailedMetadata>,
    ) {
        let (display_path, display_metadata) = Self::resolve_display_target(
            selected_indices,
            &self.track_paths,
            &self.track_metadata,
            playing_track_path,
            playing_track_metadata,
        );
        self.update_cover_art(display_path.as_ref());
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::MetadataDisplayChanged(display_metadata),
        ));
    }

    fn read_track_metadata(&self, path: &PathBuf) -> TrackMetadata {
        debug!("Reading metadata for: {}", path.display());

        // Try ID3 tags first
        if let Ok(tag) = Tag::read_from_path(path) {
            let title = tag.title().unwrap_or("");
            let artist = tag.artist().unwrap_or("");
            let album = tag.album().unwrap_or("");
            let album_artist = tag.album_artist().unwrap_or("");
            let date = tag
                .date_recorded()
                .map(|d| d.to_string())
                .or_else(|| tag.year().map(|y| y.to_string()))
                .unwrap_or_default();
            let year = tag
                .year()
                .map(|y| y.to_string())
                .or_else(|| {
                    if date.len() >= 4 {
                        Some(date[0..4].to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let genre = tag.genre().unwrap_or("").to_string();
            let track_number = tag.track().map(|n| n.to_string()).unwrap_or_default();

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    album_artist: album_artist.to_string(),
                    date,
                    year,
                    genre,
                    track_number,
                };
            }
        }

        // Fall back to APE tags
        if let Ok(ape_tag) = ape::read_from_path(path) {
            let title: &str = ape_tag
                .item("title")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let artist: &str = ape_tag
                .item("artist")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let album: &str = ape_tag
                .item("album")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let album_artist: &str = ape_tag
                .item("album artist")
                .or_else(|| ape_tag.item("albumartist"))
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let date: &str = ape_tag
                .item("year")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let year: &str = ape_tag
                .item("year")
                .or_else(|| ape_tag.item("date"))
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let genre: &str = ape_tag
                .item("genre")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let track_number: &str = ape_tag
                .item("track")
                .or_else(|| ape_tag.item("tracknumber"))
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    album_artist: album_artist.to_string(),
                    date: date.to_string(),
                    year: year.to_string(),
                    genre: genre.to_string(),
                    track_number: track_number.to_string(),
                };
            }
        }

        // Fall back to FLAC tags
        if let Ok(flac_tag) = metaflac::Tag::read_from_path(path) {
            let title = flac_tag
                .get_vorbis("title")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let artist = flac_tag
                .get_vorbis("artist")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let album = flac_tag
                .get_vorbis("album")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let album_artist = flac_tag
                .get_vorbis("albumartist")
                .and_then(|mut i| i.next())
                .or_else(|| {
                    flac_tag
                        .get_vorbis("album artist")
                        .and_then(|mut i| i.next())
                })
                .unwrap_or("");
            let date = flac_tag
                .get_vorbis("date")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let year = flac_tag
                .get_vorbis("year")
                .and_then(|mut i| i.next())
                .or_else(|| {
                    if date.len() >= 4 {
                        Some(&date[0..4])
                    } else {
                        None
                    }
                })
                .unwrap_or("");
            let genre = flac_tag
                .get_vorbis("genre")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let track_number = flac_tag
                .get_vorbis("tracknumber")
                .and_then(|mut i| i.next())
                .or_else(|| flac_tag.get_vorbis("track").and_then(|mut i| i.next()))
                .unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    album_artist: album_artist.to_string(),
                    date: date.to_string(),
                    year: year.to_string(),
                    genre: genre.to_string(),
                    track_number: track_number.to_string(),
                };
            }
        }

        // If no tags found, use filename as title
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();

        TrackMetadata {
            title: filename,
            artist: "".to_string(),
            album: "".to_string(),
            album_artist: "".to_string(),
            date: "".to_string(),
            year: "".to_string(),
            genre: "".to_string(),
            track_number: "".to_string(),
        }
    }

    /// Handles selection gesture start from the UI overlay.
    pub fn on_pointer_down(&mut self, pressed_index: usize, ctrl: bool, shift: bool) {
        debug!(
            "on_pointer_down: index={}, ctrl={}, shift={}",
            pressed_index, ctrl, shift
        );
        self.pressed_index = Some(pressed_index);

        let is_already_selected = self.selected_indices.contains(&pressed_index);
        if !is_already_selected || ctrl || shift {
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectTrackMulti {
                    index: pressed_index,
                    ctrl,
                    shift,
                },
            ));
        }
    }

    /// Starts drag state for track row reordering.
    pub fn on_drag_start(&mut self, pressed_index: usize) {
        debug!(
            ">>> on_drag_start START: pressed_index={}, self.selected_indices={:?}",
            pressed_index, self.selected_indices
        );
        if self.selected_indices.contains(&pressed_index) {
            self.drag_indices = self.selected_indices.clone();
            debug!(
                ">>> MULTI-SELECT DRAG: using drag_indices {:?}",
                self.drag_indices
            );
        } else {
            self.drag_indices = vec![pressed_index];
            debug!(">>> SINGLE DRAG: using index {:?}", self.drag_indices);
        }
        self.drag_indices.sort();
        debug!(
            ">>> on_drag_start END: drag_indices={:?}",
            self.drag_indices
        );
        self.is_dragging = true;
    }

    /// Updates the visual drag target gap during row drag.
    pub fn on_drag_move(&mut self, drop_gap: usize) {
        if self.is_dragging {
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                ui.set_drop_index(drop_gap as i32);
            });
        }
    }

    /// Finalizes drag state and emits track reorder command when applicable.
    pub fn on_drag_end(&mut self, drop_gap: usize) {
        debug!(
            ">>> on_drag_end START: is_dragging={}, drop_gap={}, drag_indices={:?}",
            self.is_dragging, drop_gap, self.drag_indices
        );
        if self.is_dragging && !self.drag_indices.is_empty() {
            let indices = self.drag_indices.clone();
            let to = drop_gap;
            debug!(
                ">>> on_drag_end SENDING ReorderTracks: indices={:?}, to={}",
                indices, to
            );

            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::ReorderTracks { indices, to },
            ));
        }

        self.drag_indices.clear();
        self.is_dragging = false;
        self.pressed_index = None;

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_is_dragging(false);
            ui.set_drop_index(-1);
            ui.set_pressed_index(-1);
        });
    }

    /// Starts the blocking UI event loop that listens for bus messages.
    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => {
                    self.on_message_received();
                    match message {
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistsRestored(playlists),
                        ) => {
                            let old_len = self.playlist_ids.len();
                            self.playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
                            let new_len = self.playlist_ids.len();
                            let mut slint_playlists = Vec::new();
                            for p in playlists {
                                slint_playlists.push(StandardListViewItem::from(p.name.as_str()));
                            }

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_playlists(ModelRc::from(Rc::new(VecModel::from(
                                    slint_playlists,
                                ))));
                                if new_len > old_len && old_len > 0 {
                                    ui.set_editing_playlist_index((new_len - 1) as i32);
                                }
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistChanged(id),
                        ) => {
                            self.active_playlist_id = id.clone();
                            self.playlist_column_width_overrides_px.clear();
                            self.apply_playlist_column_layout();
                            if let Some(index) =
                                self.playlist_ids.iter().position(|p_id| p_id == &id)
                            {
                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_active_playlist_index(index as i32);
                                });
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistRestored(tracks),
                        ) => {
                            self.track_ids.clear();
                            self.track_paths.clear();
                            self.track_metadata.clear();

                            let mut track_row_values: Vec<Vec<String>> = Vec::new();
                            for track in tracks {
                                self.track_ids.push(track.id.clone());
                                self.track_paths.push(track.path.clone());
                                let metadata = self.read_track_metadata(&track.path);
                                self.track_metadata.push(metadata.clone());

                                let row_values = Self::build_playlist_row_values(
                                    &metadata,
                                    &self.playlist_columns,
                                );
                                track_row_values.push(row_values);
                            }
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                while track_model.row_count() > 0 {
                                    track_model.remove(0);
                                }

                                for row_values in track_row_values {
                                    let values_shared: Vec<slint::SharedString> =
                                        row_values.into_iter().map(Into::into).collect();
                                    track_model.push(TrackRowData {
                                        status: "".into(),
                                        values: ModelRc::from(values_shared.as_slice()),
                                        selected: false,
                                    });
                                }
                            });
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::TrackAdded {
                            id,
                            path,
                        }) => {
                            self.track_ids.push(id.clone());
                            self.track_paths.push(path.clone());
                            let tags = self.read_track_metadata(&path);
                            self.track_metadata.push(tags.clone());
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();

                            let row_values =
                                Self::build_playlist_row_values(&tags, &self.playlist_columns);
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");
                                let values_shared: Vec<slint::SharedString> =
                                    row_values.into_iter().map(Into::into).collect();
                                track_model.push(TrackRowData {
                                    status: "".into(),
                                    values: ModelRc::from(values_shared.as_slice()),
                                    selected: false,
                                });
                            });
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTracks(
                            mut indices,
                        )) => {
                            indices.sort_by(|a, b| b.cmp(a));
                            let indices_to_remove = indices.clone();

                            for index in indices {
                                if index < self.track_ids.len() {
                                    self.track_ids.remove(index);
                                }
                                if index < self.track_paths.len() {
                                    self.track_paths.remove(index);
                                }
                                if index < self.track_metadata.len() {
                                    self.track_metadata.remove(index);
                                }
                            }
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for index in indices_to_remove {
                                    if index < track_model.row_count() {
                                        track_model.remove(index);
                                    }
                                }
                            });
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::DeleteSelected) => {
                            if self.selected_indices.is_empty() {
                                continue;
                            }
                            let indices = self.selected_indices.clone();
                            let _ = self.bus_sender.send(protocol::Message::Playlist(
                                protocol::PlaylistMessage::DeleteTracks(indices),
                            ));
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::PlayTrackByIndex(index),
                        ) => {
                            let status_text = self
                                .track_metadata
                                .get(index)
                                .map(Self::status_text_from_track_metadata)
                                .unwrap_or_else(|| "".into());
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    if i == index {
                                        row.status = "▶️".into();
                                        track_model.set_row_data(i, row);
                                    } else if !row.status.is_empty() {
                                        row.status = "".into();
                                        track_model.set_row_data(i, row);
                                    }
                                }
                                ui.set_playing_track_index(index as i32);
                                ui.set_status_text(status_text);
                            });
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                            let metadata_snapshot = self.track_metadata.clone();
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let playing_index = ui.get_playing_track_index();
                                if playing_index >= 0 {
                                    let track_model_strong = ui.get_track_model();
                                    let track_model = track_model_strong
                                        .as_any()
                                        .downcast_ref::<VecModel<TrackRowData>>()
                                        .expect("VecModel<TrackRowData> expected");
                                    if (playing_index as usize) < track_model.row_count() {
                                        let mut row =
                                            track_model.row_data(playing_index as usize).unwrap();
                                        row.status = "▶️".into();
                                        track_model.set_row_data(playing_index as usize, row);
                                        if (playing_index as usize) < metadata_snapshot.len() {
                                            ui.set_status_text(
                                                UiManager::status_text_from_track_metadata(
                                                    &metadata_snapshot[playing_index as usize],
                                                ),
                                            );
                                        }
                                    }
                                }
                            });
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let playing_index = ui.get_playing_track_index();
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                if playing_index >= 0
                                    && (playing_index as usize) < track_model.row_count()
                                {
                                    let mut row =
                                        track_model.row_data(playing_index as usize).unwrap();
                                    row.status = "⏸️".into();
                                    track_model.set_row_data(playing_index as usize, row);
                                }
                            });
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::PlaybackProgress {
                                elapsed_ms,
                                total_ms,
                            },
                        ) => {
                            self.last_progress_at = Some(Instant::now());
                            if self.progress_rl.check().is_ok() {
                                // Check if the displayed second has changed (for text updates)
                                let elapsed_secs = elapsed_ms / 1000;
                                let last_elapsed_secs = self.last_elapsed_ms / 1000;
                                let time_text_changed = elapsed_secs != last_elapsed_secs
                                    || total_ms != self.last_total_ms;

                                // Always compute percentage for smooth progress bar
                                let percentage = if total_ms > 0 {
                                    elapsed_ms as f32 / total_ms as f32
                                } else {
                                    0.0
                                };

                                if time_text_changed {
                                    // Update cached values and set integer ms properties
                                    // This triggers Slint's pure functions to recompute text
                                    self.last_elapsed_ms = elapsed_ms;
                                    self.last_total_ms = total_ms;

                                    let elapsed_ms_i32 = elapsed_ms as i32;
                                    let total_ms_i32 = total_ms as i32;

                                    let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_elapsed_ms(elapsed_ms_i32);
                                        ui.set_total_ms(total_ms_i32);
                                        ui.set_position_percentage(percentage);
                                    });
                                } else {
                                    // Just update the progress bar percentage for smooth animation
                                    let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                        ui.set_position_percentage(percentage);
                                    });
                                }
                            }
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::TechnicalMetadataChanged(meta),
                        ) => {
                            debug!("UiManager: Technical metadata changed: {:?}", meta);
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_technical_info(
                                    format!(
                                        "{} | {} kbps | {} Hz",
                                        meta.format, meta.bitrate_kbps, meta.sample_rate_hz
                                    )
                                    .into(),
                                );
                            });
                        }
                        protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                            self.playback_active = false;
                            self.last_progress_at = None;
                            self.current_playing_track_path = None;
                            self.current_playing_track_metadata = None;
                            let selected_indices = self.selected_indices.clone();
                            self.update_display_for_selection(&selected_indices, None, None);

                            // Reset cached progress values
                            self.last_elapsed_ms = 0;
                            self.last_total_ms = 0;

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_technical_info("".into());
                                ui.set_status_text("No track selected".into());
                                ui.set_position_percentage(0.0);
                                ui.set_elapsed_ms(0);
                                ui.set_total_ms(0);

                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    if !row.status.is_empty() {
                                        row.status = "".into();
                                        track_model.set_row_data(i, row);
                                    }
                                }
                                ui.set_playing_track_index(-1);
                            });
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted {
                            index,
                            playlist_id,
                        }) => {
                            let is_active_playlist = playlist_id == self.active_playlist_id;
                            let status_text = self
                                .track_metadata
                                .get(index)
                                .map(Self::status_text_from_track_metadata)
                                .unwrap_or_else(|| "".into());

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if is_active_playlist {
                                    let track_model_strong = ui.get_track_model();
                                    let track_model = track_model_strong
                                        .as_any()
                                        .downcast_ref::<VecModel<TrackRowData>>()
                                        .expect("VecModel<TrackRowData> expected");

                                    for i in 0..track_model.row_count() {
                                        let mut row = track_model.row_data(i).unwrap();
                                        if i == index {
                                            row.status = "▶️".into();
                                            track_model.set_row_data(i, row);
                                        } else if !row.status.is_empty() {
                                            row.status = "".into();
                                            track_model.set_row_data(i, row);
                                        }
                                    }
                                    ui.set_selected_track_index(index as i32);
                                    ui.set_playing_track_index(index as i32);
                                } else {
                                    ui.set_playing_track_index(-1);
                                }
                                ui.set_status_text(status_text);
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistIndicesChanged {
                                playing_playlist_id,
                                playing_index,
                                playing_track_path,
                                playing_track_metadata,
                                selected_indices,
                                is_playing,
                                playback_order,
                                repeat_mode,
                            },
                        ) => {
                            self.playback_active = is_playing;
                            if !is_playing {
                                self.last_progress_at = None;
                            }
                            let selected_indices_clone = selected_indices.clone();
                            self.selected_indices = selected_indices_clone.clone();
                            let is_playing_active_playlist =
                                playing_playlist_id.as_ref() == Some(&self.active_playlist_id);

                            self.current_playing_track_path = playing_track_path.clone();
                            self.current_playing_track_metadata = playing_track_metadata.clone();
                            self.update_display_for_selection(
                                &selected_indices,
                                playing_track_path.as_ref(),
                                playing_track_metadata.as_ref(),
                            );

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if is_playing_active_playlist {
                                    ui.set_playing_track_index(
                                        playing_index.map(|i| i as i32).unwrap_or(-1),
                                    );
                                } else {
                                    ui.set_playing_track_index(-1);
                                }

                                let repeat_int = match repeat_mode {
                                    protocol::RepeatMode::Off => 0,
                                    protocol::RepeatMode::Playlist => 1,
                                    protocol::RepeatMode::Track => 2,
                                };
                                ui.set_repeat_mode(repeat_int);

                                let order_int = match playback_order {
                                    protocol::PlaybackOrder::Default => 0,
                                    protocol::PlaybackOrder::Shuffle => 1,
                                    protocol::PlaybackOrder::Random => 2,
                                };
                                ui.set_playback_order_index(order_int);

                                ui.set_selected_track_index(
                                    selected_indices_clone
                                        .first()
                                        .copied()
                                        .map(|i| i as i32)
                                        .unwrap_or(-1),
                                );

                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    let was_selected = row.selected;
                                    let should_be_selected = selected_indices_clone.contains(&i);

                                    if was_selected != should_be_selected {
                                        row.selected = should_be_selected;
                                        track_model.set_row_data(i, row);
                                    }
                                }

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    if is_playing_active_playlist && playing_index == Some(i) {
                                        let emoji = if is_playing { "▶️" } else { "⏸️" };
                                        row.status = emoji.into();
                                        track_model.set_row_data(i, row);
                                    } else if !row.status.is_empty() {
                                        row.status = "".into();
                                        track_model.set_row_data(i, row);
                                    }
                                }
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::SelectionChanged(indices),
                        ) => {
                            debug!(
                                "SelectionChanged: setting selected_indices to {:?}",
                                indices
                            );
                            let indices_clone = indices.clone();
                            self.selected_indices = indices_clone.clone();
                            debug!(
                                "After SelectionChanged: self.selected_indices = {:?}",
                                self.selected_indices
                            );

                            // Update cover art and metadata display based on selection.
                            // If nothing is selected, fall back to the currently playing track.
                            let playing_track_path = self.current_playing_track_path.clone();
                            let playing_track_metadata =
                                self.current_playing_track_metadata.clone();
                            self.update_display_for_selection(
                                &indices,
                                playing_track_path.as_ref(),
                                playing_track_metadata.as_ref(),
                            );

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    let should_be_selected = indices_clone.contains(&i);
                                    if row.selected != should_be_selected {
                                        row.selected = should_be_selected;
                                        track_model.set_row_data(i, row);
                                    }
                                }
                            });
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnPointerDown {
                            index,
                            ctrl,
                            shift,
                        }) => {
                            self.on_pointer_down(index, ctrl, shift);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnDragStart {
                            pressed_index,
                        }) => {
                            self.on_drag_start(pressed_index);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnDragMove {
                            drop_gap,
                        }) => {
                            self.on_drag_move(drop_gap);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::OnDragEnd {
                            drop_gap,
                        }) => {
                            self.on_drag_end(drop_gap);
                        }
                        protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                            indices,
                            to,
                        }) => {
                            debug!("ReorderTracks: indices={:?}, to={}", indices, to);

                            let indices_clone = indices.clone();

                            let mut sorted_indices = indices.clone();
                            sorted_indices.sort_unstable();
                            sorted_indices.dedup();
                            sorted_indices.retain(|&i| i < self.track_paths.len());

                            if sorted_indices.is_empty() {
                                continue;
                            }

                            let to = to.min(self.track_paths.len());

                            let first = sorted_indices[0];
                            let last = *sorted_indices.last().unwrap();
                            let block_len = sorted_indices.len();

                            let is_contiguous = sorted_indices
                                .iter()
                                .enumerate()
                                .all(|(k, &i)| i == first + k);

                            if is_contiguous && to >= first && to <= last + 1 {
                                continue;
                            }

                            let mut moved_paths = Vec::new();
                            let mut moved_ids = Vec::new();
                            let mut moved_metadata = Vec::new();

                            for &idx in sorted_indices.iter().rev() {
                                if idx < self.track_paths.len() {
                                    moved_paths.push(self.track_paths.remove(idx));
                                }
                                if idx < self.track_ids.len() {
                                    moved_ids.push(self.track_ids.remove(idx));
                                }
                                if idx < self.track_metadata.len() {
                                    moved_metadata.push(self.track_metadata.remove(idx));
                                }
                            }
                            moved_paths.reverse();
                            moved_ids.reverse();
                            moved_metadata.reverse();

                            let removed_before = sorted_indices.iter().filter(|&&i| i < to).count();
                            let insert_at = to.saturating_sub(removed_before);

                            for (i, path) in moved_paths.into_iter().enumerate() {
                                self.track_paths.insert(insert_at + i, path);
                            }
                            for (i, id) in moved_ids.into_iter().enumerate() {
                                self.track_ids.insert(insert_at + i, id);
                            }
                            for (i, metadata) in moved_metadata.into_iter().enumerate() {
                                self.track_metadata.insert(insert_at + i, metadata);
                            }

                            self.selected_indices = (insert_at..insert_at + block_len).collect();

                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                let mut sorted_indices = indices_clone.clone();
                                sorted_indices.sort_unstable();
                                sorted_indices.dedup();
                                sorted_indices.retain(|&i| i < track_model.row_count());

                                if sorted_indices.is_empty() {
                                    return;
                                }

                                let to = to.min(track_model.row_count());
                                let first = sorted_indices[0];
                                let last = *sorted_indices.last().unwrap();

                                if is_contiguous && to >= first && to <= last + 1 {
                                    return;
                                }

                                let mut moved_rows = Vec::new();
                                for &idx in sorted_indices.iter().rev() {
                                    if idx < track_model.row_count() {
                                        moved_rows.push(track_model.row_data(idx).unwrap());
                                        track_model.remove(idx);
                                    }
                                }
                                moved_rows.reverse();

                                let removed_before =
                                    sorted_indices.iter().filter(|&&i| i < to).count();
                                let insert_at = to.saturating_sub(removed_before);

                                for (i, row) in moved_rows.into_iter().enumerate() {
                                    track_model.insert(insert_at + i, row);
                                }
                            });
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::CoverArtChanged(path),
                        ) => {
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if let Some(path) = path {
                                    if let Ok(img) = slint::Image::load_from_path(&path) {
                                        ui.set_current_cover_art(img);
                                    } else {
                                        ui.set_current_cover_art(slint::Image::default());
                                    }
                                } else {
                                    ui.set_current_cover_art(slint::Image::default());
                                }
                            });
                        }
                        protocol::Message::Playback(
                            protocol::PlaybackMessage::MetadataDisplayChanged(meta),
                        ) => {
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                if let Some(meta) = meta {
                                    ui.set_display_title(meta.title.into());
                                    ui.set_display_artist(meta.artist.into());
                                    ui.set_display_album(meta.album.into());
                                    ui.set_display_date(meta.date.into());
                                    ui.set_display_genre(meta.genre.into());
                                } else {
                                    ui.set_display_title("".into());
                                    ui.set_display_artist("".into());
                                    ui.set_display_album("".into());
                                    ui.set_display_date("".into());
                                    ui.set_display_genre("".into());
                                }
                            });
                        }
                        protocol::Message::Config(protocol::ConfigMessage::ConfigChanged(
                            config,
                        )) => {
                            self.playlist_columns = config.ui.playlist_columns.clone();
                            let valid_column_keys: HashSet<String> = self
                                .playlist_columns
                                .iter()
                                .map(Self::playlist_column_key)
                                .collect();
                            self.playlist_column_width_overrides_px
                                .retain(|key, _| valid_column_keys.contains(key));
                            self.refresh_playlist_column_content_targets();
                            self.apply_playlist_column_layout();
                            let playlist_columns = self.playlist_columns.clone();
                            let track_metadata = self.track_metadata.clone();

                            let visible_headers: Vec<slint::SharedString> = playlist_columns
                                .iter()
                                .filter(|column| column.enabled)
                                .map(|column| column.name.as_str().into())
                                .collect();
                            let menu_labels: Vec<slint::SharedString> = playlist_columns
                                .iter()
                                .map(|column| column.name.as_str().into())
                                .collect();
                            let menu_checked: Vec<bool> = playlist_columns
                                .iter()
                                .map(|column| column.enabled)
                                .collect();
                            let menu_is_custom: Vec<bool> = playlist_columns
                                .iter()
                                .map(|column| column.custom)
                                .collect();
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_show_album_art(true);
                                ui.set_playlist_visible_column_headers(ModelRc::from(Rc::new(
                                    VecModel::from(visible_headers),
                                )));
                                ui.set_playlist_column_menu_labels(ModelRc::from(Rc::new(
                                    VecModel::from(menu_labels),
                                )));
                                ui.set_playlist_column_menu_checked(ModelRc::from(Rc::new(
                                    VecModel::from(menu_checked),
                                )));
                                ui.set_playlist_column_menu_is_custom(ModelRc::from(Rc::new(
                                    VecModel::from(menu_is_custom),
                                )));

                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    if i < track_metadata.len() {
                                        let values = UiManager::build_playlist_row_values(
                                            &track_metadata[i],
                                            &playlist_columns,
                                        );
                                        let values_shared: Vec<slint::SharedString> =
                                            values.into_iter().map(Into::into).collect();
                                        row.values = ModelRc::from(values_shared.as_slice());
                                        track_model.set_row_data(i, row);
                                    }
                                }
                            });
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::PlaylistViewportWidthChanged(width_px),
                        ) => {
                            if self.playlist_columns_available_width_px != width_px {
                                self.playlist_columns_available_width_px = width_px;
                                self.apply_playlist_column_layout();
                            }
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ActivePlaylistColumnWidthOverrides(
                                overrides,
                            ),
                        ) => {
                            self.playlist_column_width_overrides_px.clear();
                            if let Some(overrides) = overrides {
                                for override_item in overrides {
                                    if override_item.column_key.trim().is_empty() {
                                        continue;
                                    }
                                    self.playlist_column_width_overrides_px.insert(
                                        override_item.column_key,
                                        override_item.width_px.max(48),
                                    );
                                }
                            }
                            self.apply_playlist_column_layout();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::SetActivePlaylistColumnWidthOverride {
                                column_key,
                                width_px,
                                ..
                            },
                        ) => {
                            if column_key.trim().is_empty() {
                                continue;
                            }
                            self.playlist_column_width_overrides_px
                                .insert(column_key, width_px.max(48));
                            self.apply_playlist_column_layout();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::ClearActivePlaylistColumnWidthOverride {
                                column_key,
                                ..
                            },
                        ) => {
                            self.playlist_column_width_overrides_px.remove(&column_key);
                            self.apply_playlist_column_layout();
                        }
                        protocol::Message::Playlist(
                            protocol::PlaylistMessage::RepeatModeChanged(repeat_mode),
                        ) => {
                            debug!("UiManager: Repeat mode changed: {:?}", repeat_mode);
                            let repeat_int = match repeat_mode {
                                protocol::RepeatMode::Off => 0,
                                protocol::RepeatMode::Playlist => 1,
                                protocol::RepeatMode::Track => 2,
                            };
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_repeat_mode(repeat_int);
                            });
                        }
                        _ => {}
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    self.on_message_lagged();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fit_column_widths_to_available_space, CoverArtLookupRequest, TrackMetadata, UiManager,
    };
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn make_meta(title: &str) -> TrackMetadata {
        TrackMetadata {
            title: title.to_string(),
            artist: format!("{title}-artist"),
            album: format!("{title}-album"),
            album_artist: format!("{title}-album-artist"),
            date: "2026".to_string(),
            year: "2026".to_string(),
            genre: "test".to_string(),
            track_number: "1".to_string(),
        }
    }

    #[test]
    fn test_coalesce_cover_art_requests_keeps_latest() {
        let (tx, rx) = mpsc::channel::<CoverArtLookupRequest>();
        tx.send(CoverArtLookupRequest {
            track_path: Some(PathBuf::from("first.mp3")),
        })
        .expect("failed to queue request");
        tx.send(CoverArtLookupRequest {
            track_path: Some(PathBuf::from("second.mp3")),
        })
        .expect("failed to queue request");
        tx.send(CoverArtLookupRequest { track_path: None })
            .expect("failed to queue request");

        let first = rx.recv().expect("expected first request");
        let latest = UiManager::coalesce_cover_art_requests(first, &rx);
        assert_eq!(latest.track_path, None);
    }

    #[test]
    fn test_resolve_display_target_prefers_selection() {
        let selected = vec![1usize];
        let paths = vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")];
        let metadata = vec![make_meta("A"), make_meta("B")];
        let playing_path = Some(PathBuf::from("playing.mp3"));
        let playing_meta = crate::protocol::DetailedMetadata {
            title: "Playing".to_string(),
            artist: "P".to_string(),
            album: "P".to_string(),
            date: "".to_string(),
            genre: "".to_string(),
        };

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            Some(&playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("selected metadata should exist");
        assert_eq!(meta.title, "B");
    }

    #[test]
    fn test_resolve_display_target_falls_back_to_playing_when_no_selection() {
        let selected = vec![];
        let paths = vec![PathBuf::from("a.mp3")];
        let metadata = vec![make_meta("A")];
        let playing_path = Some(PathBuf::from("playing.mp3"));
        let playing_meta = crate::protocol::DetailedMetadata {
            title: "Playing".to_string(),
            artist: "P".to_string(),
            album: "P".to_string(),
            date: "".to_string(),
            genre: "".to_string(),
        };

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            Some(&playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("playing.mp3")));
        let meta = meta.expect("playing metadata should exist");
        assert_eq!(meta.title, "Playing");
    }

    #[test]
    fn test_resolve_display_target_prefers_cached_metadata_for_playing_path_match() {
        let selected = vec![];
        let paths = vec![PathBuf::from("a.mp3"), PathBuf::from("b.mp3")];
        let metadata = vec![make_meta("A"), make_meta("B")];
        let playing_path = Some(PathBuf::from("b.mp3"));
        let stale_playing_meta = crate::protocol::DetailedMetadata {
            title: "stale".to_string(),
            artist: "stale".to_string(),
            album: "stale".to_string(),
            date: String::new(),
            genre: String::new(),
        };

        let (path, meta) = UiManager::resolve_display_target(
            &selected,
            &paths,
            &metadata,
            playing_path.as_ref(),
            Some(&stale_playing_meta),
        );

        assert_eq!(path, Some(PathBuf::from("b.mp3")));
        let meta = meta.expect("playing metadata should exist");
        assert_eq!(meta.title, "B");
        assert_eq!(meta.artist, "B-artist");
    }

    #[test]
    fn test_resolve_display_target_returns_none_without_selection_or_playing() {
        let selected = vec![];
        let paths = vec![PathBuf::from("a.mp3")];
        let metadata = vec![make_meta("A")];

        let (path, meta) =
            UiManager::resolve_display_target(&selected, &paths, &metadata, None, None);

        assert!(path.is_none());
        assert!(meta.is_none());
    }

    #[test]
    fn test_render_column_value_replaces_placeholders() {
        let metadata = make_meta("Song");
        let rendered = UiManager::render_column_value(&metadata, "{album} ({year})");
        assert_eq!(rendered, "Song-album (2026)");
    }

    #[test]
    fn test_render_column_value_handles_escaping_and_unknown_fields() {
        let metadata = make_meta("Song");
        let rendered = UiManager::render_column_value(&metadata, "{{{unknown}}} - {artist}");
        assert_eq!(rendered, "{} - Song-artist");
    }

    #[test]
    fn test_fit_column_widths_shrinks_to_available_width() {
        let mut widths = vec![220u32, 180u32, 200u32];
        let mins = vec![100u32, 120u32, 130u32];
        let maxs = vec![400u32, 360u32, 380u32];

        fit_column_widths_to_available_space(&mut widths, &mins, &maxs, 420);

        assert_eq!(widths.iter().copied().sum::<u32>(), 420);
        assert!(widths[0] >= mins[0] && widths[0] <= maxs[0]);
        assert!(widths[1] >= mins[1] && widths[1] <= maxs[1]);
        assert!(widths[2] >= mins[2] && widths[2] <= maxs[2]);
    }

    #[test]
    fn test_fit_column_widths_keeps_targets_when_space_is_available() {
        let mut widths = vec![120u32, 140u32, 160u32];
        let mins = vec![90u32, 100u32, 120u32];
        let maxs = vec![260u32, 280u32, 300u32];

        fit_column_widths_to_available_space(&mut widths, &mins, &maxs, 640);

        assert_eq!(widths, vec![120u32, 140u32, 160u32]);
    }

    #[test]
    fn test_fit_column_widths_stops_at_minimums_when_viewport_too_small() {
        let mut widths = vec![150u32, 170u32, 190u32];
        let mins = vec![100u32, 120u32, 140u32];
        let maxs = vec![260u32, 280u32, 300u32];

        fit_column_widths_to_available_space(&mut widths, &mins, &maxs, 120);

        assert_eq!(widths, mins);
    }

    fn apply_reorder(paths: &mut Vec<String>, indices: &[usize], gap: usize) -> Vec<usize> {
        let len = paths.len();
        if len == 0 {
            return vec![];
        }

        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        indices.retain(|&i| i < len);

        if indices.is_empty() {
            return vec![];
        }

        let gap = gap.min(len);

        let first = indices[0];
        let last = *indices.last().unwrap();
        let block_len = indices.len();

        let is_contiguous = indices.iter().enumerate().all(|(k, &i)| i == first + k);

        if is_contiguous && gap >= first && gap <= last + 1 {
            return indices;
        }

        let mut moved = Vec::new();
        for &idx in indices.iter().rev() {
            if idx < paths.len() {
                moved.push(paths.remove(idx));
            }
        }
        moved.reverse();

        let removed_before = indices.iter().filter(|&&i| i < gap).count();
        let insert_at = gap.saturating_sub(removed_before);

        for (offset, item) in moved.into_iter().enumerate() {
            paths.insert(insert_at + offset, item);
        }

        (insert_at..insert_at + block_len).collect()
    }

    // Gap-index semantic tests
    // Gap 0 = before element 0
    // Gap 1 = between element 0 and 1
    // Gap 2 = between element 1 and 2
    // Gap N = after element N-1

    fn make_test_paths() -> Vec<String> {
        vec![
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            "D".to_string(),
        ]
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_0_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 0);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_2() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 2);
        assert_eq!(paths, vec!["B", "A", "C", "D"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_3() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 3);
        assert_eq!(paths, vec!["B", "C", "A", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 4);
        assert_eq!(paths, vec!["B", "C", "D", "A"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 0);
        assert_eq!(paths, vec!["C", "A", "B", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 1);
        assert_eq!(paths, vec!["A", "C", "B", "D"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 0);
        assert_eq!(paths, vec!["D", "A", "B", "C"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_1() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 1);
        assert_eq!(paths, vec!["A", "D", "B", "C"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_2() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 2);
        assert_eq!(paths, vec!["A", "B", "D", "C"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_4_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 4);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_0_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 0);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_3() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 3);
        assert_eq!(paths, vec!["C", "A", "B", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 4);
        assert_eq!(paths, vec!["C", "D", "A", "B"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 0);
        assert_eq!(paths, vec!["B", "C", "A", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 4);
        assert_eq!(paths, vec!["A", "D", "B", "C"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 0);
        assert_eq!(paths, vec!["C", "D", "A", "B"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_1() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 1);
        assert_eq!(paths, vec!["A", "C", "D", "B"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_4_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 4);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_all_three_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1, 2], 4);
        assert_eq!(paths, vec!["D", "A", "B", "C"]);
        assert_eq!(new_selection, vec![1, 2, 3]);
    }

    #[test]
    fn test_gap_reorder_all_three_from_middle_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2, 3], 0);
        assert_eq!(paths, vec!["B", "C", "D", "A"]);
        assert_eq!(new_selection, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_reorder_non_contiguous_not_noop() {
        // Select A, B, D (non-contiguous) and drop at gap 2 (after B)
        // Should bring D up before C, even though A and B don't move
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1, 3], 2);
        assert_eq!(paths, vec!["A", "B", "D", "C"]);
        assert_eq!(new_selection, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_reorder_non_contiguous_middle_gap() {
        // Select A, C (non-contiguous) and drop at gap 1 (after A)
        // Should move C to after A, resulting in [A, C, B, D]
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 2], 1);
        assert_eq!(paths, vec!["A", "C", "B", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }
}
