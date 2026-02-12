//! Online library enrichment runtime component.
//!
//! This manager fetches display-only artist/album blurbs (and artist images)
//! from Wikimedia sources, then stores short-lived cache records for the UI.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, warn};
use serde_json::Value;
use tokio::sync::broadcast::{
    error::{RecvError, TryRecvError},
    Receiver, Sender,
};

use crate::db_manager::DbManager;
use crate::protocol::{
    LibraryEnrichmentEntity, LibraryEnrichmentPayload, LibraryEnrichmentPriority,
    LibraryEnrichmentStatus, LibraryMessage, Message,
};

const WIKIPEDIA_ACTION_API_URL: &str = "https://en.wikipedia.org/w/api.php";
const WIKIPEDIA_REST_BASE_URL: &str = "https://en.wikipedia.org/w/rest.php/v1";
const WIKIDATA_ACTION_API_URL: &str = "https://www.wikidata.org/w/api.php";
const WIKIPEDIA_SOURCE_NAME: &str = "Wikipedia";
const WIKIDATA_SOURCE_NAME: &str = "Wikidata";
const IMAGE_CACHE_DIR_NAME: &str = "library_enrichment/images";
const READY_METADATA_TTL_DAYS: u32 = 30;
const NOT_FOUND_TTL_DAYS: u32 = 7;
const ERROR_TTL_DAYS: u32 = 1;
const MAX_CANDIDATES: usize = 12;
const MAX_SUMMARY_FETCHES: usize = 10;
const MAX_BLURB_CHARS: usize = 360;
const MAX_PENDING_PREFETCH_REQUESTS: usize = 64;
const PREFETCH_FETCH_BUDGET: Duration = Duration::from_millis(1800);
const INTERACTIVE_FETCH_BUDGET: Duration = Duration::from_millis(4500);
const WIKIMEDIA_USER_AGENT: &str =
    "roqtune/0.1.0 (https://github.com/roqtune/roqtune; contact: metadata enrichment)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnrichmentSource {
    Wikipedia,
    Wikidata,
}

impl EnrichmentSource {
    fn source_name(self) -> &'static str {
        match self {
            Self::Wikipedia => WIKIPEDIA_SOURCE_NAME,
            Self::Wikidata => WIKIDATA_SOURCE_NAME,
        }
    }
}

#[derive(Debug, Clone)]
struct WikiSummary {
    title: String,
    description: String,
    extract: String,
    canonical_url: String,
    image_url: Option<String>,
    page_type: String,
}

#[derive(Debug, Clone)]
struct WikidataCandidate {
    id: String,
    label: String,
    description: String,
    concept_url: String,
}

#[derive(Debug, Clone)]
struct FetchOutcome {
    payload: LibraryEnrichmentPayload,
    image_url: Option<String>,
    last_error: Option<String>,
}

/// Fetches and caches artist/album enrichment payloads for library views.
pub struct LibraryEnrichmentManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    db_manager: DbManager,
    online_metadata_enabled: bool,
    artist_image_cache_ttl_days: u32,
    artist_image_cache_max_size_mb: u32,
    queued_priorities: HashMap<LibraryEnrichmentEntity, LibraryEnrichmentPriority>,
    queued_interactive: VecDeque<LibraryEnrichmentEntity>,
    queued_prefetch: VecDeque<LibraryEnrichmentEntity>,
    in_flight_priorities: HashMap<LibraryEnrichmentEntity, LibraryEnrichmentPriority>,
    http_client: ureq::Agent,
}

impl LibraryEnrichmentManager {
    /// Creates a new manager bound to one bus receiver/sender pair.
    pub fn new(
        bus_consumer: Receiver<Message>,
        bus_producer: Sender<Message>,
        db_manager: DbManager,
    ) -> Self {
        let http_client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(7))
            .timeout_write(Duration::from_secs(7))
            .build();

        Self {
            bus_consumer,
            bus_producer,
            db_manager,
            online_metadata_enabled: false,
            artist_image_cache_ttl_days: 30,
            artist_image_cache_max_size_mb: 256,
            queued_priorities: HashMap::new(),
            queued_interactive: VecDeque::new(),
            queued_prefetch: VecDeque::new(),
            in_flight_priorities: HashMap::new(),
            http_client,
        }
    }

    fn now_unix_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    }

    fn source_entity_label(entity: &LibraryEnrichmentEntity) -> String {
        match entity {
            LibraryEnrichmentEntity::Artist { artist } => format!("artist:{artist}"),
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => format!("album:{album}\u{001f}{album_artist}"),
        }
    }

    fn collapse_whitespace(value: &str) -> String {
        value.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn normalize_text(value: &str) -> String {
        let mut normalized = String::with_capacity(value.len());
        for ch in value.chars() {
            if ch.is_ascii_alphanumeric() {
                normalized.push(ch.to_ascii_lowercase());
            } else if ch.is_ascii_whitespace() || ch == '-' || ch == '_' || ch == '/' {
                normalized.push(' ');
            }
        }
        normalized.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn compact_text(value: &str) -> String {
        value
            .chars()
            .filter_map(|ch| {
                ch.is_ascii_alphanumeric()
                    .then_some(ch.to_ascii_lowercase())
            })
            .collect()
    }

    fn title_case_words(value: &str) -> String {
        let collapsed = Self::collapse_whitespace(value);
        if collapsed.is_empty() {
            return collapsed;
        }
        collapsed
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                let Some(first) = chars.next() else {
                    return String::new();
                };
                let mut out = String::new();
                out.push(first.to_ascii_uppercase());
                out.push_str(&chars.as_str().to_ascii_lowercase());
                out
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn contains_word(haystack: &str, needle: &str) -> bool {
        haystack.split_whitespace().any(|token| {
            token == needle || token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric()) == needle
        })
    }

    fn contains_any(haystack: &str, values: &[&str]) -> bool {
        values.iter().any(|value| haystack.contains(value))
    }

    fn music_artist_keywords() -> &'static [&'static str] {
        &[
            "musician",
            "singer",
            "rapper",
            "songwriter",
            "composer",
            "producer",
            "dj",
            "band",
            "duo",
            "trio",
            "group",
            "idol",
            "artist",
        ]
    }

    fn album_keywords() -> &'static [&'static str] {
        &[
            "album",
            "ep",
            "mixtape",
            "soundtrack",
            "record",
            "release",
            "studio album",
            "live album",
            "compilation album",
        ]
    }

    fn title_parenthetical(title: &str) -> Option<String> {
        let open = title.find('(')?;
        let close = title.rfind(')')?;
        if close <= open + 1 {
            return None;
        }
        let inner = title[open + 1..close].trim();
        if inner.is_empty() {
            return None;
        }
        Some(Self::normalize_text(inner))
    }

    fn looks_disambiguation(summary: &WikiSummary) -> bool {
        let page_type = Self::normalize_text(&summary.page_type);
        let title = Self::normalize_text(&summary.title);
        let description = Self::normalize_text(&summary.description);
        page_type == "disambiguation"
            || description.contains("disambiguation")
            || title.contains("disambiguation")
    }

    fn looks_discography_or_list(summary: &WikiSummary) -> bool {
        let title = Self::normalize_text(&summary.title);
        let description = Self::normalize_text(&summary.description);
        let extract = Self::normalize_text(&summary.extract);
        title.contains("discography")
            || description.contains("discography")
            || extract.contains("discography")
            || title.starts_with("list of")
            || description.contains("list of")
    }

    fn has_album_context(summary: &WikiSummary) -> bool {
        let title = Self::normalize_text(&summary.title);
        let description = Self::normalize_text(&summary.description);
        let extract = Self::normalize_text(&summary.extract);
        Self::contains_any(&description, Self::album_keywords())
            || Self::contains_any(&extract, Self::album_keywords())
            || Self::contains_word(&title, "album")
            || Self::contains_word(&title, "ep")
    }

    fn looks_album_entity(summary: &WikiSummary) -> bool {
        let title = Self::normalize_text(&summary.title);
        let description = Self::normalize_text(&summary.description);
        let extract = Self::normalize_text(&summary.extract);
        Self::contains_any(
            &description,
            &[
                "album by",
                "studio album by",
                "ep by",
                "mixtape by",
                "soundtrack by",
                "compilation album by",
                "live album by",
            ],
        ) || Self::contains_any(
            &extract,
            &[
                "album by",
                "studio album by",
                "ep by",
                "mixtape by",
                "soundtrack by",
                "compilation album by",
                "live album by",
            ],
        ) || (Self::contains_word(&title, "album")
            && (Self::contains_word(&description, "album")
                || Self::contains_word(&extract, "album")))
    }

    fn title_has_artist_disambiguator(title: &str) -> bool {
        let normalized_title = Self::normalize_text(title);
        let parenthetical = Self::title_parenthetical(title).unwrap_or_default();
        Self::contains_any(&normalized_title, Self::music_artist_keywords())
            || Self::contains_any(&parenthetical, Self::music_artist_keywords())
    }

    fn has_artist_person_context(summary: &WikiSummary) -> bool {
        let description = Self::normalize_text(&summary.description);
        let extract = Self::normalize_text(&summary.extract);
        Self::contains_any(&description, Self::music_artist_keywords())
            || Self::contains_any(&extract, Self::music_artist_keywords())
    }

    fn word_overlap_ratio(left: &str, right: &str) -> f32 {
        let left_tokens: HashSet<&str> = left.split_whitespace().collect();
        let right_tokens: HashSet<&str> = right.split_whitespace().collect();
        if left_tokens.is_empty() || right_tokens.is_empty() {
            return 0.0;
        }
        let overlap = left_tokens.intersection(&right_tokens).count() as f32;
        overlap / (left_tokens.len().max(right_tokens.len()) as f32)
    }

    fn score_artist_summary(artist: &str, summary: &WikiSummary) -> i32 {
        let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(artist));
        let compact_artist = Self::compact_text(&normalized_artist);
        let normalized_title = Self::normalize_text(&summary.title);
        let compact_title = Self::compact_text(&normalized_title);
        let normalized_description = Self::normalize_text(&summary.description);
        let normalized_extract = Self::normalize_text(&summary.extract);

        if normalized_artist.is_empty() {
            return -10_000;
        }
        if Self::looks_disambiguation(summary) {
            return -10_000;
        }
        if Self::looks_discography_or_list(summary) {
            return -7_000;
        }
        if Self::looks_album_entity(summary) {
            return -8_000;
        }

        let has_music_context = Self::has_artist_person_context(summary);
        let has_artist_title_hint = Self::title_has_artist_disambiguator(&summary.title);
        if !has_music_context && !has_artist_title_hint {
            return -4_800;
        }

        let artist_word_count = normalized_artist.split_whitespace().count();
        let exact_compact_match = !compact_artist.is_empty() && compact_artist == compact_title;

        if artist_word_count == 1 && !exact_compact_match && !has_artist_title_hint {
            return -3_200;
        }

        let mut score = 0;
        if normalized_title == normalized_artist || exact_compact_match {
            score += 150;
        } else if !compact_artist.is_empty() && compact_title.starts_with(&compact_artist) {
            score += 105;
        } else if normalized_title.starts_with(&normalized_artist)
            || normalized_artist.starts_with(&normalized_title)
        {
            score += 80;
        } else if normalized_title.contains(&normalized_artist) {
            score += 55;
        }

        if Self::contains_any(&normalized_description, Self::music_artist_keywords())
            || Self::contains_any(&normalized_extract, Self::music_artist_keywords())
        {
            score += 32;
        }
        if has_artist_title_hint {
            score += 20;
        }

        let overlap = Self::word_overlap_ratio(&normalized_artist, &normalized_title);
        score += (overlap * 40.0).round() as i32;

        if artist_word_count >= 2 && overlap < 0.5 && !exact_compact_match {
            score -= 30;
        }
        score
    }

    fn score_album_summary(album: &str, album_artist: &str, summary: &WikiSummary) -> i32 {
        let normalized_album = Self::normalize_text(&Self::collapse_whitespace(album));
        let compact_album = Self::compact_text(&normalized_album);
        let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(album_artist));
        let normalized_title = Self::normalize_text(&summary.title);
        let compact_title = Self::compact_text(&normalized_title);
        let normalized_description = Self::normalize_text(&summary.description);
        let normalized_extract = Self::normalize_text(&summary.extract);

        if normalized_album.is_empty() {
            return -10_000;
        }
        if Self::looks_disambiguation(summary) {
            return -10_000;
        }
        if Self::looks_discography_or_list(summary) {
            return -7_000;
        }
        if !Self::has_album_context(summary) {
            return -4_800;
        }
        if Self::has_artist_person_context(summary) && !Self::has_album_context(summary) {
            return -6_200;
        }

        let mut score = 0;
        if normalized_title == normalized_album
            || (!compact_album.is_empty() && compact_album == compact_title)
        {
            score += 120;
        } else if normalized_title.starts_with(&normalized_album)
            || normalized_album.starts_with(&normalized_title)
        {
            score += 70;
        } else if normalized_title.contains(&normalized_album) {
            score += 45;
        }

        score += 35;
        if !normalized_artist.is_empty()
            && (normalized_extract.contains(&normalized_artist)
                || normalized_description.contains(&normalized_artist)
                || normalized_title.contains(&normalized_artist))
        {
            score += 28;
        }

        let overlap = Self::word_overlap_ratio(&normalized_album, &normalized_title);
        score += (overlap * 32.0).round() as i32;
        score
    }

    fn extract_title_strings(value: &Value) -> Vec<String> {
        let mut titles = Vec::new();

        if let Some(results) = value["query"]["search"].as_array() {
            for item in results {
                if let Some(title) = item["title"].as_str() {
                    let trimmed = title.trim();
                    if !trimmed.is_empty() {
                        titles.push(trimmed.to_string());
                    }
                }
            }
        }

        for key in ["pages", "results"] {
            if let Some(results) = value[key].as_array() {
                for item in results {
                    if let Some(title) = item["title"].as_str() {
                        let trimmed = title.trim();
                        if !trimmed.is_empty() {
                            titles.push(trimmed.to_string());
                        }
                    }
                }
            }
        }

        titles
    }

    fn http_get_json(&self, url: &str) -> Result<Value, String> {
        let response = self
            .http_client
            .get(url)
            .set("User-Agent", WIKIMEDIA_USER_AGENT)
            .set("Accept", "application/json")
            .call()
            .map_err(|error| format!("Request failed: {error}"))?;
        let mut body = String::new();
        response
            .into_reader()
            .read_to_string(&mut body)
            .map_err(|error| format!("Failed to read response: {error}"))?;
        serde_json::from_str(&body).map_err(|error| format!("Invalid JSON response: {error}"))
    }

    fn search_wikipedia_titles_action(&self, query: &str) -> Result<Vec<String>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}?action=query&list=search&srsearch={}&srlimit={}&format=json&utf8=1&maxlag=5",
            WIKIPEDIA_ACTION_API_URL, encoded_query, MAX_CANDIDATES
        );
        let parsed = self.http_get_json(&url)?;
        Ok(Self::extract_title_strings(&parsed))
    }

    fn search_wikipedia_titles_rest(
        &self,
        endpoint: &str,
        query: &str,
    ) -> Result<Vec<String>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}/{}?q={}&limit={}",
            WIKIPEDIA_REST_BASE_URL, endpoint, encoded_query, MAX_CANDIDATES
        );
        let parsed = self.http_get_json(&url)?;
        Ok(Self::extract_title_strings(&parsed))
    }

    fn fetch_wikipedia_summary(&self, title: &str) -> Result<WikiSummary, String> {
        let encoded_title = urlencoding::encode(title);
        let url = format!(
            "{}?action=query&prop=extracts|pageimages|description|pageprops|info&\
             inprop=url&redirects=1&exintro=1&explaintext=1&pithumbsize=640&titles={}&\
             format=json&utf8=1&maxlag=5",
            WIKIPEDIA_ACTION_API_URL, encoded_title
        );
        let parsed = self.http_get_json(&url)?;
        let pages = parsed["query"]["pages"]
            .as_object()
            .ok_or_else(|| "Wikipedia summary response missing pages".to_string())?;

        for (page_id, page_value) in pages {
            if page_id == "-1" {
                continue;
            }
            let title = page_value["title"]
                .as_str()
                .unwrap_or(title)
                .trim()
                .to_string();
            if title.is_empty() {
                continue;
            }

            let extract = page_value["extract"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let description = page_value["description"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let canonical_url = page_value["fullurl"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let image_url = page_value["thumbnail"]["source"]
                .as_str()
                .map(str::to_string);
            let page_type = if page_value["pageprops"]["disambiguation"].is_string()
                || page_value["pageprops"]["disambiguation"].is_object()
            {
                "disambiguation".to_string()
            } else {
                String::new()
            };

            return Ok(WikiSummary {
                title,
                description,
                extract,
                canonical_url,
                image_url,
                page_type,
            });
        }

        Err(format!("Wikipedia page not found for '{title}'"))
    }

    fn push_unique_title(
        titles: &mut Vec<String>,
        seen_titles: &mut HashSet<String>,
        title: String,
    ) {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            return;
        }
        let key = trimmed.to_ascii_lowercase();
        if seen_titles.insert(key) {
            titles.push(trimmed.to_string());
        }
    }

    fn append_titles_for_query(
        &self,
        query: &str,
        titles: &mut Vec<String>,
        seen_titles: &mut HashSet<String>,
        search_had_error: &mut bool,
    ) {
        if query.trim().is_empty() {
            return;
        }
        for fetch in [
            self.search_wikipedia_titles_action(query),
            self.search_wikipedia_titles_rest("search/title", query),
            self.search_wikipedia_titles_rest("search/page", query),
        ] {
            match fetch {
                Ok(found_titles) => {
                    for title in found_titles {
                        Self::push_unique_title(titles, seen_titles, title);
                    }
                }
                Err(_) => {
                    *search_had_error = true;
                }
            }
        }
    }

    fn candidate_titles_for_entity(
        &self,
        entity: &LibraryEnrichmentEntity,
    ) -> Result<Vec<String>, String> {
        let mut titles = Vec::new();
        let mut seen_titles = HashSet::new();
        let mut search_had_error = false;

        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                let cleaned_artist = Self::collapse_whitespace(artist);
                let normalized_artist = Self::normalize_text(&cleaned_artist);
                let compact_artist = normalized_artist.replace(' ', "");
                let title_case_artist = Self::title_case_words(&cleaned_artist);
                if cleaned_artist.is_empty() {
                    return Ok(Vec::new());
                }

                Self::push_unique_title(&mut titles, &mut seen_titles, cleaned_artist.clone());
                Self::push_unique_title(&mut titles, &mut seen_titles, title_case_artist.clone());
                if !compact_artist.is_empty() {
                    Self::push_unique_title(
                        &mut titles,
                        &mut seen_titles,
                        Self::title_case_words(&compact_artist),
                    );
                }
                Self::push_unique_title(
                    &mut titles,
                    &mut seen_titles,
                    format!("{cleaned_artist} (musician)"),
                );
                Self::push_unique_title(
                    &mut titles,
                    &mut seen_titles,
                    format!("{cleaned_artist} (band)"),
                );
                Self::push_unique_title(
                    &mut titles,
                    &mut seen_titles,
                    format!("{cleaned_artist} (singer)"),
                );

                for query in [
                    format!("\"{cleaned_artist}\""),
                    format!("\"{cleaned_artist}\" musician"),
                    format!("{cleaned_artist} musician"),
                    format!("\"{title_case_artist}\""),
                    format!("\"{title_case_artist}\" musician"),
                    format!("intitle:\"{cleaned_artist}\""),
                    format!("intitle:\"{title_case_artist}\""),
                    cleaned_artist.clone(),
                ] {
                    self.append_titles_for_query(
                        &query,
                        &mut titles,
                        &mut seen_titles,
                        &mut search_had_error,
                    );
                }
                if !compact_artist.is_empty()
                    && compact_artist != normalized_artist
                    && compact_artist != cleaned_artist.to_ascii_lowercase()
                {
                    self.append_titles_for_query(
                        &compact_artist,
                        &mut titles,
                        &mut seen_titles,
                        &mut search_had_error,
                    );
                }
            }
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => {
                let cleaned_album = Self::collapse_whitespace(album);
                let cleaned_artist = Self::collapse_whitespace(album_artist);
                let normalized_album = Self::normalize_text(&cleaned_album);
                let compact_album = normalized_album.replace(' ', "");
                let title_case_album = Self::title_case_words(&cleaned_album);
                let title_case_artist = Self::title_case_words(&cleaned_artist);
                if cleaned_album.is_empty() {
                    return Ok(Vec::new());
                }

                Self::push_unique_title(&mut titles, &mut seen_titles, cleaned_album.clone());
                Self::push_unique_title(&mut titles, &mut seen_titles, title_case_album.clone());
                if !compact_album.is_empty() {
                    Self::push_unique_title(
                        &mut titles,
                        &mut seen_titles,
                        Self::title_case_words(&compact_album),
                    );
                }
                Self::push_unique_title(
                    &mut titles,
                    &mut seen_titles,
                    format!("{cleaned_album} (album)"),
                );
                if !cleaned_artist.is_empty() {
                    Self::push_unique_title(
                        &mut titles,
                        &mut seen_titles,
                        format!("{cleaned_album} ({cleaned_artist} album)"),
                    );
                }

                let query_bundle = if cleaned_artist.is_empty() {
                    vec![
                        format!("\"{cleaned_album}\" album"),
                        format!("\"{title_case_album}\" album"),
                        cleaned_album.clone(),
                    ]
                } else {
                    vec![
                        format!("\"{cleaned_album}\" \"{cleaned_artist}\" album"),
                        format!("\"{title_case_album}\" \"{title_case_artist}\" album"),
                        format!("{cleaned_album} {cleaned_artist} album"),
                        format!("\"{cleaned_album}\" album"),
                        cleaned_album.clone(),
                        format!("intitle:\"{cleaned_album}\""),
                    ]
                };
                for query in query_bundle {
                    self.append_titles_for_query(
                        &query,
                        &mut titles,
                        &mut seen_titles,
                        &mut search_had_error,
                    );
                }
                if !compact_album.is_empty()
                    && compact_album != normalized_album
                    && compact_album != cleaned_album.to_ascii_lowercase()
                {
                    self.append_titles_for_query(
                        &compact_album,
                        &mut titles,
                        &mut seen_titles,
                        &mut search_had_error,
                    );
                }
            }
        }

        if titles.is_empty() && search_had_error {
            return Err("Wikipedia candidate search failed".to_string());
        }
        if titles.len() > MAX_CANDIDATES * 4 {
            titles.truncate(MAX_CANDIDATES * 4);
        }
        Ok(titles)
    }

    fn lookup_budget_for_priority(priority: LibraryEnrichmentPriority) -> Duration {
        match priority {
            LibraryEnrichmentPriority::Interactive => INTERACTIVE_FETCH_BUDGET,
            LibraryEnrichmentPriority::Prefetch => PREFETCH_FETCH_BUDGET,
        }
    }

    fn effective_priority_for_entity(
        &self,
        entity: &LibraryEnrichmentEntity,
        default_priority: LibraryEnrichmentPriority,
    ) -> LibraryEnrichmentPriority {
        self.in_flight_priorities
            .get(entity)
            .copied()
            .unwrap_or(default_priority)
    }

    fn budget_exceeded(
        &self,
        entity: &LibraryEnrichmentEntity,
        start: Instant,
        default_priority: LibraryEnrichmentPriority,
    ) -> bool {
        let effective_priority = self.effective_priority_for_entity(entity, default_priority);
        start.elapsed() >= Self::lookup_budget_for_priority(effective_priority)
    }

    fn truncate_blurb(value: &str, max_chars: usize) -> String {
        let compact = value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if compact.chars().count() <= max_chars {
            return compact;
        }
        compact.chars().take(max_chars).collect::<String>() + "…"
    }

    #[allow(clippy::too_many_arguments)]
    fn build_outcome(
        entity: &LibraryEnrichmentEntity,
        status: LibraryEnrichmentStatus,
        blurb: String,
        image_path: Option<PathBuf>,
        image_url: Option<String>,
        source: EnrichmentSource,
        source_url: String,
        last_error: Option<String>,
    ) -> FetchOutcome {
        FetchOutcome {
            payload: LibraryEnrichmentPayload {
                entity: entity.clone(),
                status,
                blurb,
                image_path,
                source_name: source.source_name().to_string(),
                source_url,
            },
            image_url,
            last_error,
        }
    }

    fn drain_bus_messages_nonblocking(&mut self) {
        loop {
            match self.bus_consumer.try_recv() {
                Ok(message) => self.handle_bus_message(message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Closed) => break,
            }
        }
    }

    fn fetch_wikipedia_outcome_for_entity(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> FetchOutcome {
        let titles = match self.candidate_titles_for_entity(entity) {
            Ok(titles) => titles,
            Err(error) => {
                return Self::build_outcome(
                    entity,
                    LibraryEnrichmentStatus::Error,
                    String::new(),
                    None,
                    None,
                    EnrichmentSource::Wikipedia,
                    String::new(),
                    Some(error),
                );
            }
        };

        if titles.is_empty() {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::Wikipedia,
                String::new(),
                None,
            );
        }

        let mut best_summary: Option<WikiSummary> = None;
        let mut best_score = -10_001;
        let min_threshold = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 92,
            LibraryEnrichmentEntity::Album { .. } => 88,
        };
        let early_exit_threshold = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 165,
            LibraryEnrichmentEntity::Album { .. } => 155,
        };

        for title in titles.into_iter().take(MAX_SUMMARY_FETCHES) {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                return Self::build_outcome(
                    entity,
                    LibraryEnrichmentStatus::Error,
                    String::new(),
                    None,
                    None,
                    EnrichmentSource::Wikipedia,
                    String::new(),
                    Some("Lookup budget exceeded".to_string()),
                );
            }

            let Ok(summary) = self.fetch_wikipedia_summary(&title) else {
                continue;
            };
            let score = match entity {
                LibraryEnrichmentEntity::Artist { artist } => {
                    Self::score_artist_summary(artist, &summary)
                }
                LibraryEnrichmentEntity::Album {
                    album,
                    album_artist,
                } => Self::score_album_summary(album, album_artist, &summary),
            };
            if score > best_score {
                best_score = score;
                best_summary = Some(summary);
                if best_score >= early_exit_threshold {
                    break;
                }
            }
        }

        let Some(best_summary) = best_summary else {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::Wikipedia,
                String::new(),
                None,
            );
        };

        if best_score < min_threshold {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::Wikipedia,
                best_summary.canonical_url,
                None,
            );
        }

        let blurb = Self::truncate_blurb(&best_summary.extract, MAX_BLURB_CHARS);
        if blurb.trim().is_empty() {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::Wikipedia,
                best_summary.canonical_url,
                None,
            );
        }

        let image_path = match entity {
            LibraryEnrichmentEntity::Artist { .. } => best_summary
                .image_url
                .as_ref()
                .and_then(|url| self.download_artist_image(entity, url)),
            LibraryEnrichmentEntity::Album { .. } => None,
        };

        Self::build_outcome(
            entity,
            LibraryEnrichmentStatus::Ready,
            blurb,
            image_path,
            best_summary.image_url.clone(),
            EnrichmentSource::Wikipedia,
            best_summary.canonical_url,
            None,
        )
    }

    fn search_wikidata_entities(&self, query: &str) -> Result<Vec<WikidataCandidate>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}?action=wbsearchentities&search={}&language=en&uselang=en&type=item&\
             limit={}&format=json",
            WIKIDATA_ACTION_API_URL, encoded_query, MAX_CANDIDATES
        );
        let parsed = self.http_get_json(&url)?;
        let mut out = Vec::new();
        if let Some(items) = parsed["search"].as_array() {
            for item in items {
                let id = item["id"].as_str().unwrap_or_default().trim().to_string();
                let label = item["label"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if id.is_empty() || label.is_empty() {
                    continue;
                }
                let description = item["description"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let concept_url = item["concepturi"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                out.push(WikidataCandidate {
                    id,
                    label,
                    description,
                    concept_url,
                });
            }
        }
        Ok(out)
    }

    fn fetch_wikidata_entity_details(&self, entity_id: &str) -> Result<Value, String> {
        let encoded_id = urlencoding::encode(entity_id);
        let url = format!(
            "{}?action=wbgetentities&ids={}&props=labels|descriptions|sitelinks&\
             languages=en&sitefilter=enwiki&format=json",
            WIKIDATA_ACTION_API_URL, encoded_id
        );
        self.http_get_json(&url)
    }

    fn score_wikidata_candidate(
        entity: &LibraryEnrichmentEntity,
        candidate: &WikidataCandidate,
    ) -> i32 {
        let normalized_label = Self::normalize_text(&candidate.label);
        let normalized_description = Self::normalize_text(&candidate.description);
        let compact_label = Self::compact_text(&normalized_label);

        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(artist));
                let compact_artist = Self::compact_text(&normalized_artist);
                if normalized_artist.is_empty() {
                    return -10_000;
                }

                let mut score = 0;
                if normalized_label == normalized_artist
                    || (!compact_artist.is_empty() && compact_artist == compact_label)
                {
                    score += 120;
                } else if normalized_label.starts_with(&normalized_artist)
                    || normalized_artist.starts_with(&normalized_label)
                {
                    score += 70;
                } else if normalized_label.contains(&normalized_artist) {
                    score += 45;
                }

                if Self::contains_any(&normalized_description, Self::music_artist_keywords()) {
                    score += 30;
                }

                let overlap = Self::word_overlap_ratio(&normalized_artist, &normalized_label);
                score += (overlap * 35.0).round() as i32;

                if normalized_artist.split_whitespace().count() == 1 {
                    if !Self::contains_any(&normalized_description, Self::music_artist_keywords()) {
                        return -2_000;
                    }
                    if score < 120 {
                        return -2_000;
                    }
                }
                score
            }
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => {
                let normalized_album = Self::normalize_text(&Self::collapse_whitespace(album));
                let compact_album = Self::compact_text(&normalized_album);
                let normalized_artist =
                    Self::normalize_text(&Self::collapse_whitespace(album_artist));
                if normalized_album.is_empty() {
                    return -10_000;
                }

                let mut score = 0;
                if normalized_label == normalized_album
                    || (!compact_album.is_empty() && compact_album == compact_label)
                {
                    score += 110;
                } else if normalized_label.starts_with(&normalized_album)
                    || normalized_album.starts_with(&normalized_label)
                {
                    score += 60;
                } else if normalized_label.contains(&normalized_album) {
                    score += 35;
                }
                if Self::contains_any(&normalized_description, Self::album_keywords()) {
                    score += 26;
                }
                if !normalized_artist.is_empty()
                    && normalized_description.contains(&normalized_artist)
                {
                    score += 20;
                }
                score
            }
        }
    }

    fn wikidata_description_from_details(details: &Value, entity_id: &str) -> Option<String> {
        details["entities"][entity_id]["descriptions"]["en"]["value"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    fn wikidata_enwiki_title(details: &Value, entity_id: &str) -> Option<String> {
        details["entities"][entity_id]["sitelinks"]["enwiki"]["title"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    fn fetch_wikidata_fallback(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> Option<FetchOutcome> {
        let mut query_candidates = Vec::new();
        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                let cleaned_artist = Self::collapse_whitespace(artist);
                let compact_artist = Self::compact_text(&cleaned_artist);
                query_candidates.push(cleaned_artist.clone());
                query_candidates.push(Self::title_case_words(&cleaned_artist));
                if !compact_artist.is_empty() {
                    query_candidates.push(compact_artist);
                }
            }
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => {
                let cleaned_album = Self::collapse_whitespace(album);
                let cleaned_artist = Self::collapse_whitespace(album_artist);
                let compact_album = Self::compact_text(&cleaned_album);
                query_candidates.push(cleaned_album.clone());
                query_candidates.push(format!("{cleaned_album} album"));
                if !cleaned_artist.is_empty() {
                    query_candidates.push(format!("{cleaned_album} {cleaned_artist} album"));
                }
                if !compact_album.is_empty() {
                    query_candidates.push(compact_album);
                }
            }
        }

        let mut seen_ids = HashSet::new();
        let mut best_candidate: Option<WikidataCandidate> = None;
        let mut best_score = -10_001;

        for query in query_candidates {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                break;
            }
            let Ok(candidates) = self.search_wikidata_entities(&query) else {
                continue;
            };
            for candidate in candidates {
                if !seen_ids.insert(candidate.id.clone()) {
                    continue;
                }
                let score = Self::score_wikidata_candidate(entity, &candidate);
                if score > best_score {
                    best_score = score;
                    best_candidate = Some(candidate);
                }
            }
        }

        let best = best_candidate?;
        let min_score = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 120,
            LibraryEnrichmentEntity::Album { .. } => 110,
        };
        if best_score < min_score {
            return None;
        }

        self.drain_bus_messages_nonblocking();
        if self.budget_exceeded(entity, start, initial_priority) {
            return None;
        }

        let details = self.fetch_wikidata_entity_details(&best.id).ok()?;
        if let Some(enwiki_title) = Self::wikidata_enwiki_title(&details, &best.id) {
            let summary = self.fetch_wikipedia_summary(&enwiki_title).ok()?;
            let score = match entity {
                LibraryEnrichmentEntity::Artist { artist } => {
                    Self::score_artist_summary(artist, &summary)
                }
                LibraryEnrichmentEntity::Album {
                    album,
                    album_artist,
                } => Self::score_album_summary(album, album_artist, &summary),
            };
            let threshold = match entity {
                LibraryEnrichmentEntity::Artist { .. } => 90,
                LibraryEnrichmentEntity::Album { .. } => 85,
            };
            if score >= threshold {
                let blurb = Self::truncate_blurb(&summary.extract, MAX_BLURB_CHARS);
                if !blurb.trim().is_empty() {
                    let image_path = match entity {
                        LibraryEnrichmentEntity::Artist { .. } => summary
                            .image_url
                            .as_ref()
                            .and_then(|url| self.download_artist_image(entity, url)),
                        LibraryEnrichmentEntity::Album { .. } => None,
                    };
                    return Some(Self::build_outcome(
                        entity,
                        LibraryEnrichmentStatus::Ready,
                        blurb,
                        image_path,
                        summary.image_url,
                        EnrichmentSource::Wikipedia,
                        summary.canonical_url,
                        None,
                    ));
                }
            }
        }

        if let LibraryEnrichmentEntity::Artist { .. } = entity {
            let description = Self::wikidata_description_from_details(&details, &best.id)
                .or_else(|| (!best.description.is_empty()).then_some(best.description.clone()))?;
            let normalized_description = Self::normalize_text(&description);
            if !Self::contains_any(&normalized_description, Self::music_artist_keywords()) {
                return None;
            }
            if best_score < 145 {
                return None;
            }
            return Some(Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::Ready,
                Self::truncate_blurb(&description, MAX_BLURB_CHARS),
                None,
                None,
                EnrichmentSource::Wikidata,
                if best.concept_url.is_empty() {
                    format!("https://www.wikidata.org/wiki/{}", best.id)
                } else {
                    best.concept_url
                },
                None,
            ));
        }

        None
    }

    fn fetch_outcome_for_entity(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
    ) -> FetchOutcome {
        let started_at = Instant::now();
        let wiki_outcome = self.fetch_wikipedia_outcome_for_entity(entity, priority, started_at);
        if wiki_outcome.payload.status == LibraryEnrichmentStatus::Ready {
            return wiki_outcome;
        }
        if let Some(wikidata_outcome) = self.fetch_wikidata_fallback(entity, priority, started_at) {
            return wikidata_outcome;
        }
        wiki_outcome
    }

    fn image_cache_dir() -> Option<PathBuf> {
        let cache_dir = dirs::cache_dir()?
            .join("roqtune")
            .join(IMAGE_CACHE_DIR_NAME);
        if !cache_dir.exists() {
            fs::create_dir_all(&cache_dir).ok()?;
        }
        Some(cache_dir)
    }

    fn detect_image_extension(bytes: &[u8]) -> Option<&'static str> {
        if bytes.len() >= 8
            && bytes[0] == 0x89
            && bytes[1] == b'P'
            && bytes[2] == b'N'
            && bytes[3] == b'G'
            && bytes[4] == 0x0D
            && bytes[5] == 0x0A
            && bytes[6] == 0x1A
            && bytes[7] == 0x0A
        {
            return Some("png");
        }
        if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
            return Some("jpg");
        }
        if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
            return Some("webp");
        }
        if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
            return Some("gif");
        }
        if bytes.len() >= 2 && bytes[0] == b'B' && bytes[1] == b'M' {
            return Some("bmp");
        }
        None
    }

    fn file_looks_like_supported_image(path: &Path) -> bool {
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(_) => return false,
        };
        let mut header = [0u8; 16];
        let read = match file.read(&mut header) {
            Ok(read) => read,
            Err(_) => return false,
        };
        if read == 0 {
            return false;
        }
        Self::detect_image_extension(&header[..read]).is_some()
    }

    fn download_artist_image(
        &self,
        entity: &LibraryEnrichmentEntity,
        image_url: &str,
    ) -> Option<PathBuf> {
        let cache_dir = Self::image_cache_dir()?;
        let response = self
            .http_client
            .get(image_url)
            .set("User-Agent", WIKIMEDIA_USER_AGENT)
            .call()
            .ok()?;
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).ok()?;
        if bytes.is_empty() {
            return None;
        }

        let extension = Self::detect_image_extension(&bytes)?;
        let mut hasher = DefaultHasher::new();
        Self::source_entity_label(entity).hash(&mut hasher);
        image_url.hash(&mut hasher);
        let hash = hasher.finish();
        let image_path = cache_dir.join(format!("{hash:x}.{extension}"));
        if image_path.exists() {
            if Self::file_looks_like_supported_image(&image_path) {
                return Some(image_path);
            }
            let _ = fs::remove_file(&image_path);
        }

        let temp_path = cache_dir.join(format!("{hash:x}.{extension}.tmp"));
        if fs::write(&temp_path, &bytes).is_err() {
            return None;
        }
        if !Self::file_looks_like_supported_image(&temp_path) {
            let _ = fs::remove_file(&temp_path);
            return None;
        }
        if fs::rename(&temp_path, &image_path).is_err() {
            let _ = fs::remove_file(&temp_path);
            return None;
        }
        Some(image_path)
    }

    fn ttl_days_for_payload(&self, payload: &LibraryEnrichmentPayload) -> u32 {
        let ready_ttl_days = if matches!(payload.entity, LibraryEnrichmentEntity::Artist { .. }) {
            self.artist_image_cache_ttl_days.max(1)
        } else {
            READY_METADATA_TTL_DAYS
        };
        match payload.status {
            LibraryEnrichmentStatus::Ready => ready_ttl_days,
            LibraryEnrichmentStatus::NotFound => NOT_FOUND_TTL_DAYS,
            LibraryEnrichmentStatus::Disabled => 1,
            LibraryEnrichmentStatus::Error => ERROR_TTL_DAYS,
        }
    }

    fn emit_enrichment_result(&self, payload: LibraryEnrichmentPayload) {
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::EnrichmentResult(payload)));
    }

    fn prune_enrichment_cache(&self) {
        if let Err(error) = self
            .db_manager
            .prune_expired_library_enrichment_cache(Self::now_unix_ms())
        {
            warn!("Failed to prune enrichment cache rows: {}", error);
        }
    }

    fn prune_artist_image_cache_by_size(&self) {
        let Some(cache_dir) = Self::image_cache_dir() else {
            return;
        };

        let mut files = Vec::new();
        let mut total_size_bytes: u64 = 0;
        let read_dir = match fs::read_dir(&cache_dir) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                warn!(
                    "Failed to read enrichment image cache directory {}: {}",
                    cache_dir.display(),
                    error
                );
                return;
            }
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let size = metadata.len();
            total_size_bytes = total_size_bytes.saturating_add(size);
            let modified = metadata
                .modified()
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            files.push((path, size, modified));
        }

        let max_size_bytes =
            u64::from(self.artist_image_cache_max_size_mb.max(1)) * 1024u64 * 1024u64;
        if total_size_bytes <= max_size_bytes {
            return;
        }

        files.sort_by_key(|(_, _, modified)| *modified);
        for (path, size, _) in files {
            if total_size_bytes <= max_size_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total_size_bytes = total_size_bytes.saturating_sub(size);
                if let Err(error) = self
                    .db_manager
                    .clear_library_enrichment_image_path(path.to_string_lossy().as_ref())
                {
                    warn!(
                        "Failed to clear enrichment image reference after deleting {}: {}",
                        path.display(),
                        error
                    );
                }
            }
        }
    }

    fn clear_artist_image_cache_files(&self) -> usize {
        let Some(cache_dir) = Self::image_cache_dir() else {
            return 0;
        };
        let mut deleted = 0usize;
        let Ok(entries) = fs::read_dir(&cache_dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && fs::remove_file(&path).is_ok() {
                deleted = deleted.saturating_add(1);
            }
        }
        deleted
    }

    fn clear_enrichment_cache(&mut self) {
        let cleared_rows = match self.db_manager.clear_library_enrichment_cache() {
            Ok(count) => count,
            Err(error) => {
                warn!("Failed to clear enrichment DB cache rows: {}", error);
                0
            }
        };
        let deleted_images = self.clear_artist_image_cache_files();
        self.queued_priorities.clear();
        self.queued_interactive.clear();
        self.queued_prefetch.clear();
        self.in_flight_priorities.clear();
        let _ = self
            .bus_producer
            .send(Message::Library(LibraryMessage::EnrichmentCacheCleared {
                cleared_rows,
                deleted_images,
            }));
    }

    fn enqueue_enrichment_request(
        &mut self,
        entity: LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
    ) {
        if let Some(in_flight_priority) = self.in_flight_priorities.get_mut(&entity) {
            if *in_flight_priority == LibraryEnrichmentPriority::Prefetch
                && priority == LibraryEnrichmentPriority::Interactive
            {
                *in_flight_priority = LibraryEnrichmentPriority::Interactive;
            }
            return;
        }

        if let Some(existing_priority) = self.queued_priorities.get(&entity).copied() {
            if existing_priority == LibraryEnrichmentPriority::Prefetch
                && priority == LibraryEnrichmentPriority::Interactive
            {
                self.queued_priorities
                    .insert(entity.clone(), LibraryEnrichmentPriority::Interactive);
                self.queued_interactive.push_back(entity);
            }
            return;
        }

        if priority == LibraryEnrichmentPriority::Prefetch
            && self.queued_prefetch.len() >= MAX_PENDING_PREFETCH_REQUESTS
        {
            return;
        }

        self.queued_priorities.insert(entity.clone(), priority);
        match priority {
            LibraryEnrichmentPriority::Interactive => self.queued_interactive.push_back(entity),
            LibraryEnrichmentPriority::Prefetch => self.queued_prefetch.push_back(entity),
        }
    }

    fn dequeue_enrichment_request(
        &mut self,
    ) -> Option<(LibraryEnrichmentEntity, LibraryEnrichmentPriority)> {
        while let Some(entity) = self.queued_interactive.pop_front() {
            match self.queued_priorities.get(&entity).copied() {
                Some(LibraryEnrichmentPriority::Interactive) => {
                    self.queued_priorities.remove(&entity);
                    return Some((entity, LibraryEnrichmentPriority::Interactive));
                }
                Some(LibraryEnrichmentPriority::Prefetch) | None => {}
            }
        }

        while let Some(entity) = self.queued_prefetch.pop_front() {
            let Some(priority) = self.queued_priorities.remove(&entity) else {
                continue;
            };
            return Some((entity, priority));
        }
        None
    }

    fn handle_bus_message(&mut self, message: Message) {
        match message {
            Message::Config(crate::protocol::ConfigMessage::ConfigChanged(config)) => {
                self.online_metadata_enabled = config.library.online_metadata_enabled;
                self.artist_image_cache_ttl_days = config.library.artist_image_cache_ttl_days;
                self.artist_image_cache_max_size_mb = config.library.artist_image_cache_max_size_mb;
                self.prune_enrichment_cache();
                self.prune_artist_image_cache_by_size();
            }
            Message::Library(LibraryMessage::RequestEnrichment { entity, priority }) => {
                debug!(
                    "Enrichment request queued for {} ({:?})",
                    Self::source_entity_label(&entity),
                    priority
                );
                self.enqueue_enrichment_request(entity, priority);
            }
            Message::Library(LibraryMessage::ClearEnrichmentCache) => {
                self.clear_enrichment_cache();
            }
            _ => {}
        }
    }

    fn handle_enrichment_request(
        &mut self,
        entity: LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
    ) {
        if !self.online_metadata_enabled {
            if priority == LibraryEnrichmentPriority::Interactive {
                self.emit_enrichment_result(LibraryEnrichmentPayload {
                    entity,
                    status: LibraryEnrichmentStatus::Disabled,
                    blurb: String::new(),
                    image_path: None,
                    source_name: WIKIPEDIA_SOURCE_NAME.to_string(),
                    source_url: String::new(),
                });
            }
            return;
        }

        let now_unix_ms = Self::now_unix_ms();
        self.prune_enrichment_cache();
        match self
            .db_manager
            .get_library_enrichment_cache(&entity, now_unix_ms)
        {
            Ok(Some(mut cached)) => {
                if let Some(path) = cached.image_path.as_ref() {
                    if !path.exists() || !Self::file_looks_like_supported_image(path) {
                        if path.exists() {
                            let _ = fs::remove_file(path);
                        }
                        if let Err(error) = self
                            .db_manager
                            .clear_library_enrichment_image_path(path.to_string_lossy().as_ref())
                        {
                            warn!(
                                "Failed clearing stale enrichment image path {}: {}",
                                path.display(),
                                error
                            );
                        }
                        cached.image_path = None;
                    }
                }
                self.emit_enrichment_result(cached);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    "Failed to read enrichment cache for {}: {}",
                    Self::source_entity_label(&entity),
                    error
                );
            }
        }

        self.in_flight_priorities.insert(entity.clone(), priority);
        let outcome = self.fetch_outcome_for_entity(&entity, priority);
        self.in_flight_priorities.remove(&entity);

        let ttl_days = self.ttl_days_for_payload(&outcome.payload);
        let expires_unix_ms = now_unix_ms.saturating_add(i64::from(ttl_days) * 24 * 60 * 60 * 1000);
        if let Err(error) = self.db_manager.upsert_library_enrichment_cache(
            &outcome.payload,
            outcome.image_url.as_deref(),
            now_unix_ms,
            expires_unix_ms,
            outcome.last_error.as_deref(),
        ) {
            warn!(
                "Failed to persist enrichment cache for {}: {}",
                Self::source_entity_label(&entity),
                error
            );
        }
        self.prune_artist_image_cache_by_size();
        self.emit_enrichment_result(outcome.payload);
    }

    /// Starts the blocking event loop for enrichment requests.
    pub fn run(&mut self) {
        loop {
            loop {
                match self.bus_consumer.try_recv() {
                    Ok(message) => self.handle_bus_message(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => {}
                    Err(TryRecvError::Closed) => return,
                }
            }

            if let Some((entity, priority)) = self.dequeue_enrichment_request() {
                debug!(
                    "Enrichment request processing for {} ({:?})",
                    Self::source_entity_label(&entity),
                    priority
                );
                self.handle_enrichment_request(entity, priority);
                continue;
            }

            match self.bus_consumer.blocking_recv() {
                Ok(message) => self.handle_bus_message(message),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LibraryEnrichmentManager, WikiSummary, WikidataCandidate};

    fn sample_summary(title: &str, description: &str, extract: &str) -> WikiSummary {
        WikiSummary {
            title: title.to_string(),
            description: description.to_string(),
            extract: extract.to_string(),
            canonical_url: String::new(),
            image_url: None,
            page_type: String::new(),
        }
    }

    #[test]
    fn test_normalize_text_collapses_symbols_and_case() {
        assert_eq!(
            LibraryEnrichmentManager::normalize_text("Sample-Name / RANDOM Value!!"),
            "sample name random value"
        );
    }

    #[test]
    fn test_compact_text_removes_spacing_and_punctuation() {
        assert_eq!(
            LibraryEnrichmentManager::compact_text("Sample Group!"),
            "samplegroup"
        );
    }

    #[test]
    fn test_score_artist_summary_prefers_music_entity_match() {
        let summary = sample_summary(
            "Sample Ensemble",
            "Electronic music duo",
            "Sample Ensemble are an electronic music duo.",
        );
        assert!(LibraryEnrichmentManager::score_artist_summary("Sample Ensemble", &summary) >= 120);
    }

    #[test]
    fn test_score_artist_summary_accepts_compact_name_match() {
        let summary = sample_summary(
            "Sample Group (South Korean band)",
            "South Korean band",
            "Sample Group is a South Korean pop band.",
        );
        assert!(LibraryEnrichmentManager::score_artist_summary("SAMPLEGROUP", &summary) >= 100);
    }

    #[test]
    fn test_score_artist_summary_rejects_discography_pages() {
        let summary = sample_summary(
            "Sample Ensemble discography",
            "Discography of Sample Ensemble",
            "The discography of Sample Ensemble includes several albums.",
        );
        assert!(LibraryEnrichmentManager::score_artist_summary("Sample Ensemble", &summary) < 0);
    }

    #[test]
    fn test_score_album_summary_rejects_artist_bio_page() {
        let summary = sample_summary(
            "Sample Ensemble",
            "Electronic music duo",
            "Sample Ensemble are an electronic music duo.",
        );
        assert!(
            LibraryEnrichmentManager::score_album_summary(
                "Test Record",
                "Sample Ensemble",
                &summary
            ) < 0
        );
    }

    #[test]
    fn test_score_artist_summary_rejects_self_titled_album_page() {
        let summary = sample_summary(
            "Sample Ensemble",
            "Debut studio album by Sample Ensemble",
            "Sample Ensemble is the debut studio album by Sample Ensemble.",
        );
        assert!(LibraryEnrichmentManager::score_artist_summary("Sample Ensemble", &summary) < 0);
    }

    #[test]
    fn test_score_album_summary_rewards_album_context() {
        let summary = sample_summary(
            "Test Record",
            "Studio album by Sample Ensemble",
            "Test Record is a studio album by Sample Ensemble.",
        );
        assert!(
            LibraryEnrichmentManager::score_album_summary(
                "Test Record",
                "Sample Ensemble",
                &summary
            ) >= 110
        );
    }

    #[test]
    fn test_score_wikidata_candidate_rejects_non_music_single_token_artist() {
        let candidate = WikidataCandidate {
            id: "Q1".to_string(),
            label: "Sample".to_string(),
            description: "mythological figure".to_string(),
            concept_url: String::new(),
        };
        let score = LibraryEnrichmentManager::score_wikidata_candidate(
            &crate::protocol::LibraryEnrichmentEntity::Artist {
                artist: "Sample".to_string(),
            },
            &candidate,
        );
        assert!(score < 0);
    }

    #[test]
    fn test_detect_image_extension_recognizes_png_and_jpeg() {
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let jpg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(
            LibraryEnrichmentManager::detect_image_extension(&png),
            Some("png")
        );
        assert_eq!(
            LibraryEnrichmentManager::detect_image_extension(&jpg),
            Some("jpg")
        );
    }
}
