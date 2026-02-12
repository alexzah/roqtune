//! Output-device probing and cache support.
//!
//! This module verifies practical output stream compatibility for common sample
//! rates by opening short-lived CPAL streams, then caches successful rates per
//! device fingerprint.

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cpal::traits::{DeviceTrait, StreamTrait};
use log::{debug, warn};

const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;
const COMMON_SAMPLE_RATES_HZ: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct DeviceFingerprint {
    pub host_id: String,
    pub device_name: String,
    pub supported_config_signature: String,
}

#[derive(
    Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum ProbeSampleFormat {
    F32,
    I16,
    U16,
    Other,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ProbeResult {
    pub verified_sample_rates: Vec<u32>,
    pub verified_channels: Vec<u16>,
    pub verified_formats: Vec<ProbeSampleFormat>,
    pub probed_at_unix_secs: u64,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct ProbeCacheEntry {
    fingerprint: DeviceFingerprint,
    result: ProbeResult,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct ProbeCacheFile {
    schema_version: u32,
    entries: Vec<ProbeCacheEntry>,
}

impl Default for ProbeCacheFile {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl ProbeCacheFile {
    fn load(path: &Path) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        let parsed: Self = match serde_json::from_str(&contents) {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!(
                    "AudioProbe: Failed parsing probe cache at {}: {}",
                    path.display(),
                    err
                );
                return Self::default();
            }
        };
        if parsed.schema_version != CACHE_SCHEMA_VERSION {
            return Self::default();
        }
        parsed
    }

    fn save(&self, path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        if let Err(err) = fs::create_dir_all(parent) {
            warn!(
                "AudioProbe: Failed creating probe cache directory {}: {}",
                parent.display(),
                err
            );
            return;
        }
        let serialized = match serde_json::to_string_pretty(self) {
            Ok(serialized) => serialized,
            Err(err) => {
                warn!("AudioProbe: Failed serializing probe cache: {}", err);
                return;
            }
        };
        if let Err(err) = fs::write(path, serialized) {
            warn!(
                "AudioProbe: Failed writing probe cache {}: {}",
                path.display(),
                err
            );
        }
    }

    fn get_valid_result(
        &self,
        fingerprint: &DeviceFingerprint,
        now_unix_secs: u64,
    ) -> Option<ProbeResult> {
        let entry = self
            .entries
            .iter()
            .find(|entry| &entry.fingerprint == fingerprint)?;
        if now_unix_secs
            .saturating_sub(entry.result.probed_at_unix_secs)
            .gt(&CACHE_STALE_AFTER_SECS)
        {
            return None;
        }
        Some(entry.result.clone())
    }

    fn upsert_result(&mut self, fingerprint: DeviceFingerprint, result: ProbeResult) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.fingerprint == fingerprint)
        {
            existing.result = result;
            return;
        }
        self.entries.push(ProbeCacheEntry {
            fingerprint,
            result,
        });
    }
}

pub fn probe_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("roqtune")
        .join("output_probe_cache.json")
}

pub fn get_or_probe_output_device(
    host: &cpal::Host,
    device: &cpal::Device,
    configured_sample_rate_hz: u32,
) -> ProbeResult {
    let now_unix_secs = current_unix_secs();
    let fingerprint = device_fingerprint(host, device);
    let cache_path = probe_cache_path();
    let mut cache = ProbeCacheFile::load(&cache_path);

    if let Some(result) = cache.get_valid_result(&fingerprint, now_unix_secs) {
        debug!(
            "AudioProbe: cache hit for '{}' with rates {:?}",
            fingerprint.device_name, result.verified_sample_rates
        );
        return result;
    }

    debug!(
        "AudioProbe: probing output rates for device '{}'",
        fingerprint.device_name
    );
    let result = probe_output_device(device, configured_sample_rate_hz, now_unix_secs);
    cache.upsert_result(fingerprint, result.clone());
    cache.save(&cache_path);
    result
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn device_fingerprint(host: &cpal::Host, device: &cpal::Device) -> DeviceFingerprint {
    let host_id = format!("{:?}", host.id());
    let device_name = device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "Unknown Device".to_string());
    let mut signature_parts = Vec::new();
    if let Ok(configs) = device.supported_output_configs() {
        for range in configs {
            signature_parts.push(format!(
                "{}:{:?}:{}:{}",
                range.channels(),
                range.sample_format(),
                range.min_sample_rate(),
                range.max_sample_rate()
            ));
        }
    }
    signature_parts.sort_unstable();
    DeviceFingerprint {
        host_id,
        device_name,
        supported_config_signature: signature_parts.join("|"),
    }
}

fn probe_output_device(
    device: &cpal::Device,
    configured_sample_rate_hz: u32,
    probed_at_unix_secs: u64,
) -> ProbeResult {
    let mut candidate_rates = BTreeSet::from(COMMON_SAMPLE_RATES_HZ);
    candidate_rates.insert(configured_sample_rate_hz.clamp(8_000, 192_000));

    let mut configs_by_rate: HashMap<u32, Vec<cpal::SupportedStreamConfig>> = HashMap::new();
    if let Ok(configs) = device.supported_output_configs() {
        for range in configs {
            for rate in &candidate_rates {
                if *rate >= range.min_sample_rate() && *rate <= range.max_sample_rate() {
                    configs_by_rate
                        .entry(*rate)
                        .or_default()
                        .push(range.with_sample_rate(*rate));
                }
            }
        }
    }

    let mut verified_sample_rates = BTreeSet::new();
    let mut verified_channels = BTreeSet::new();
    let mut verified_formats = BTreeSet::new();

    for rate in candidate_rates {
        let mut candidates = configs_by_rate.remove(&rate).unwrap_or_default();
        candidates.sort_by_key(stream_probe_sort_key);
        for config in candidates {
            if probe_stream_config(device, &config) {
                verified_sample_rates.insert(rate);
                verified_channels.insert(config.channels());
                verified_formats.insert(probe_sample_format(config.sample_format()));
                break;
            }
        }
    }

    ProbeResult {
        verified_sample_rates: verified_sample_rates.into_iter().collect(),
        verified_channels: verified_channels.into_iter().collect(),
        verified_formats: verified_formats.into_iter().collect(),
        probed_at_unix_secs,
    }
}

fn stream_probe_sort_key(config: &cpal::SupportedStreamConfig) -> (u16, u8, u16) {
    let channel_penalty = config.channels().abs_diff(2);
    let format_penalty = match config.sample_format() {
        cpal::SampleFormat::F32 => 0,
        cpal::SampleFormat::I16 => 1,
        cpal::SampleFormat::U16 => 2,
        _ => 3,
    };
    let bits = (config.sample_format().sample_size() * 8) as u16;
    (channel_penalty, format_penalty, bits.abs_diff(24))
}

fn probe_sample_format(sample_format: cpal::SampleFormat) -> ProbeSampleFormat {
    match sample_format {
        cpal::SampleFormat::F32 => ProbeSampleFormat::F32,
        cpal::SampleFormat::I16 => ProbeSampleFormat::I16,
        cpal::SampleFormat::U16 => ProbeSampleFormat::U16,
        _ => ProbeSampleFormat::Other,
    }
}

fn probe_stream_config(device: &cpal::Device, config: &cpal::SupportedStreamConfig) -> bool {
    let stream_config = config.config();
    let callback_observed = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicBool::new(false));
    let callback_observed_for_error = Arc::clone(&callback_observed);
    let stream_error_for_error = Arc::clone(&stream_error);

    let stream_result = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let callback_observed = Arc::clone(&callback_observed);
            let stream_error = Arc::clone(&stream_error);
            device.build_output_stream(
                &stream_config,
                move |output: &mut [f32], _| {
                    callback_observed.store(true, Ordering::Relaxed);
                    output.fill(0.0);
                },
                move |_| {
                    stream_error.store(true, Ordering::Relaxed);
                },
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let callback_observed = Arc::clone(&callback_observed);
            let stream_error = Arc::clone(&stream_error);
            device.build_output_stream(
                &stream_config,
                move |output: &mut [i16], _| {
                    callback_observed.store(true, Ordering::Relaxed);
                    output.fill(0);
                },
                move |_| {
                    stream_error.store(true, Ordering::Relaxed);
                },
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let callback_observed = Arc::clone(&callback_observed);
            let stream_error = Arc::clone(&stream_error);
            device.build_output_stream(
                &stream_config,
                move |output: &mut [u16], _| {
                    callback_observed.store(true, Ordering::Relaxed);
                    output.fill(u16::MAX / 2 + 1);
                },
                move |_| {
                    stream_error.store(true, Ordering::Relaxed);
                },
                None,
            )
        }
        _ => return false,
    };

    let Ok(stream) = stream_result else {
        return false;
    };
    if stream.play().is_err() {
        return false;
    }
    std::thread::sleep(Duration::from_millis(140));

    let callback_seen = callback_observed_for_error.load(Ordering::Relaxed);
    let saw_error = stream_error_for_error.load(Ordering::Relaxed);
    callback_seen && !saw_error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "roqtune_probe_cache_{}_{}_{}.json",
            test_name,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn test_probe_cache_roundtrip() {
        let cache_path = unique_temp_path("roundtrip");
        let mut cache = ProbeCacheFile::default();
        let fingerprint = DeviceFingerprint {
            host_id: "HostA".to_string(),
            device_name: "DeviceA".to_string(),
            supported_config_signature: "sig".to_string(),
        };
        let result = ProbeResult {
            verified_sample_rates: vec![44_100, 48_000, 96_000],
            verified_channels: vec![2],
            verified_formats: vec![ProbeSampleFormat::F32],
            probed_at_unix_secs: 1_000,
        };
        cache.upsert_result(fingerprint.clone(), result.clone());
        cache.save(&cache_path);

        let loaded = ProbeCacheFile::load(&cache_path);
        let retrieved = loaded
            .get_valid_result(&fingerprint, 1_001)
            .expect("cached result should be readable");
        assert_eq!(retrieved, result);

        let _ = fs::remove_file(cache_path);
    }

    #[test]
    fn test_probe_cache_stale_entries_are_rejected() {
        let mut cache = ProbeCacheFile::default();
        let fingerprint = DeviceFingerprint {
            host_id: "HostB".to_string(),
            device_name: "DeviceB".to_string(),
            supported_config_signature: "sig".to_string(),
        };
        cache.upsert_result(
            fingerprint.clone(),
            ProbeResult {
                verified_sample_rates: vec![48_000],
                verified_channels: vec![2],
                verified_formats: vec![ProbeSampleFormat::I16],
                probed_at_unix_secs: 100,
            },
        );

        assert!(cache.get_valid_result(&fingerprint, 100).is_some());
        assert!(cache
            .get_valid_result(&fingerprint, 100 + CACHE_STALE_AFTER_SECS + 1)
            .is_none());
    }
}
