//! Online library enrichment runtime component.
//!
//! This manager fetches display-only artist/album blurbs (and artist images)
//! from Wikipedia, then stores short-lived cache records for the UI layer.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, warn};
use serde_json::Value;
use tokio::sync::broadcast::{Receiver, Sender};

use crate::db_manager::DbManager;
use crate::protocol::{
    LibraryEnrichmentEntity, LibraryEnrichmentPayload, LibraryEnrichmentPriority,
    LibraryEnrichmentStatus, LibraryMessage, Message,
};

const WIKIPEDIA_API_SEARCH_URL: &str = "https://en.wikipedia.org/w/api.php";
const WIKIPEDIA_REST_SUMMARY_URL: &str = "https://en.wikipedia.org/api/rest_v1/page/summary";
const SOURCE_NAME: &str = "Wikipedia";
const IMAGE_CACHE_DIR_NAME: &str = "library_enrichment/images";
const READY_METADATA_TTL_DAYS: u32 = 30;
const NOT_FOUND_TTL_DAYS: u32 = 7;
const ERROR_TTL_DAYS: u32 = 1;
const MAX_CANDIDATES: usize = 5;
const MAX_BLURB_CHARS: usize = 360;

#[derive(Debug, Clone)]
struct WikiSummary {
    title: String,
    description: String,
    extract: String,
    canonical_url: String,
    image_url: Option<String>,
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
            .timeout_connect(Duration::from_secs(6))
            .timeout_read(Duration::from_secs(8))
            .timeout_write(Duration::from_secs(8))
            .build();

        Self {
            bus_consumer,
            bus_producer,
            db_manager,
            online_metadata_enabled: false,
            artist_image_cache_ttl_days: 30,
            artist_image_cache_max_size_mb: 256,
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

    fn contains_any(haystack: &str, values: &[&str]) -> bool {
        values.iter().any(|value| haystack.contains(value))
    }

    fn score_artist_summary(artist: &str, summary: &WikiSummary) -> i32 {
        let normalized_artist = Self::normalize_text(artist);
        let normalized_title = Self::normalize_text(&summary.title);
        let normalized_description = Self::normalize_text(&summary.description);
        let normalized_extract = Self::normalize_text(&summary.extract);

        if normalized_description.contains("disambiguation") {
            return -10_000;
        }

        let mut score = 0;
        if normalized_title == normalized_artist {
            score += 100;
        } else if normalized_title.starts_with(&normalized_artist)
            || normalized_artist.starts_with(&normalized_title)
        {
            score += 65;
        } else if normalized_title.contains(&normalized_artist)
            || normalized_artist.contains(&normalized_title)
        {
            score += 35;
        }

        if Self::contains_any(
            &normalized_description,
            &[
                "musician",
                "singer",
                "band",
                "rapper",
                "composer",
                "producer",
                "songwriter",
            ],
        ) || Self::contains_any(
            &normalized_extract,
            &[
                "musician", "singer", "band", "rapper", "composer", "producer",
            ],
        ) {
            score += 25;
        }

        score
    }

    fn score_album_summary(album: &str, album_artist: &str, summary: &WikiSummary) -> i32 {
        let normalized_album = Self::normalize_text(album);
        let normalized_artist = Self::normalize_text(album_artist);
        let normalized_title = Self::normalize_text(&summary.title);
        let normalized_description = Self::normalize_text(&summary.description);
        let normalized_extract = Self::normalize_text(&summary.extract);

        if normalized_description.contains("disambiguation") {
            return -10_000;
        }

        let mut score = 0;
        if normalized_title == normalized_album {
            score += 90;
        } else if normalized_title.starts_with(&normalized_album)
            || normalized_album.starts_with(&normalized_title)
        {
            score += 55;
        } else if normalized_title.contains(&normalized_album) {
            score += 35;
        }

        if Self::contains_any(
            &normalized_description,
            &["album", "ep", "mixtape", "soundtrack", "record"],
        ) {
            score += 25;
        }

        if !normalized_artist.is_empty()
            && (normalized_extract.contains(&normalized_artist)
                || normalized_description.contains(&normalized_artist)
                || normalized_title.contains(&normalized_artist))
        {
            score += 20;
        }

        score
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

    fn search_wikipedia_titles(&self, query: &str) -> Result<Vec<String>, String> {
        let encoded_query = urlencoding::encode(query);
        let url = format!(
            "{}?action=query&list=search&srsearch={}&srlimit={}&format=json&utf8=1",
            WIKIPEDIA_API_SEARCH_URL, encoded_query, MAX_CANDIDATES
        );
        let response = self
            .http_client
            .get(&url)
            .call()
            .map_err(|error| format!("Wikipedia search request failed: {error}"))?;
        let mut body = String::new();
        response
            .into_reader()
            .read_to_string(&mut body)
            .map_err(|error| format!("Failed to read search response: {error}"))?;
        let parsed: Value =
            serde_json::from_str(&body).map_err(|error| format!("Invalid search JSON: {error}"))?;
        let mut titles = Vec::new();
        if let Some(results) = parsed["query"]["search"].as_array() {
            for item in results {
                if let Some(title) = item["title"].as_str() {
                    let trimmed = title.trim();
                    if !trimmed.is_empty() {
                        titles.push(trimmed.to_string());
                    }
                }
            }
        }
        Ok(titles)
    }

    fn fetch_wikipedia_summary(&self, title: &str) -> Result<WikiSummary, String> {
        let encoded_title = urlencoding::encode(title);
        let url = format!("{}/{}", WIKIPEDIA_REST_SUMMARY_URL, encoded_title);
        let response = self
            .http_client
            .get(&url)
            .call()
            .map_err(|error| format!("Wikipedia summary request failed: {error}"))?;
        let mut body = String::new();
        response
            .into_reader()
            .read_to_string(&mut body)
            .map_err(|error| format!("Failed to read summary response: {error}"))?;
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|error| format!("Invalid summary JSON for '{title}': {error}"))?;
        let canonical_url = parsed["content_urls"]["desktop"]["page"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let image_url = parsed["thumbnail"]["source"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                parsed["originalimage"]["source"]
                    .as_str()
                    .map(str::to_string)
            });
        Ok(WikiSummary {
            title: parsed["title"].as_str().unwrap_or(title).to_string(),
            description: parsed["description"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            extract: parsed["extract"].as_str().unwrap_or_default().to_string(),
            canonical_url,
            image_url,
        })
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

    fn image_file_extension(url: &str) -> &'static str {
        let lower = url.to_ascii_lowercase();
        if lower.contains(".png") {
            "png"
        } else if lower.contains(".webp") {
            "webp"
        } else if lower.contains(".jpeg") {
            "jpeg"
        } else {
            "jpg"
        }
    }

    fn download_artist_image(
        &self,
        entity: &LibraryEnrichmentEntity,
        image_url: &str,
    ) -> Option<PathBuf> {
        let cache_dir = Self::image_cache_dir()?;
        let mut hasher = DefaultHasher::new();
        Self::source_entity_label(entity).hash(&mut hasher);
        image_url.hash(&mut hasher);
        let extension = Self::image_file_extension(image_url);
        let image_path = cache_dir.join(format!("{:x}.{extension}", hasher.finish()));
        if image_path.exists() {
            return Some(image_path);
        }

        let response = self.http_client.get(image_url).call().ok()?;
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).ok()?;
        if bytes.is_empty() {
            return None;
        }
        if fs::write(&image_path, bytes).is_err() {
            return None;
        }
        Some(image_path)
    }

    fn fetch_outcome_for_entity(&self, entity: &LibraryEnrichmentEntity) -> FetchOutcome {
        let query = match entity {
            LibraryEnrichmentEntity::Artist { artist } => format!("{artist} musician"),
            LibraryEnrichmentEntity::Album {
                album,
                album_artist,
            } => format!("{album} {album_artist} album"),
        };
        let titles = match self.search_wikipedia_titles(&query) {
            Ok(titles) => titles,
            Err(error) => {
                return FetchOutcome {
                    payload: LibraryEnrichmentPayload {
                        entity: entity.clone(),
                        status: LibraryEnrichmentStatus::Error,
                        blurb: String::new(),
                        image_path: None,
                        source_name: SOURCE_NAME.to_string(),
                        source_url: String::new(),
                    },
                    image_url: None,
                    last_error: Some(error),
                };
            }
        };
        if titles.is_empty() {
            return FetchOutcome {
                payload: LibraryEnrichmentPayload {
                    entity: entity.clone(),
                    status: LibraryEnrichmentStatus::NotFound,
                    blurb: String::new(),
                    image_path: None,
                    source_name: SOURCE_NAME.to_string(),
                    source_url: String::new(),
                },
                image_url: None,
                last_error: None,
            };
        }

        let mut best_summary: Option<WikiSummary> = None;
        let mut best_score = -10_001;

        for title in titles {
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
            }
        }

        let threshold = match entity {
            LibraryEnrichmentEntity::Artist { .. } => 85,
            LibraryEnrichmentEntity::Album { .. } => 75,
        };
        let Some(best_summary) = best_summary else {
            return FetchOutcome {
                payload: LibraryEnrichmentPayload {
                    entity: entity.clone(),
                    status: LibraryEnrichmentStatus::NotFound,
                    blurb: String::new(),
                    image_path: None,
                    source_name: SOURCE_NAME.to_string(),
                    source_url: String::new(),
                },
                image_url: None,
                last_error: None,
            };
        };
        if best_score < threshold {
            return FetchOutcome {
                payload: LibraryEnrichmentPayload {
                    entity: entity.clone(),
                    status: LibraryEnrichmentStatus::NotFound,
                    blurb: String::new(),
                    image_path: None,
                    source_name: SOURCE_NAME.to_string(),
                    source_url: best_summary.canonical_url,
                },
                image_url: None,
                last_error: None,
            };
        }

        let blurb = Self::truncate_blurb(&best_summary.extract, MAX_BLURB_CHARS);
        if blurb.trim().is_empty() {
            return FetchOutcome {
                payload: LibraryEnrichmentPayload {
                    entity: entity.clone(),
                    status: LibraryEnrichmentStatus::NotFound,
                    blurb: String::new(),
                    image_path: None,
                    source_name: SOURCE_NAME.to_string(),
                    source_url: best_summary.canonical_url,
                },
                image_url: None,
                last_error: None,
            };
        }

        let image_path = match entity {
            LibraryEnrichmentEntity::Artist { .. } => best_summary
                .image_url
                .as_ref()
                .and_then(|url| self.download_artist_image(entity, url)),
            LibraryEnrichmentEntity::Album { .. } => None,
        };

        FetchOutcome {
            payload: LibraryEnrichmentPayload {
                entity: entity.clone(),
                status: LibraryEnrichmentStatus::Ready,
                blurb,
                image_path,
                source_name: SOURCE_NAME.to_string(),
                source_url: best_summary.canonical_url,
            },
            image_url: best_summary.image_url,
            last_error: None,
        }
    }

    fn ttl_days_for_payload(&self, payload: &LibraryEnrichmentPayload) -> u32 {
        let ready_ttl_days = if matches!(payload.entity, LibraryEnrichmentEntity::Artist { .. }) {
            self.artist_image_cache_ttl_days.max(1)
        } else {
            READY_METADATA_TTL_DAYS
        };
        let status = payload.status;
        match status {
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

    fn handle_enrichment_request(
        &self,
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
                    source_name: SOURCE_NAME.to_string(),
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
            Ok(Some(cached)) => {
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

        let outcome = self.fetch_outcome_for_entity(&entity);
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
            match self.bus_consumer.blocking_recv() {
                Ok(Message::Config(crate::protocol::ConfigMessage::ConfigChanged(config))) => {
                    self.online_metadata_enabled = config.library.online_metadata_enabled;
                    self.artist_image_cache_ttl_days = config.library.artist_image_cache_ttl_days;
                    self.artist_image_cache_max_size_mb =
                        config.library.artist_image_cache_max_size_mb;
                    self.prune_enrichment_cache();
                    self.prune_artist_image_cache_by_size();
                }
                Ok(Message::Library(LibraryMessage::RequestEnrichment { entity, priority })) => {
                    debug!(
                        "Enrichment request received for {} ({:?})",
                        Self::source_entity_label(&entity),
                        priority
                    );
                    self.handle_enrichment_request(entity, priority);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LibraryEnrichmentManager, WikiSummary};

    #[test]
    fn test_normalize_text_collapses_symbols_and_case() {
        assert_eq!(
            LibraryEnrichmentManager::normalize_text("Daft-Punk / RANDOM Access!!"),
            "daft punk random access"
        );
    }

    #[test]
    fn test_score_artist_summary_prefers_music_entity_match() {
        let summary = WikiSummary {
            title: "Daft Punk".to_string(),
            description: "French electronic music duo".to_string(),
            extract: "Daft Punk were a French electronic music duo.".to_string(),
            canonical_url: String::new(),
            image_url: None,
        };
        assert!(LibraryEnrichmentManager::score_artist_summary("Daft Punk", &summary) >= 100);
    }

    #[test]
    fn test_score_album_summary_rewards_album_context() {
        let summary = WikiSummary {
            title: "Discovery".to_string(),
            description: "2001 studio album by Daft Punk".to_string(),
            extract: "Discovery is the second studio album by French duo Daft Punk.".to_string(),
            canonical_url: String::new(),
            image_url: None,
        };
        assert!(
            LibraryEnrichmentManager::score_album_summary("Discovery", "Daft Punk", &summary)
                >= 100
        );
    }
}
