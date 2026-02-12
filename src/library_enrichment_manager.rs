//! Online library enrichment runtime component.
//!
//! This manager fetches display-only artist/album blurbs (and artist images)
//! from online metadata sources, then stores short-lived cache records for the UI.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, info, warn};
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
const THEAUDIODB_BASE_URL: &str = "https://www.theaudiodb.com/api/v1/json/2";
const WIKIPEDIA_SOURCE_NAME: &str = "Wikipedia";
const THEAUDIODB_SOURCE_NAME: &str = "TheAudioDB";
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
const THEAUDIODB_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const THEAUDIODB_RATE_LIMIT_MAX_REQUESTS: usize = 30;
const THEAUDIODB_RATE_LIMIT_PREFETCH_BUDGET: usize = 20;
const THEAUDIODB_RATE_LIMIT_DEFERRED: &str = "TheAudioDB rate limit saturated";
const RETRY_REASON_TIMEOUT_PREFIX: &str = "timeout:";
const RETRY_REASON_HARD_PREFIX: &str = "hard:";
const RETRY_REASON_RATE_LIMIT_PREFIX: &str = "rate_limit:";
const RETRY_REASON_NOT_FOUND_PREFIX: &str = "not_found:";
const RETRYABLE_TIMEOUT_BLURB: &str = "Internet metadata request timed out.";
const RETRYABLE_RATE_LIMIT_BLURB: &str = "Internet metadata request hit a provider rate limit.";
const HARD_ERROR_BLURB: &str = "Internet metadata provider returned an error.";
const WIKIMEDIA_USER_AGENT: &str =
    "roqtune/0.1.0 (https://github.com/roqtune/roqtune; contact: metadata enrichment)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnrichmentSource {
    TheAudioDB,
    Wikipedia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TitleMatchTier {
    None,
    Fuzzy,
    NearExact,
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpFailureKind {
    Timeout,
    Hard,
}

#[derive(Debug, Clone)]
struct RankedSummaryCandidate {
    requested_title: String,
    summary: WikiSummary,
    deterministic_score: i32,
}

#[derive(Debug, Clone)]
struct AudioDbArtistCandidate {
    name: String,
    biography: String,
    image_url: Option<String>,
    source_url: String,
    genre: String,
}

#[derive(Debug, Clone)]
struct AudioDbAlbumCandidate {
    album: String,
    artist: String,
    description: String,
    source_url: String,
}

impl EnrichmentSource {
    fn source_name(self) -> &'static str {
        match self {
            Self::TheAudioDB => THEAUDIODB_SOURCE_NAME,
            Self::Wikipedia => WIKIPEDIA_SOURCE_NAME,
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
    wikidata_item_id: Option<String>,
}

#[derive(Debug, Clone)]
struct WikidataCandidate {
    id: String,
    label: String,
    description: String,
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
    wikidata_alias_cache: HashMap<String, HashSet<String>>,
    audiodb_request_timestamps: VecDeque<Instant>,
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
            wikidata_alias_cache: HashMap::new(),
            audiodb_request_timestamps: VecDeque::new(),
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

    fn timeout_retry_policy(priority: LibraryEnrichmentPriority) -> (u32, Duration) {
        match priority {
            LibraryEnrichmentPriority::Interactive => (3, Duration::from_millis(320)),
            LibraryEnrichmentPriority::Prefetch => (2, Duration::from_millis(650)),
        }
    }

    fn timeout_backoff_delay(base_delay: Duration, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(6);
        let multiplier = 1u32 << exponent;
        base_delay
            .checked_mul(multiplier)
            .unwrap_or(Duration::from_secs(4))
            .min(Duration::from_secs(4))
    }

    fn build_timeout_reason(message: impl Into<String>) -> String {
        format!("{RETRY_REASON_TIMEOUT_PREFIX}{}", message.into())
    }

    fn build_hard_reason(message: impl Into<String>) -> String {
        format!("{RETRY_REASON_HARD_PREFIX}{}", message.into())
    }

    fn build_rate_limit_reason(message: impl Into<String>) -> String {
        format!("{RETRY_REASON_RATE_LIMIT_PREFIX}{}", message.into())
    }

    fn build_not_found_reason(message: impl Into<String>) -> String {
        format!("{RETRY_REASON_NOT_FOUND_PREFIX}{}", message.into())
    }

    fn is_timeout_reason(reason: &str) -> bool {
        reason.starts_with(RETRY_REASON_TIMEOUT_PREFIX)
    }

    fn is_hard_reason(reason: &str) -> bool {
        reason.starts_with(RETRY_REASON_HARD_PREFIX)
    }

    fn is_rate_limit_reason(reason: &str) -> bool {
        reason.starts_with(RETRY_REASON_RATE_LIMIT_PREFIX)
    }

    fn classify_ureq_failure(error: &ureq::Error) -> HttpFailureKind {
        match error {
            ureq::Error::Status(code, _) => match code {
                408 | 429 | 500 | 502 | 503 | 504 => HttpFailureKind::Timeout,
                _ => HttpFailureKind::Hard,
            },
            ureq::Error::Transport(transport) => {
                let lowered = transport.to_string().to_ascii_lowercase();
                if lowered.contains("timed out") || lowered.contains("timeout") {
                    HttpFailureKind::Timeout
                } else {
                    HttpFailureKind::Hard
                }
            }
        }
    }

    fn classify_io_timeout(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) || error.to_string().to_ascii_lowercase().contains("timed out")
    }

    fn execute_with_timeout_backoff<T, F>(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
        label: &str,
        mut operation: F,
    ) -> Result<T, String>
    where
        F: FnMut(&mut Self) -> Result<T, String>,
    {
        let (max_attempts, base_delay) = Self::timeout_retry_policy(priority);
        let mut attempt = 1u32;
        loop {
            match operation(self) {
                Ok(value) => return Ok(value),
                Err(error_reason)
                    if Self::is_timeout_reason(&error_reason) && attempt < max_attempts =>
                {
                    let backoff = Self::timeout_backoff_delay(base_delay, attempt);
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: {} attempt {} timed out, retrying in {:?}",
                            Self::source_entity_label(entity),
                            label,
                            attempt,
                            backoff
                        );
                    }
                    std::thread::sleep(backoff);
                    self.drain_bus_messages_nonblocking();
                    attempt = attempt.saturating_add(1);
                }
                Err(error_reason) => return Err(error_reason),
            }
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

    fn normalized_text_contains_phrase(haystack: &str, phrase: &str) -> bool {
        if haystack.is_empty() || phrase.is_empty() {
            return false;
        }
        let mut padded_haystack = String::with_capacity(haystack.len() + 2);
        padded_haystack.push(' ');
        padded_haystack.push_str(haystack);
        padded_haystack.push(' ');

        let mut padded_phrase = String::with_capacity(phrase.len() + 2);
        padded_phrase.push(' ');
        padded_phrase.push_str(phrase);
        padded_phrase.push(' ');
        padded_haystack.contains(&padded_phrase)
    }

    fn artist_alias_mentioned_in_summary(artist: &str, summary: &WikiSummary) -> bool {
        let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(artist));
        if normalized_artist.is_empty() {
            return false;
        }
        let compact_artist = Self::compact_text(&normalized_artist);
        for text in [&summary.title, &summary.description, &summary.extract] {
            let normalized = Self::normalize_text(text);
            if Self::normalized_text_contains_phrase(&normalized, &normalized_artist) {
                return true;
            }
            if !compact_artist.is_empty() {
                let compact = Self::compact_text(&normalized);
                if compact.contains(&compact_artist) {
                    return true;
                }
            }
        }
        false
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

    fn normalized_title_head(title: &str) -> String {
        let head = title.split('(').next().unwrap_or(title).trim();
        Self::normalize_text(head)
    }

    fn title_match_tier(target: &str, title: &str) -> TitleMatchTier {
        let normalized_target = Self::normalize_text(&Self::collapse_whitespace(target));
        let normalized_title = Self::normalize_text(title);
        if normalized_target.is_empty() || normalized_title.is_empty() {
            return TitleMatchTier::None;
        }

        let normalized_title_head = Self::normalized_title_head(title);
        let compact_target = Self::compact_text(&normalized_target);
        let compact_title = Self::compact_text(&normalized_title);
        let compact_title_head = Self::compact_text(&normalized_title_head);

        if normalized_title == normalized_target
            || normalized_title_head == normalized_target
            || (!compact_target.is_empty()
                && (compact_title == compact_target || compact_title_head == compact_target))
        {
            return TitleMatchTier::Exact;
        }

        if normalized_title.starts_with(&normalized_target)
            || normalized_title_head.starts_with(&normalized_target)
            || (!compact_target.is_empty()
                && (compact_title.starts_with(&compact_target)
                    || compact_title_head.starts_with(&compact_target)))
        {
            return TitleMatchTier::NearExact;
        }

        let overlap = Self::word_overlap_ratio(&normalized_target, &normalized_title);
        if overlap >= 0.6 {
            TitleMatchTier::Fuzzy
        } else {
            TitleMatchTier::None
        }
    }

    fn push_name_identity_variants(variants: &mut HashSet<String>, value: &str) {
        let normalized = Self::normalize_text(&Self::collapse_whitespace(value));
        if normalized.is_empty() {
            return;
        }
        variants.insert(normalized.clone());
        let compact = Self::compact_text(&normalized);
        if !compact.is_empty() {
            variants.insert(compact);
        }
    }

    fn name_matches_identity_variants(name: &str, variants: &HashSet<String>) -> bool {
        let normalized = Self::normalize_text(&Self::collapse_whitespace(name));
        if normalized.is_empty() {
            return false;
        }
        if variants.contains(&normalized) {
            return true;
        }
        let compact = Self::compact_text(&normalized);
        !compact.is_empty() && variants.contains(&compact)
    }

    fn artist_wikidata_alias_match_allowed(summary: &WikiSummary) -> bool {
        if Self::looks_disambiguation(summary)
            || Self::looks_discography_or_list(summary)
            || Self::looks_album_entity(summary)
        {
            return false;
        }
        Self::has_artist_person_context(summary)
            || Self::title_has_artist_disambiguator(&summary.title)
    }

    fn direct_lookup_titles_for_entity(entity: &LibraryEnrichmentEntity) -> Vec<String> {
        let mut titles = Vec::new();
        let mut seen = HashSet::new();
        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                let cleaned_artist = Self::collapse_whitespace(artist);
                let normalized_artist = Self::normalize_text(&cleaned_artist);
                let compact_artist = normalized_artist.replace(' ', "");
                let title_case_artist = Self::title_case_words(&cleaned_artist);
                Self::push_unique_title(&mut titles, &mut seen, cleaned_artist.clone());
                Self::push_unique_title(&mut titles, &mut seen, title_case_artist);
                if !compact_artist.is_empty() {
                    Self::push_unique_title(
                        &mut titles,
                        &mut seen,
                        Self::title_case_words(&compact_artist),
                    );
                }
            }
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => {
                let cleaned_album = Self::collapse_whitespace(album);
                let normalized_album = Self::normalize_text(&cleaned_album);
                let compact_album = normalized_album.replace(' ', "");
                Self::push_unique_title(&mut titles, &mut seen, cleaned_album.clone());
                Self::push_unique_title(
                    &mut titles,
                    &mut seen,
                    Self::title_case_words(&cleaned_album),
                );
                if !compact_album.is_empty() {
                    Self::push_unique_title(
                        &mut titles,
                        &mut seen,
                        Self::title_case_words(&compact_album),
                    );
                }
                if !album_artist.trim().is_empty() {
                    let cleaned_artist = Self::collapse_whitespace(album_artist);
                    Self::push_unique_title(
                        &mut titles,
                        &mut seen,
                        format!("{cleaned_album} ({cleaned_artist} album)"),
                    );
                }
                Self::push_unique_title(&mut titles, &mut seen, format!("{cleaned_album} (album)"));
            }
        }
        titles
    }

    fn direct_summary_rejection_reason(
        entity: &LibraryEnrichmentEntity,
        summary: &WikiSummary,
    ) -> Option<&'static str> {
        if Self::looks_disambiguation(summary) || Self::looks_discography_or_list(summary) {
            return Some("disambiguation_or_list");
        }

        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                if Self::looks_album_entity(summary) {
                    return Some("artist_matched_album_entity");
                }

                let tier = Self::title_match_tier(artist, &summary.title);
                let has_music_context = Self::has_artist_person_context(summary);
                let alias_mentioned = Self::artist_alias_mentioned_in_summary(artist, summary);
                if tier < TitleMatchTier::NearExact && !(alias_mentioned && has_music_context) {
                    return Some("artist_title_not_near_exact");
                }

                let has_artist_title_hint = Self::title_has_artist_disambiguator(&summary.title);
                let artist_word_count = Self::normalize_text(artist).split_whitespace().count();
                if artist_word_count == 1 && !has_music_context && !has_artist_title_hint {
                    return Some("single_token_artist_missing_music_context");
                }

                if tier == TitleMatchTier::Exact || has_music_context || has_artist_title_hint {
                    None
                } else {
                    Some("artist_missing_music_context")
                }
            }
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => {
                let tier = Self::title_match_tier(album, &summary.title);
                if tier < TitleMatchTier::NearExact {
                    return Some("album_title_not_near_exact");
                }

                let has_album_context = Self::has_album_context(summary);
                let title_parenthetical =
                    Self::title_parenthetical(&summary.title).unwrap_or_default();
                let title_has_album_hint = Self::contains_any(
                    &title_parenthetical,
                    &["album", "ep", "soundtrack", "mixtape"],
                );
                let normalized_album_artist = Self::normalize_text(album_artist);
                let mentions_artist = !normalized_album_artist.is_empty()
                    && (Self::normalize_text(&summary.extract).contains(&normalized_album_artist)
                        || Self::normalize_text(&summary.description)
                            .contains(&normalized_album_artist)
                        || Self::normalize_text(&summary.title).contains(&normalized_album_artist));

                let accepted = (tier == TitleMatchTier::Exact
                    && (has_album_context || title_has_album_hint))
                    || (tier == TitleMatchTier::NearExact
                        && (has_album_context || (title_has_album_hint && mentions_artist)));
                if accepted {
                    None
                } else {
                    Some("album_context_or_artist_hint_missing")
                }
            }
        }
    }

    #[cfg(test)]
    fn direct_summary_matches_entity(
        entity: &LibraryEnrichmentEntity,
        summary: &WikiSummary,
    ) -> bool {
        Self::direct_summary_rejection_reason(entity, summary).is_none()
    }

    fn score_artist_summary(artist: &str, summary: &WikiSummary) -> i32 {
        let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(artist));
        let normalized_title = Self::normalize_text(&summary.title);
        let normalized_description = Self::normalize_text(&summary.description);
        let normalized_extract = Self::normalize_text(&summary.extract);
        let title_match_tier = Self::title_match_tier(artist, &summary.title);

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
        let alias_mentioned = Self::artist_alias_mentioned_in_summary(artist, summary);
        if !has_music_context
            && !has_artist_title_hint
            && title_match_tier < TitleMatchTier::NearExact
        {
            return -4_800;
        }

        let artist_word_count = normalized_artist.split_whitespace().count();
        if artist_word_count == 1
            && title_match_tier < TitleMatchTier::NearExact
            && !has_artist_title_hint
            && !has_music_context
        {
            return -3_200;
        }

        let mut score = 0;
        match title_match_tier {
            TitleMatchTier::Exact => score += 180,
            TitleMatchTier::NearExact => score += 130,
            TitleMatchTier::Fuzzy => score += 60,
            TitleMatchTier::None => {
                if normalized_title.starts_with(&normalized_artist)
                    || normalized_artist.starts_with(&normalized_title)
                {
                    score += 80;
                } else if normalized_title.contains(&normalized_artist) {
                    score += 55;
                }
            }
        }

        if Self::contains_any(&normalized_description, Self::music_artist_keywords())
            || Self::contains_any(&normalized_extract, Self::music_artist_keywords())
        {
            score += 32;
        }
        if has_artist_title_hint {
            score += 20;
        }
        if alias_mentioned && has_music_context && title_match_tier < TitleMatchTier::NearExact {
            score += 105;
        }

        let overlap = Self::word_overlap_ratio(&normalized_artist, &normalized_title);
        score += (overlap * 40.0).round() as i32;

        if artist_word_count >= 2 && overlap < 0.5 && title_match_tier < TitleMatchTier::NearExact {
            score -= 30;
        }
        score
    }

    fn score_album_summary(album: &str, album_artist: &str, summary: &WikiSummary) -> i32 {
        let normalized_album = Self::normalize_text(&Self::collapse_whitespace(album));
        let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(album_artist));
        let normalized_title = Self::normalize_text(&summary.title);
        let normalized_description = Self::normalize_text(&summary.description);
        let normalized_extract = Self::normalize_text(&summary.extract);
        let title_match_tier = Self::title_match_tier(album, &summary.title);

        if normalized_album.is_empty() {
            return -10_000;
        }
        if Self::looks_disambiguation(summary) {
            return -10_000;
        }
        if Self::looks_discography_or_list(summary) {
            return -7_000;
        }
        if !Self::has_album_context(summary) && title_match_tier < TitleMatchTier::NearExact {
            return -4_800;
        }
        if Self::has_artist_person_context(summary)
            && !Self::has_album_context(summary)
            && title_match_tier < TitleMatchTier::NearExact
        {
            return -6_200;
        }

        let mut score = 0;
        match title_match_tier {
            TitleMatchTier::Exact => score += 145,
            TitleMatchTier::NearExact => score += 95,
            TitleMatchTier::Fuzzy => score += 55,
            TitleMatchTier::None => {
                if normalized_title.starts_with(&normalized_album)
                    || normalized_album.starts_with(&normalized_title)
                {
                    score += 70;
                } else if normalized_title.contains(&normalized_album) {
                    score += 45;
                }
            }
        }

        if Self::has_album_context(summary) {
            score += 35;
        }
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

    fn http_get_json_once(&self, url: &str) -> Result<Value, String> {
        let response = self
            .http_client
            .get(url)
            .set("User-Agent", WIKIMEDIA_USER_AGENT)
            .set("Accept", "application/json")
            .call()
            .map_err(|error| {
                let classified = Self::classify_ureq_failure(&error);
                let message = format!("Request failed: {error}");
                match classified {
                    HttpFailureKind::Timeout => Self::build_timeout_reason(message),
                    HttpFailureKind::Hard => Self::build_hard_reason(message),
                }
            })?;
        let mut body = String::new();
        response
            .into_reader()
            .read_to_string(&mut body)
            .map_err(|error| {
                Self::build_hard_reason(format!("Failed to read response: {error}"))
            })?;
        serde_json::from_str(&body)
            .map_err(|error| Self::build_hard_reason(format!("Invalid JSON response: {error}")))
    }

    fn http_get_json(
        &mut self,
        url: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
        label: &str,
    ) -> Result<Value, String> {
        self.execute_with_timeout_backoff(entity, priority, verbose_log, label, |manager| {
            manager.http_get_json_once(url)
        })
    }

    fn audiodb_request_url(endpoint: &str, params: &[(&str, &str)]) -> String {
        let mut url = format!("{THEAUDIODB_BASE_URL}/{endpoint}");
        if params.is_empty() {
            return url;
        }

        url.push('?');
        for (index, (key, value)) in params.iter().enumerate() {
            if index > 0 {
                url.push('&');
            }
            url.push_str(key);
            url.push('=');
            url.push_str(urlencoding::encode(value).as_ref());
        }
        url
    }

    fn wait_for_audiodb_rate_limit_slot(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        default_priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> bool {
        loop {
            let now = Instant::now();
            while self
                .audiodb_request_timestamps
                .front()
                .is_some_and(|timestamp| {
                    now.duration_since(*timestamp) >= THEAUDIODB_RATE_LIMIT_WINDOW
                })
            {
                self.audiodb_request_timestamps.pop_front();
            }

            let effective_priority = self.effective_priority_for_entity(entity, default_priority);
            let request_budget = if effective_priority == LibraryEnrichmentPriority::Prefetch {
                THEAUDIODB_RATE_LIMIT_PREFETCH_BUDGET
            } else {
                THEAUDIODB_RATE_LIMIT_MAX_REQUESTS
            };

            if self.audiodb_request_timestamps.len() < request_budget {
                self.audiodb_request_timestamps.push_back(now);
                return true;
            }

            if effective_priority == LibraryEnrichmentPriority::Prefetch {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: deferring prefetch request due to TheAudioDB rate limit saturation",
                        Self::source_entity_label(entity)
                    );
                }
                return false;
            }

            let Some(oldest) = self.audiodb_request_timestamps.front().copied() else {
                continue;
            };
            let elapsed = now.duration_since(oldest);
            let wait_duration = THEAUDIODB_RATE_LIMIT_WINDOW.saturating_sub(elapsed);
            if verbose_log {
                info!(
                    "Enrichment[{}]: waiting {:?} for TheAudioDB rate limit",
                    Self::source_entity_label(entity),
                    wait_duration
                );
            }
            let sleep_duration = wait_duration.min(Duration::from_millis(250));
            std::thread::sleep(sleep_duration);
            self.drain_bus_messages_nonblocking();
        }
    }

    fn audiodb_get_json(
        &mut self,
        endpoint: &str,
        params: &[(&str, &str)],
        entity: &LibraryEnrichmentEntity,
        default_priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<(Value, String), String> {
        let url = Self::audiodb_request_url(endpoint, params);
        self.execute_with_timeout_backoff(
            entity,
            default_priority,
            verbose_log,
            "TheAudioDB request",
            |manager| {
                if !manager.wait_for_audiodb_rate_limit_slot(entity, default_priority, verbose_log)
                {
                    return Err(Self::build_rate_limit_reason(
                        THEAUDIODB_RATE_LIMIT_DEFERRED,
                    ));
                }

                let response = manager
                    .http_client
                    .get(&url)
                    .set("User-Agent", WIKIMEDIA_USER_AGENT)
                    .set("Accept", "application/json")
                    .call()
                    .map_err(|error| {
                        let classified = Self::classify_ureq_failure(&error);
                        let message = format!("Request failed: {error}");
                        match classified {
                            HttpFailureKind::Timeout => Self::build_timeout_reason(message),
                            HttpFailureKind::Hard => Self::build_hard_reason(message),
                        }
                    })?;
                let mut body = String::new();
                response
                    .into_reader()
                    .read_to_string(&mut body)
                    .map_err(|error| {
                        if Self::classify_io_timeout(&error) {
                            Self::build_timeout_reason(format!("Failed to read response: {error}"))
                        } else {
                            Self::build_hard_reason(format!("Failed to read response: {error}"))
                        }
                    })?;
                let parsed = Self::parse_audiodb_payload(endpoint, &body, entity, verbose_log)
                    .map_err(Self::build_hard_reason)?;
                Ok((parsed, url.clone()))
            },
        )
    }

    fn empty_audiodb_payload(endpoint: &str) -> Value {
        let key = match endpoint {
            "searchalbum.php" => "album",
            _ => "artists",
        };
        let mut map = serde_json::Map::new();
        map.insert(key.to_string(), Value::Array(Vec::new()));
        Value::Object(map)
    }

    fn parse_audiodb_payload(
        endpoint: &str,
        body: &str,
        entity: &LibraryEnrichmentEntity,
        verbose_log: bool,
    ) -> Result<Value, String> {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            if verbose_log {
                info!(
                    "Enrichment[{}]: TheAudioDB returned empty body for {}",
                    Self::source_entity_label(entity),
                    endpoint
                );
            }
            return Ok(Self::empty_audiodb_payload(endpoint));
        }
        if trimmed.starts_with('<') {
            if verbose_log {
                info!(
                    "Enrichment[{}]: TheAudioDB returned non-JSON body for {} (treating as no results)",
                    Self::source_entity_label(entity),
                    endpoint
                );
            }
            return Ok(Self::empty_audiodb_payload(endpoint));
        }

        match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) => Ok(parsed),
            Err(error) if error.is_eof() => {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: TheAudioDB returned truncated JSON for {} (treating as no results)",
                        Self::source_entity_label(entity),
                        endpoint
                    );
                }
                Ok(Self::empty_audiodb_payload(endpoint))
            }
            Err(error) => {
                let snippet: String = trimmed.chars().take(120).collect();
                Err(format!(
                    "Invalid JSON response: {}; endpoint={endpoint}; prefix={}",
                    error, snippet
                ))
            }
        }
    }

    fn extract_audiodb_artist_candidates(
        value: &Value,
        request_url: &str,
    ) -> Vec<AudioDbArtistCandidate> {
        let mut out = Vec::new();
        let Some(candidates) = value["artists"].as_array() else {
            return out;
        };
        for candidate in candidates {
            let name = candidate["strArtist"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }

            let biography = candidate["strBiographyEN"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let genre = candidate["strGenre"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let image_url = [
                candidate["strArtistThumb"].as_str(),
                candidate["strArtistFanart"].as_str(),
                candidate["strArtistFanart2"].as_str(),
                candidate["strArtistFanart3"].as_str(),
            ]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|value| !value.is_empty())
            .map(str::to_string);
            let source_url = candidate["idArtist"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("https://www.theaudiodb.com/artist/{value}"))
                .unwrap_or_else(|| request_url.to_string());

            out.push(AudioDbArtistCandidate {
                name,
                biography,
                image_url,
                source_url,
                genre,
            });
        }
        out
    }

    fn extract_audiodb_album_candidates(
        value: &Value,
        request_url: &str,
    ) -> Vec<AudioDbAlbumCandidate> {
        let mut out = Vec::new();
        let Some(candidates) = value["album"].as_array() else {
            return out;
        };
        for candidate in candidates {
            let album = candidate["strAlbum"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if album.is_empty() {
                continue;
            }
            let artist = candidate["strArtist"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let description = candidate["strDescriptionEN"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let source_url = candidate["idAlbum"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("https://www.theaudiodb.com/album/{value}"))
                .unwrap_or_else(|| request_url.to_string());
            out.push(AudioDbAlbumCandidate {
                album,
                artist,
                description,
                source_url,
            });
        }
        out
    }

    fn score_audiodb_artist_candidate(artist: &str, candidate: &AudioDbArtistCandidate) -> i32 {
        let title_match_tier = Self::title_match_tier(artist, &candidate.name);
        if title_match_tier < TitleMatchTier::NearExact {
            return -4_800;
        }

        let normalized_artist = Self::normalize_text(&Self::collapse_whitespace(artist));
        let normalized_bio = Self::normalize_text(&candidate.biography);
        let normalized_genre = Self::normalize_text(&candidate.genre);
        let has_music_context = Self::contains_any(&normalized_bio, Self::music_artist_keywords())
            || Self::contains_any(&normalized_genre, Self::music_artist_keywords())
            || Self::contains_any(
                &normalized_genre,
                &[
                    "pop",
                    "rock",
                    "hip hop",
                    "electronic",
                    "dance",
                    "metal",
                    "jazz",
                    "blues",
                    "classical",
                    "k pop",
                    "r b",
                    "soul",
                    "country",
                ],
            );
        let word_count = normalized_artist.split_whitespace().count();
        if word_count == 1 && !has_music_context && title_match_tier < TitleMatchTier::Exact {
            return -3_200;
        }

        let mut score = 0;
        match title_match_tier {
            TitleMatchTier::Exact => score += 180,
            TitleMatchTier::NearExact => score += 132,
            TitleMatchTier::Fuzzy => score += 65,
            TitleMatchTier::None => {}
        }
        if has_music_context {
            score += 28;
        }
        if !normalized_artist.is_empty() && normalized_bio.contains(&normalized_artist) {
            score += 24;
        }
        if !normalized_genre.is_empty() {
            score += 8;
        }
        score
    }

    fn score_audiodb_album_candidate(
        album: &str,
        album_artist: &str,
        candidate: &AudioDbAlbumCandidate,
    ) -> i32 {
        let title_match_tier = Self::title_match_tier(album, &candidate.album);
        if title_match_tier < TitleMatchTier::NearExact {
            return -4_800;
        }

        let normalized_album_artist =
            Self::normalize_text(&Self::collapse_whitespace(album_artist));
        let normalized_candidate_artist = Self::normalize_text(&candidate.artist);
        let artist_matches = normalized_album_artist.is_empty()
            || normalized_album_artist == normalized_candidate_artist
            || normalized_candidate_artist.contains(&normalized_album_artist)
            || normalized_album_artist.contains(&normalized_candidate_artist)
            || (!normalized_album_artist.is_empty()
                && Self::compact_text(&normalized_album_artist)
                    == Self::compact_text(&normalized_candidate_artist));
        if !artist_matches {
            return -3_800;
        }

        let normalized_description = Self::normalize_text(&candidate.description);
        let has_album_context = Self::contains_any(&normalized_description, Self::album_keywords());

        let mut score = 0;
        match title_match_tier {
            TitleMatchTier::Exact => score += 150,
            TitleMatchTier::NearExact => score += 108,
            TitleMatchTier::Fuzzy => score += 60,
            TitleMatchTier::None => {}
        }
        if artist_matches {
            score += 30;
        }
        if has_album_context {
            score += 24;
        }
        score
    }

    fn search_wikipedia_titles_action(
        &mut self,
        query: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<Vec<String>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}?action=query&list=search&srsearch={}&srlimit={}&format=json&utf8=1&maxlag=5",
            WIKIPEDIA_ACTION_API_URL, encoded_query, MAX_CANDIDATES
        );
        let parsed = self.http_get_json(
            &url,
            entity,
            priority,
            verbose_log,
            "Wikipedia search/action",
        )?;
        Ok(Self::extract_title_strings(&parsed))
    }

    fn search_wikipedia_titles_action_nearmatch(
        &mut self,
        query: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<Vec<String>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}?action=query&list=search&srsearch={}&srwhat=nearmatch&srlimit={}&format=json&utf8=1&maxlag=5",
            WIKIPEDIA_ACTION_API_URL, encoded_query, MAX_CANDIDATES
        );
        let parsed = self.http_get_json(
            &url,
            entity,
            priority,
            verbose_log,
            "Wikipedia search/nearmatch",
        )?;
        Ok(Self::extract_title_strings(&parsed))
    }

    fn search_wikipedia_titles_rest(
        &mut self,
        endpoint: &str,
        query: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<Vec<String>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}/{}?q={}&limit={}",
            WIKIPEDIA_REST_BASE_URL, endpoint, encoded_query, MAX_CANDIDATES
        );
        let parsed =
            self.http_get_json(&url, entity, priority, verbose_log, "Wikipedia search/rest")?;
        Ok(Self::extract_title_strings(&parsed))
    }

    fn fetch_wikipedia_summary(
        &mut self,
        title: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<WikiSummary, String> {
        let encoded_title = urlencoding::encode(title);
        let url = format!(
            "{}?action=query&prop=extracts|pageimages|description|pageprops|info&\
             inprop=url&redirects=1&exintro=1&explaintext=1&pithumbsize=640&titles={}&\
             format=json&utf8=1&maxlag=5",
            WIKIPEDIA_ACTION_API_URL, encoded_title
        );
        let parsed =
            self.http_get_json(&url, entity, priority, verbose_log, "Wikipedia summary")?;
        let pages = parsed["query"]["pages"].as_object().ok_or_else(|| {
            Self::build_not_found_reason("Wikipedia summary response missing pages")
        })?;

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
            let wikidata_item_id = page_value["pageprops"]["wikibase_item"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);

            return Ok(WikiSummary {
                title,
                description,
                extract,
                canonical_url,
                image_url,
                page_type,
                wikidata_item_id,
            });
        }

        Err(Self::build_not_found_reason(format!(
            "Wikipedia page not found for '{title}'"
        )))
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
        &mut self,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
        query: &str,
        titles: &mut Vec<String>,
        seen_titles: &mut HashSet<String>,
    ) {
        if query.trim().is_empty() {
            return;
        }
        for found_titles in [
            self.search_wikipedia_titles_action_nearmatch(query, entity, priority, verbose_log),
            self.search_wikipedia_titles_action(query, entity, priority, verbose_log),
            self.search_wikipedia_titles_rest("search/title", query, entity, priority, verbose_log),
            self.search_wikipedia_titles_rest("search/page", query, entity, priority, verbose_log),
        ]
        .into_iter()
        .flatten()
        {
            for title in found_titles {
                Self::push_unique_title(titles, seen_titles, title);
            }
        }
    }

    fn candidate_titles_for_entity(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Vec<String> {
        let mut titles = Vec::new();
        let mut seen_titles = HashSet::new();

        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                let cleaned_artist = Self::collapse_whitespace(artist);
                let normalized_artist = Self::normalize_text(&cleaned_artist);
                let compact_artist = normalized_artist.replace(' ', "");
                let title_case_artist = Self::title_case_words(&cleaned_artist);
                if cleaned_artist.is_empty() {
                    return Vec::new();
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
                        entity,
                        priority,
                        verbose_log,
                        &query,
                        &mut titles,
                        &mut seen_titles,
                    );
                }
                if !compact_artist.is_empty()
                    && compact_artist != normalized_artist
                    && compact_artist != cleaned_artist.to_ascii_lowercase()
                {
                    self.append_titles_for_query(
                        entity,
                        priority,
                        verbose_log,
                        &compact_artist,
                        &mut titles,
                        &mut seen_titles,
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
                    return Vec::new();
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
                        entity,
                        priority,
                        verbose_log,
                        &query,
                        &mut titles,
                        &mut seen_titles,
                    );
                }
                if !compact_album.is_empty()
                    && compact_album != normalized_album
                    && compact_album != cleaned_album.to_ascii_lowercase()
                {
                    self.append_titles_for_query(
                        entity,
                        priority,
                        verbose_log,
                        &compact_album,
                        &mut titles,
                        &mut seen_titles,
                    );
                }
            }
        }

        if titles.len() > MAX_CANDIDATES * 4 {
            titles.truncate(MAX_CANDIDATES * 4);
        }
        titles
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

    fn build_error_outcome(
        entity: &LibraryEnrichmentEntity,
        source: EnrichmentSource,
        source_url: String,
        reason: String,
    ) -> FetchOutcome {
        let blurb = if Self::is_timeout_reason(&reason) {
            RETRYABLE_TIMEOUT_BLURB
        } else if Self::is_rate_limit_reason(&reason) {
            RETRYABLE_RATE_LIMIT_BLURB
        } else {
            HARD_ERROR_BLURB
        };
        Self::build_outcome(
            entity,
            LibraryEnrichmentStatus::Error,
            blurb.to_string(),
            None,
            None,
            source,
            source_url,
            Some(reason),
        )
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

    fn fetch_audiodb_outcome_for_entity(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> FetchOutcome {
        match entity {
            LibraryEnrichmentEntity::Artist { artist } => {
                self.fetch_audiodb_artist_outcome(entity, artist, initial_priority, start)
            }
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => self.fetch_audiodb_album_outcome(
                entity,
                album,
                album_artist,
                initial_priority,
                start,
            ),
        }
    }

    fn fetch_audiodb_artist_outcome(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        artist: &str,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> FetchOutcome {
        let entity_label = Self::source_entity_label(entity);
        let verbose_log = initial_priority == LibraryEnrichmentPriority::Interactive;
        let cleaned_artist = Self::collapse_whitespace(artist);
        if cleaned_artist.is_empty() {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                String::new(),
                None,
            );
        }

        let normalized_artist = Self::normalize_text(&cleaned_artist);
        let compact_artist = normalized_artist.replace(' ', "");
        let query_candidates = if initial_priority == LibraryEnrichmentPriority::Interactive {
            vec![
                cleaned_artist.clone(),
                Self::title_case_words(&cleaned_artist),
                compact_artist,
            ]
        } else {
            vec![cleaned_artist.clone()]
        };
        let mut queries = Vec::new();
        let mut seen_queries = HashSet::new();
        for query in query_candidates {
            let key = query.to_ascii_lowercase();
            if !query.trim().is_empty() && seen_queries.insert(key) {
                queries.push(query);
            }
        }
        if verbose_log {
            info!(
                "Enrichment[{}]: TheAudioDB artist stage starting ({} queries): {:?}",
                entity_label,
                queries.len(),
                queries
            );
        }

        let mut best_candidate: Option<AudioDbArtistCandidate> = None;
        let mut best_score = -10_001;
        let mut saw_timeout: Option<String> = None;
        let mut saw_hard_error: Option<String> = None;
        let mut saw_rate_limit: Option<String> = None;
        for query in queries {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: TheAudioDB artist stage stopped due to budget timeout",
                        entity_label
                    );
                }
                break;
            }
            let (value, request_url) = match self.audiodb_get_json(
                "search.php",
                &[("s", query.as_str())],
                entity,
                initial_priority,
                verbose_log,
            ) {
                Ok(response) => response,
                Err(error) => {
                    if Self::is_rate_limit_reason(&error)
                        || error.contains(THEAUDIODB_RATE_LIMIT_DEFERRED)
                    {
                        saw_rate_limit = Some(error.clone());
                        if verbose_log {
                            info!(
                                "Enrichment[{}]: TheAudioDB artist query '{}' deferred by rate limit",
                                entity_label, query
                            );
                        }
                        break;
                    }
                    if Self::is_timeout_reason(&error) {
                        saw_timeout = Some(error.clone());
                    } else if Self::is_hard_reason(&error) {
                        saw_hard_error = Some(error.clone());
                    }
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: TheAudioDB artist query '{}' failed: {}",
                            entity_label, query, error
                        );
                    }
                    continue;
                }
            };
            let candidates = Self::extract_audiodb_artist_candidates(&value, &request_url);
            if verbose_log {
                info!(
                    "Enrichment[{}]: TheAudioDB artist query '{}' yielded {} candidates",
                    entity_label,
                    query,
                    candidates.len()
                );
            }
            for candidate in candidates {
                let score = Self::score_audiodb_artist_candidate(artist, &candidate);
                if verbose_log {
                    info!(
                        "Enrichment[{}]: TheAudioDB artist candidate '{}' scored {}",
                        entity_label, candidate.name, score
                    );
                }
                if score > best_score {
                    best_score = score;
                    best_candidate = Some(candidate);
                }
            }
        }

        let Some(best_candidate) = best_candidate else {
            if let Some(reason) = saw_rate_limit {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::TheAudioDB,
                    String::new(),
                    reason,
                );
            }
            if let Some(reason) = saw_timeout {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::TheAudioDB,
                    String::new(),
                    reason,
                );
            }
            if let Some(reason) = saw_hard_error {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::TheAudioDB,
                    String::new(),
                    reason,
                );
            }
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                String::new(),
                None,
            );
        };
        if best_score < 96 {
            if verbose_log {
                info!(
                    "Enrichment[{}]: TheAudioDB artist best score {} below threshold 96",
                    entity_label, best_score
                );
            }
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                best_candidate.source_url,
                None,
            );
        }

        let blurb = Self::truncate_blurb(&best_candidate.biography, MAX_BLURB_CHARS);
        if blurb.trim().is_empty() {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                best_candidate.source_url,
                None,
            );
        }

        let image_path = best_candidate
            .image_url
            .as_ref()
            .and_then(|url| self.download_artist_image(entity, url, initial_priority, verbose_log));
        Self::build_outcome(
            entity,
            LibraryEnrichmentStatus::Ready,
            blurb,
            image_path,
            best_candidate.image_url,
            EnrichmentSource::TheAudioDB,
            best_candidate.source_url,
            None,
        )
    }

    fn fetch_audiodb_album_outcome(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        album: &str,
        album_artist: &str,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> FetchOutcome {
        let entity_label = Self::source_entity_label(entity);
        let verbose_log = initial_priority == LibraryEnrichmentPriority::Interactive;
        let cleaned_album = Self::collapse_whitespace(album);
        let cleaned_artist = Self::collapse_whitespace(album_artist);
        if cleaned_album.is_empty() {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                String::new(),
                None,
            );
        }

        let mut request_specs: Vec<Vec<(&str, String)>> = Vec::new();
        if !cleaned_artist.is_empty() {
            request_specs.push(vec![
                ("s", cleaned_artist.clone()),
                ("a", cleaned_album.clone()),
            ]);
            request_specs.push(vec![("s", cleaned_artist.clone())]);
        }
        request_specs.push(vec![("a", cleaned_album.clone())]);

        let mut best_candidate: Option<AudioDbAlbumCandidate> = None;
        let mut best_score = -10_001;
        let mut saw_timeout: Option<String> = None;
        let mut saw_hard_error: Option<String> = None;
        let mut saw_rate_limit: Option<String> = None;
        let mut seen_candidates = HashSet::new();
        for params in request_specs {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: TheAudioDB album stage stopped due to budget timeout",
                        entity_label
                    );
                }
                break;
            }
            let borrowed_params: Vec<(&str, &str)> = params
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect();
            let (value, request_url) = match self.audiodb_get_json(
                "searchalbum.php",
                &borrowed_params,
                entity,
                initial_priority,
                verbose_log,
            ) {
                Ok(response) => response,
                Err(error) => {
                    if Self::is_rate_limit_reason(&error)
                        || error.contains(THEAUDIODB_RATE_LIMIT_DEFERRED)
                    {
                        saw_rate_limit = Some(error.clone());
                        if verbose_log {
                            info!(
                                "Enrichment[{}]: TheAudioDB album request {:?} deferred by rate limit",
                                entity_label, params
                            );
                        }
                        break;
                    }
                    if Self::is_timeout_reason(&error) {
                        saw_timeout = Some(error.clone());
                    } else if Self::is_hard_reason(&error) {
                        saw_hard_error = Some(error.clone());
                    }
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: TheAudioDB album request {:?} failed: {}",
                            entity_label, params, error
                        );
                    }
                    continue;
                }
            };
            for candidate in Self::extract_audiodb_album_candidates(&value, &request_url) {
                let dedupe_key = format!(
                    "{}\u{001f}{}\u{001f}{}",
                    candidate.source_url, candidate.album, candidate.artist
                );
                if !seen_candidates.insert(dedupe_key) {
                    continue;
                }
                let score = Self::score_audiodb_album_candidate(album, album_artist, &candidate);
                if verbose_log {
                    info!(
                        "Enrichment[{}]: TheAudioDB album candidate '{} / {}' scored {}",
                        entity_label, candidate.album, candidate.artist, score
                    );
                }
                if score > best_score {
                    best_score = score;
                    best_candidate = Some(candidate);
                }
            }
        }

        let Some(best_candidate) = best_candidate else {
            if let Some(reason) = saw_rate_limit {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::TheAudioDB,
                    String::new(),
                    reason,
                );
            }
            if let Some(reason) = saw_timeout {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::TheAudioDB,
                    String::new(),
                    reason,
                );
            }
            if let Some(reason) = saw_hard_error {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::TheAudioDB,
                    String::new(),
                    reason,
                );
            }
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                String::new(),
                None,
            );
        };
        if best_score < 90 {
            if verbose_log {
                info!(
                    "Enrichment[{}]: TheAudioDB album best score {} below threshold 90",
                    entity_label, best_score
                );
            }
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                best_candidate.source_url,
                None,
            );
        }

        let blurb = Self::truncate_blurb(&best_candidate.description, MAX_BLURB_CHARS);
        if blurb.trim().is_empty() {
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::TheAudioDB,
                best_candidate.source_url,
                None,
            );
        }

        Self::build_outcome(
            entity,
            LibraryEnrichmentStatus::Ready,
            blurb,
            None,
            None,
            EnrichmentSource::TheAudioDB,
            best_candidate.source_url,
            None,
        )
    }

    fn fetch_wikipedia_outcome_for_entity(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> FetchOutcome {
        let entity_label = Self::source_entity_label(entity);
        let verbose_log = initial_priority == LibraryEnrichmentPriority::Interactive;
        let mut saw_timeout: Option<String> = None;
        let mut saw_hard_error: Option<String> = None;
        let direct_titles = Self::direct_lookup_titles_for_entity(entity);
        if verbose_log {
            info!(
                "Enrichment[{}]: direct-title stage starting ({} candidates): {:?}",
                entity_label,
                direct_titles.len(),
                direct_titles
            );
        }

        for direct_title in direct_titles.into_iter().take(6) {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: direct-title stage stopped due to budget timeout",
                        entity_label
                    );
                }
                break;
            }
            let summary = match self.fetch_wikipedia_summary(
                &direct_title,
                entity,
                initial_priority,
                verbose_log,
            ) {
                Ok(summary) => summary,
                Err(error) => {
                    if Self::is_timeout_reason(&error) {
                        saw_timeout = Some(error.clone());
                    } else if Self::is_hard_reason(&error) {
                        saw_hard_error = Some(error.clone());
                    }
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: direct-title '{}' fetch failed: {}",
                            entity_label, direct_title, error
                        );
                    }
                    continue;
                }
            };
            if let Some(reason) = Self::direct_summary_rejection_reason(entity, &summary) {
                let alias_override = match entity {
                    LibraryEnrichmentEntity::Artist { artist } => self
                        .artist_matches_summary_via_wikidata_alias(
                            artist,
                            &summary,
                            entity,
                            initial_priority,
                            verbose_log,
                        ),
                    LibraryEnrichmentEntity::Album { .. } => false,
                };
                if !alias_override {
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: direct-title '{}' resolved to '{}' but rejected: {}",
                            entity_label, direct_title, summary.title, reason
                        );
                    }
                    continue;
                }
                if verbose_log {
                    info!(
                        "Enrichment[{}]: direct-title '{}' accepted by Wikidata alias evidence for '{}'",
                        entity_label, direct_title, summary.title
                    );
                }
            }

            let blurb = Self::truncate_blurb(&summary.extract, MAX_BLURB_CHARS);
            if blurb.trim().is_empty() {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: direct-title '{}' resolved to '{}' but blurb was empty",
                        entity_label, direct_title, summary.title
                    );
                }
                continue;
            }
            let image_path = match entity {
                LibraryEnrichmentEntity::Artist { .. } => {
                    summary.image_url.as_ref().and_then(|url| {
                        self.download_artist_image(entity, url, initial_priority, verbose_log)
                    })
                }
                LibraryEnrichmentEntity::Album { .. } => None,
            };
            info!(
                "Enrichment[{}]: direct-title hit '{}' (from query '{}')",
                entity_label, summary.title, direct_title
            );
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::Ready,
                blurb,
                image_path,
                summary.image_url,
                EnrichmentSource::Wikipedia,
                summary.canonical_url,
                None,
            );
        }

        let titles = self.candidate_titles_for_entity(entity, initial_priority, verbose_log);

        if titles.is_empty() {
            if verbose_log {
                info!(
                    "Enrichment[{}]: search stage produced no candidate titles",
                    entity_label
                );
            }
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

        let mut ranked_candidates: Vec<RankedSummaryCandidate> = Vec::new();
        let min_threshold = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 92,
            LibraryEnrichmentEntity::Album { .. } => 88,
        };
        if verbose_log {
            info!(
                "Enrichment[{}]: scored-search stage starting ({} candidates, min={})",
                entity_label,
                titles.len(),
                min_threshold
            );
        }

        for title in titles.into_iter().take(MAX_SUMMARY_FETCHES) {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: scored-search stopped due to budget timeout",
                        entity_label
                    );
                }
                break;
            }

            let summary =
                match self.fetch_wikipedia_summary(&title, entity, initial_priority, verbose_log) {
                    Ok(summary) => summary,
                    Err(error) => {
                        if Self::is_timeout_reason(&error) {
                            saw_timeout = Some(error.clone());
                        } else if Self::is_hard_reason(&error) {
                            saw_hard_error = Some(error.clone());
                        }
                        if verbose_log {
                            info!(
                                "Enrichment[{}]: candidate '{}' fetch failed: {}",
                                entity_label, title, error
                            );
                        }
                        continue;
                    }
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
            ranked_candidates.push(RankedSummaryCandidate {
                requested_title: title,
                summary,
                deterministic_score: score,
            });
        }

        if ranked_candidates.is_empty() {
            if let Some(reason) = saw_timeout {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::Wikipedia,
                    String::new(),
                    reason,
                );
            }
            if let Some(reason) = saw_hard_error {
                return Self::build_error_outcome(
                    entity,
                    EnrichmentSource::Wikipedia,
                    String::new(),
                    reason,
                );
            }
            if verbose_log {
                info!(
                    "Enrichment[{}]: no fetchable/scorable candidate summaries",
                    entity_label
                );
            }
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

        let mut best_score = -10_001;
        let mut best_candidate_index = 0usize;
        for (index, candidate) in ranked_candidates.iter().enumerate() {
            let alias_bonus = match entity {
                LibraryEnrichmentEntity::Artist { artist } => {
                    let needs_help = candidate.deterministic_score < min_threshold;
                    if needs_help
                        && self.artist_matches_summary_via_wikidata_alias(
                            artist,
                            &candidate.summary,
                            entity,
                            initial_priority,
                            verbose_log,
                        )
                    {
                        110
                    } else {
                        0
                    }
                }
                LibraryEnrichmentEntity::Album { .. } => 0,
            };
            let combined_score = candidate.deterministic_score.saturating_add(alias_bonus);
            if verbose_log {
                info!(
                    "Enrichment[{}]: candidate '{}' -> resolved '{}' deterministic={} alias_bonus={} combined={}",
                    entity_label,
                    candidate.requested_title,
                    candidate.summary.title,
                    candidate.deterministic_score,
                    alias_bonus,
                    combined_score
                );
            }
            if combined_score > best_score {
                best_score = combined_score;
                best_candidate_index = index;
            }
        }
        let best_candidate = &ranked_candidates[best_candidate_index];
        let best_summary = &best_candidate.summary;

        if best_score < min_threshold {
            if verbose_log {
                info!(
                    "Enrichment[{}]: best score {} below threshold {} (best title '{}')",
                    entity_label, best_score, min_threshold, best_summary.title
                );
            }
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::Wikipedia,
                best_summary.canonical_url.clone(),
                None,
            );
        }

        let blurb = Self::truncate_blurb(&best_summary.extract, MAX_BLURB_CHARS);
        if blurb.trim().is_empty() {
            if verbose_log {
                info!(
                    "Enrichment[{}]: best summary '{}' had empty blurb",
                    entity_label, best_summary.title
                );
            }
            return Self::build_outcome(
                entity,
                LibraryEnrichmentStatus::NotFound,
                String::new(),
                None,
                None,
                EnrichmentSource::Wikipedia,
                best_summary.canonical_url.clone(),
                None,
            );
        }

        let image_path = match entity {
            LibraryEnrichmentEntity::Artist { .. } => {
                best_summary.image_url.as_ref().and_then(|url| {
                    self.download_artist_image(entity, url, initial_priority, verbose_log)
                })
            }
            LibraryEnrichmentEntity::Album { .. } => None,
        };
        if verbose_log {
            info!(
                "Enrichment[{}]: scored-search accepted '{}' with combined score {}",
                entity_label, best_summary.title, best_score
            );
        }

        Self::build_outcome(
            entity,
            LibraryEnrichmentStatus::Ready,
            blurb,
            image_path,
            best_summary.image_url.clone(),
            EnrichmentSource::Wikipedia,
            best_summary.canonical_url.clone(),
            None,
        )
    }

    fn search_wikidata_entities(
        &mut self,
        query: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<Vec<WikidataCandidate>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}?action=wbsearchentities&search={}&language=en&uselang=en&type=item&\
             limit={}&format=json",
            WIKIDATA_ACTION_API_URL, encoded_query, MAX_CANDIDATES
        );
        let parsed = self.http_get_json(&url, entity, priority, verbose_log, "Wikidata search")?;
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
                out.push(WikidataCandidate {
                    id,
                    label,
                    description,
                });
            }
        }
        Ok(out)
    }

    fn fetch_wikidata_entity_details(
        &mut self,
        entity_id: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Result<Value, String> {
        let encoded_id = urlencoding::encode(entity_id);
        let url = format!(
            "{}?action=wbgetentities&ids={}&props=labels|descriptions|aliases|sitelinks&\
             languages=en&sitefilter=enwiki&format=json",
            WIKIDATA_ACTION_API_URL, encoded_id
        );
        self.http_get_json(&url, entity, priority, verbose_log, "Wikidata details")
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
                let is_single_token_query = normalized_artist.split_whitespace().count() == 1;
                let has_music_context =
                    Self::contains_any(&normalized_description, Self::music_artist_keywords());
                if is_single_token_query && !has_music_context {
                    let exactish_match = normalized_label == normalized_artist
                        || (!compact_artist.is_empty() && compact_artist == compact_label);
                    if exactish_match {
                        score -= 240;
                    } else {
                        score -= 40;
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

    fn wikidata_enwiki_title(details: &Value, entity_id: &str) -> Option<String> {
        details["entities"][entity_id]["sitelinks"]["enwiki"]["title"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    fn wikidata_alias_variants_from_details(details: &Value, entity_id: &str) -> HashSet<String> {
        let mut variants = HashSet::new();
        if let Some(label) = details["entities"][entity_id]["labels"]["en"]["value"].as_str() {
            Self::push_name_identity_variants(&mut variants, label);
        }
        if let Some(alias_list) = details["entities"][entity_id]["aliases"]["en"].as_array() {
            for alias in alias_list {
                if let Some(value) = alias["value"].as_str() {
                    Self::push_name_identity_variants(&mut variants, value);
                }
            }
        }
        if let Some(enwiki_title) = Self::wikidata_enwiki_title(details, entity_id) {
            Self::push_name_identity_variants(&mut variants, &enwiki_title);
        }
        variants
    }

    fn wikidata_alias_variants_for_item(
        &mut self,
        item_id: &str,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Option<HashSet<String>> {
        if let Some(cached) = self.wikidata_alias_cache.get(item_id) {
            return Some(cached.clone());
        }
        let details = self
            .fetch_wikidata_entity_details(item_id, entity, priority, verbose_log)
            .ok()?;
        let variants = Self::wikidata_alias_variants_from_details(&details, item_id);
        if variants.is_empty() {
            return None;
        }
        self.wikidata_alias_cache
            .insert(item_id.to_string(), variants.clone());
        Some(variants)
    }

    fn artist_matches_summary_via_wikidata_alias(
        &mut self,
        artist: &str,
        summary: &WikiSummary,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> bool {
        if !Self::artist_wikidata_alias_match_allowed(summary) {
            return false;
        }
        let Some(item_id) = summary.wikidata_item_id.as_deref() else {
            return false;
        };
        let Some(variants) =
            self.wikidata_alias_variants_for_item(item_id, entity, priority, verbose_log)
        else {
            return false;
        };
        Self::name_matches_identity_variants(artist, &variants)
    }

    fn fetch_wikidata_fallback(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        initial_priority: LibraryEnrichmentPriority,
        start: Instant,
    ) -> Option<FetchOutcome> {
        let entity_label = Self::source_entity_label(entity);
        let verbose_log = initial_priority == LibraryEnrichmentPriority::Interactive;
        let mut saw_timeout: Option<String> = None;
        let mut saw_hard_error: Option<String> = None;
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
        let mut scored_candidates: Vec<(WikidataCandidate, i32)> = Vec::new();
        if verbose_log {
            info!(
                "Enrichment[{}]: Wikidata fallback starting with queries {:?}",
                entity_label, query_candidates
            );
        }

        for query in query_candidates {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: Wikidata fallback stopped due to budget timeout",
                        entity_label
                    );
                }
                break;
            }
            let candidates = match self.search_wikidata_entities(
                &query,
                entity,
                initial_priority,
                verbose_log,
            ) {
                Ok(candidates) => candidates,
                Err(error) => {
                    if Self::is_timeout_reason(&error) {
                        saw_timeout = Some(error.clone());
                    } else if Self::is_hard_reason(&error) {
                        saw_hard_error = Some(error.clone());
                    }
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: Wikidata query '{}' failed: {}",
                            entity_label, query, error
                        );
                    }
                    continue;
                }
            };
            if candidates.is_empty() {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: Wikidata query '{}' returned no candidates",
                        entity_label, query
                    );
                }
                continue;
            }
            for candidate in candidates {
                if !seen_ids.insert(candidate.id.clone()) {
                    continue;
                }
                let score = Self::score_wikidata_candidate(entity, &candidate);
                if verbose_log {
                    info!(
                        "Enrichment[{}]: Wikidata candidate '{}' ({}) scored {}",
                        entity_label, candidate.label, candidate.id, score
                    );
                }
                scored_candidates.push((candidate, score));
            }
        }

        if scored_candidates.is_empty() {
            if let Some(reason) = saw_timeout {
                return Some(Self::build_error_outcome(
                    entity,
                    EnrichmentSource::Wikipedia,
                    String::new(),
                    reason,
                ));
            }
            if let Some(reason) = saw_hard_error {
                return Some(Self::build_error_outcome(
                    entity,
                    EnrichmentSource::Wikipedia,
                    String::new(),
                    reason,
                ));
            }
            return None;
        }
        scored_candidates.sort_by(|left, right| right.1.cmp(&left.1));
        let query_name = match entity {
            LibraryEnrichmentEntity::Artist { artist } => artist.as_str(),
            LibraryEnrichmentEntity::Album { album, .. } => album.as_str(),
        };
        let min_candidate_score = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 70,
            LibraryEnrichmentEntity::Album { .. } => 65,
        };
        let summary_threshold = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 92,
            LibraryEnrichmentEntity::Album { .. } => 88,
        };

        for (candidate, deterministic_score) in
            scored_candidates.into_iter().take(MAX_SUMMARY_FETCHES)
        {
            self.drain_bus_messages_nonblocking();
            if self.budget_exceeded(entity, start, initial_priority) {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: Wikidata details stage timed out by budget",
                        entity_label
                    );
                }
                break;
            }

            let details = match self.fetch_wikidata_entity_details(
                &candidate.id,
                entity,
                initial_priority,
                verbose_log,
            ) {
                Ok(details) => details,
                Err(error) => {
                    if Self::is_timeout_reason(&error) {
                        saw_timeout = Some(error.clone());
                    } else if Self::is_hard_reason(&error) {
                        saw_hard_error = Some(error.clone());
                    }
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: Wikidata details fetch failed for {}: {}",
                            entity_label, candidate.id, error
                        );
                    }
                    continue;
                }
            };
            let alias_variants =
                Self::wikidata_alias_variants_from_details(&details, &candidate.id);
            if !alias_variants.is_empty() {
                self.wikidata_alias_cache
                    .insert(candidate.id.clone(), alias_variants.clone());
            }
            let alias_match = Self::name_matches_identity_variants(query_name, &alias_variants);
            let effective_score = deterministic_score + if alias_match { 120 } else { 0 };
            if verbose_log {
                info!(
                    "Enrichment[{}]: Wikidata candidate '{}' ({}) deterministic={} alias_match={} effective={}",
                    entity_label,
                    candidate.label,
                    candidate.id,
                    deterministic_score,
                    alias_match,
                    effective_score
                );
            }
            if effective_score < min_candidate_score {
                continue;
            }

            let Some(enwiki_title) = Self::wikidata_enwiki_title(&details, &candidate.id) else {
                continue;
            };
            let summary = match self.fetch_wikipedia_summary(
                &enwiki_title,
                entity,
                initial_priority,
                verbose_log,
            ) {
                Ok(summary) => summary,
                Err(error) => {
                    if Self::is_timeout_reason(&error) {
                        saw_timeout = Some(error.clone());
                    } else if Self::is_hard_reason(&error) {
                        saw_hard_error = Some(error.clone());
                    }
                    if verbose_log {
                        info!(
                            "Enrichment[{}]: Wikidata enwiki '{}' fetch failed: {}",
                            entity_label, enwiki_title, error
                        );
                    }
                    continue;
                }
            };
            let mut score = match entity {
                LibraryEnrichmentEntity::Artist { artist } => {
                    Self::score_artist_summary(artist, &summary)
                }
                LibraryEnrichmentEntity::Album {
                    album,
                    album_artist,
                } => Self::score_album_summary(album, album_artist, &summary),
            };
            if alias_match
                && matches!(entity, LibraryEnrichmentEntity::Artist { .. })
                && Self::artist_wikidata_alias_match_allowed(&summary)
            {
                score += 110;
            }
            if verbose_log {
                info!(
                    "Enrichment[{}]: Wikidata enwiki '{}' resolved '{}' scored {} (threshold {})",
                    entity_label, enwiki_title, summary.title, score, summary_threshold
                );
            }
            if score < summary_threshold {
                continue;
            }
            let blurb = Self::truncate_blurb(&summary.extract, MAX_BLURB_CHARS);
            if blurb.trim().is_empty() {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: Wikidata enwiki '{}' had empty blurb",
                        entity_label, enwiki_title
                    );
                }
                continue;
            }
            let image_path = match entity {
                LibraryEnrichmentEntity::Artist { .. } => {
                    summary.image_url.as_ref().and_then(|url| {
                        self.download_artist_image(entity, url, initial_priority, verbose_log)
                    })
                }
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

        if let Some(reason) = saw_timeout {
            return Some(Self::build_error_outcome(
                entity,
                EnrichmentSource::Wikipedia,
                String::new(),
                reason,
            ));
        }
        if let Some(reason) = saw_hard_error {
            return Some(Self::build_error_outcome(
                entity,
                EnrichmentSource::Wikipedia,
                String::new(),
                reason,
            ));
        }
        None
    }

    fn fetch_outcome_for_entity(
        &mut self,
        entity: &LibraryEnrichmentEntity,
        priority: LibraryEnrichmentPriority,
    ) -> FetchOutcome {
        let entity_label = Self::source_entity_label(entity);
        let verbose_log = priority == LibraryEnrichmentPriority::Interactive;
        let started_at = Instant::now();
        let mut best_error: Option<FetchOutcome> = None;
        let audiodb_outcome = self.fetch_audiodb_outcome_for_entity(entity, priority, started_at);
        if audiodb_outcome.payload.status == LibraryEnrichmentStatus::Ready {
            if verbose_log {
                info!(
                    "Enrichment[{}]: TheAudioDB stage returned Ready",
                    entity_label
                );
            }
            return audiodb_outcome;
        }
        if audiodb_outcome.payload.status == LibraryEnrichmentStatus::Error {
            best_error = Some(audiodb_outcome.clone());
        }
        if priority == LibraryEnrichmentPriority::Prefetch
            && matches!(entity, LibraryEnrichmentEntity::Artist { .. })
        {
            return audiodb_outcome;
        }
        let wiki_outcome = self.fetch_wikipedia_outcome_for_entity(entity, priority, started_at);
        if wiki_outcome.payload.status == LibraryEnrichmentStatus::Ready {
            if verbose_log {
                info!(
                    "Enrichment[{}]: Wikipedia stage returned Ready",
                    entity_label
                );
            }
            return wiki_outcome;
        }
        if wiki_outcome.payload.status == LibraryEnrichmentStatus::Error {
            best_error = Some(wiki_outcome.clone());
        }
        if verbose_log {
            info!(
                "Enrichment[{}]: Wikipedia stage returned {:?}, trying Wikidata fallback",
                entity_label, wiki_outcome.payload.status
            );
        }
        if let Some(wikidata_outcome) = self.fetch_wikidata_fallback(entity, priority, started_at) {
            if verbose_log {
                info!(
                    "Enrichment[{}]: Wikidata fallback returned {:?}",
                    entity_label, wikidata_outcome.payload.status
                );
            }
            if wikidata_outcome.payload.status == LibraryEnrichmentStatus::Error {
                best_error = Some(wikidata_outcome.clone());
            } else {
                return wikidata_outcome;
            }
        }

        if let Some(error_outcome) = best_error {
            if verbose_log {
                info!(
                    "Enrichment[{}]: final outcome {:?}",
                    entity_label, error_outcome.payload.status
                );
            }
            return error_outcome;
        }
        if verbose_log {
            info!(
                "Enrichment[{}]: final outcome {:?}",
                entity_label, wiki_outcome.payload.status
            );
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
        &mut self,
        entity: &LibraryEnrichmentEntity,
        image_url: &str,
        priority: LibraryEnrichmentPriority,
        verbose_log: bool,
    ) -> Option<PathBuf> {
        let cache_dir = Self::image_cache_dir()?;
        let bytes = match self.execute_with_timeout_backoff(
            entity,
            priority,
            verbose_log,
            "artist image download",
            |manager| {
                let response = manager
                    .http_client
                    .get(image_url)
                    .set("User-Agent", WIKIMEDIA_USER_AGENT)
                    .call()
                    .map_err(|error| {
                        let classified = Self::classify_ureq_failure(&error);
                        let message = format!("Image request failed: {error}");
                        match classified {
                            HttpFailureKind::Timeout => Self::build_timeout_reason(message),
                            HttpFailureKind::Hard => Self::build_hard_reason(message),
                        }
                    })?;
                let mut reader = response.into_reader();
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).map_err(|error| {
                    if Self::classify_io_timeout(&error) {
                        Self::build_timeout_reason(format!("Image read failed: {error}"))
                    } else {
                        Self::build_hard_reason(format!("Image read failed: {error}"))
                    }
                })?;
                if bytes.is_empty() {
                    return Err(Self::build_hard_reason("Image response was empty"));
                }
                Ok(bytes)
            },
        ) {
            Ok(bytes) => bytes,
            Err(reason) => {
                if verbose_log {
                    info!(
                        "Enrichment[{}]: image fetch failed for '{}': {}",
                        Self::source_entity_label(entity),
                        image_url,
                        reason
                    );
                }
                return None;
            }
        };

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
            LibraryEnrichmentStatus::Error => {
                let lowered = payload.blurb.to_ascii_lowercase();
                if lowered.contains("timed out") || lowered.contains("rate limit") {
                    0
                } else {
                    ERROR_TTL_DAYS
                }
            }
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
        self.wikidata_alias_cache.clear();
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

    fn replace_prefetch_queue(&mut self, entities: Vec<LibraryEnrichmentEntity>) {
        let mut ordered = Vec::new();
        let mut desired_set = HashSet::new();
        for entity in entities {
            if desired_set.insert(entity.clone()) {
                ordered.push(entity);
            }
        }

        self.queued_priorities.retain(|entity, priority| {
            *priority == LibraryEnrichmentPriority::Interactive || desired_set.contains(entity)
        });
        self.queued_prefetch.clear();

        for entity in ordered {
            if self.queued_prefetch.len() >= MAX_PENDING_PREFETCH_REQUESTS {
                break;
            }
            if matches!(
                self.queued_priorities.get(&entity),
                Some(LibraryEnrichmentPriority::Interactive)
            ) {
                continue;
            }
            if self.in_flight_priorities.contains_key(&entity) {
                continue;
            }
            self.queued_priorities
                .insert(entity.clone(), LibraryEnrichmentPriority::Prefetch);
            self.queued_prefetch.push_back(entity);
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
            Message::Library(LibraryMessage::ReplaceEnrichmentPrefetchQueue { entities }) => {
                self.replace_prefetch_queue(entities);
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
                    source_name: THEAUDIODB_SOURCE_NAME.to_string(),
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
                let bypass_cached_for_interactive = priority
                    == LibraryEnrichmentPriority::Interactive
                    && matches!(
                        cached.status,
                        LibraryEnrichmentStatus::NotFound
                            | LibraryEnrichmentStatus::Error
                            | LibraryEnrichmentStatus::Disabled
                    );
                if bypass_cached_for_interactive {
                    info!(
                        "Enrichment[{}]: interactive request bypassing cached {:?} result",
                        Self::source_entity_label(&entity),
                        cached.status
                    );
                } else if priority == LibraryEnrichmentPriority::Interactive {
                    info!(
                        "Enrichment[{}]: interactive request satisfied from cache ({:?})",
                        Self::source_entity_label(&entity),
                        cached.status
                    );
                }
                if bypass_cached_for_interactive {
                    // Continue into a fresh fetch below.
                } else {
                    if let Some(path) = cached.image_path.as_ref() {
                        if !path.exists() || !Self::file_looks_like_supported_image(path) {
                            if path.exists() {
                                let _ = fs::remove_file(path);
                            }
                            if let Err(error) = self.db_manager.clear_library_enrichment_image_path(
                                path.to_string_lossy().as_ref(),
                            ) {
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
    use std::collections::HashSet;

    use super::{
        AudioDbAlbumCandidate, AudioDbArtistCandidate, LibraryEnrichmentManager, TitleMatchTier,
        WikiSummary, WikidataCandidate,
    };

    fn sample_summary(title: &str, description: &str, extract: &str) -> WikiSummary {
        WikiSummary {
            title: title.to_string(),
            description: description.to_string(),
            extract: extract.to_string(),
            canonical_url: String::new(),
            image_url: None,
            page_type: String::new(),
            wikidata_item_id: None,
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
    fn test_name_matches_identity_variants_accepts_compact_case_variants() {
        let mut variants = HashSet::new();
        LibraryEnrichmentManager::push_name_identity_variants(&mut variants, "Sample Group");
        assert!(LibraryEnrichmentManager::name_matches_identity_variants(
            "SAMPLEGROUP",
            &variants
        ));
    }

    #[test]
    fn test_score_audiodb_artist_candidate_rewards_near_exact_music_match() {
        let candidate = AudioDbArtistCandidate {
            name: "Sample Group".to_string(),
            biography: "Sample Group is an electronic music duo.".to_string(),
            image_url: None,
            source_url: String::new(),
            genre: "Electronic".to_string(),
        };
        assert!(
            LibraryEnrichmentManager::score_audiodb_artist_candidate("SAMPLEGROUP", &candidate)
                >= 120
        );
    }

    #[test]
    fn test_score_audiodb_album_candidate_rejects_wrong_artist() {
        let candidate = AudioDbAlbumCandidate {
            album: "Test Record".to_string(),
            artist: "Different Performer".to_string(),
            description: "Test Record is a studio album.".to_string(),
            source_url: String::new(),
        };
        assert!(
            LibraryEnrichmentManager::score_audiodb_album_candidate(
                "Test Record",
                "Sample Performer",
                &candidate
            ) < 0
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
    fn test_title_match_tier_exact_case_variant() {
        assert_eq!(
            LibraryEnrichmentManager::title_match_tier("samplegroup", "SampleGroup"),
            TitleMatchTier::Exact
        );
    }

    #[test]
    fn test_title_match_tier_exact_parenthetical_compact_variant() {
        assert_eq!(
            LibraryEnrichmentManager::title_match_tier(
                "samplegroup",
                "Sample Group (South Korean band)"
            ),
            TitleMatchTier::Exact
        );
    }

    #[test]
    fn test_direct_summary_matches_entity_allows_exact_mixed_case_artist() {
        let summary = sample_summary(
            "Sample Group",
            "South Korean girl group",
            "Sample Group is a South Korean girl group.",
        );
        let entity = crate::protocol::LibraryEnrichmentEntity::Artist {
            artist: "SAMPLE GROUP".to_string(),
        };
        assert!(LibraryEnrichmentManager::direct_summary_matches_entity(
            &entity, &summary
        ));
    }

    #[test]
    fn test_direct_summary_matches_entity_allows_alias_redirect_with_music_context() {
        let summary = sample_summary(
            "Canonical Performer",
            "American rapper",
            "Canonical Performer, known professionally as Alias42, is an American rapper.",
        );
        let entity = crate::protocol::LibraryEnrichmentEntity::Artist {
            artist: "Alias42".to_string(),
        };
        assert!(LibraryEnrichmentManager::direct_summary_matches_entity(
            &entity, &summary
        ));
    }

    #[test]
    fn test_direct_summary_matches_entity_rejects_single_token_non_music_exact() {
        let summary = sample_summary(
            "Sample",
            "Ancient city and kingdom",
            "Sample was an ancient settlement.",
        );
        let entity = crate::protocol::LibraryEnrichmentEntity::Artist {
            artist: "SAMPLE".to_string(),
        };
        assert!(!LibraryEnrichmentManager::direct_summary_matches_entity(
            &entity, &summary
        ));
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
    fn test_score_artist_summary_accepts_alias_redirect_with_music_context() {
        let summary = sample_summary(
            "Canonical Performer",
            "American rapper",
            "Canonical Performer, known professionally as Alias42, is an American rapper.",
        );
        assert!(LibraryEnrichmentManager::score_artist_summary("Alias42", &summary) >= 92);
    }

    #[test]
    fn test_score_artist_summary_rejects_alias_without_music_context() {
        let summary = sample_summary(
            "Canonical Place",
            "Ancient settlement",
            "Canonical Place, also known as Alias42, was an ancient settlement.",
        );
        assert!(LibraryEnrichmentManager::score_artist_summary("Alias42", &summary) < 0);
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

    #[test]
    fn test_parse_audiodb_payload_empty_body_returns_empty_candidates() {
        let entity = crate::protocol::LibraryEnrichmentEntity::Artist {
            artist: "Sample".to_string(),
        };
        let parsed =
            LibraryEnrichmentManager::parse_audiodb_payload("searchalbum.php", "", &entity, false)
                .expect("empty response should be handled");
        assert!(parsed["album"]
            .as_array()
            .is_some_and(|items| items.is_empty()));
    }

    #[test]
    fn test_parse_audiodb_payload_non_json_body_returns_empty_candidates() {
        let entity = crate::protocol::LibraryEnrichmentEntity::Artist {
            artist: "Sample".to_string(),
        };
        let parsed = LibraryEnrichmentManager::parse_audiodb_payload(
            "search.php",
            "<html>rate limited</html>",
            &entity,
            false,
        )
        .expect("html response should be handled");
        assert!(parsed["artists"]
            .as_array()
            .is_some_and(|items| items.is_empty()));
    }
}
