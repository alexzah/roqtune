//! OpenSubsonic-specific config, credentials, and profile snapshot helpers.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use log::warn;

use crate::{
    config::{BackendProfileConfig, Config, IntegrationBackendKind},
    integration_keyring::get_opensubsonic_password,
    protocol,
};

/// Stable profile ID used for the built-in OpenSubsonic integration profile.
pub const OPENSUBSONIC_PROFILE_ID: &str = "opensubsonic-default";
/// User-facing notice shown when secure keyring storage is unavailable.
pub const OPENSUBSONIC_SESSION_KEYRING_NOTICE: &str = "System keyring is not available. roqtune will keep your OpenSubsonic password only for this session and ask again after restart.";

#[derive(Debug)]
/// Result of resolving OpenSubsonic credentials from keyring/session state.
pub enum OpenSubsonicPasswordResolution {
    /// Password loaded from secure keyring storage.
    Saved(String),
    /// Password loaded from in-memory session cache.
    SessionOnly(String),
    /// No password was available from keyring or session cache.
    Missing,
    /// Keyring access failed and no session fallback was available.
    KeyringError(String),
}

/// Returns `true` when an error string indicates keyring service unavailability.
pub fn keyring_unavailable_error(error: &str) -> bool {
    error.contains("Platform secure storage failure")
        || error.contains("org.freedesktop.DBus.Error.ServiceUnknown")
        || error.contains("failed to create keyring entry")
}

fn session_cached_opensubsonic_password(
    session_passwords: &Arc<Mutex<HashMap<String, String>>>,
    profile_id: &str,
) -> Option<String> {
    let cache = session_passwords
        .lock()
        .expect("session password cache lock poisoned");
    cache.get(profile_id).cloned()
}

/// Resolves OpenSubsonic credentials from keyring with session-cache fallback.
pub fn resolve_opensubsonic_password(
    profile_id: &str,
    session_passwords: &Arc<Mutex<HashMap<String, String>>>,
) -> OpenSubsonicPasswordResolution {
    match get_opensubsonic_password(profile_id) {
        Ok(Some(password)) => OpenSubsonicPasswordResolution::Saved(password),
        Ok(None) => {
            if let Some(password) =
                session_cached_opensubsonic_password(session_passwords, profile_id)
            {
                OpenSubsonicPasswordResolution::SessionOnly(password)
            } else {
                OpenSubsonicPasswordResolution::Missing
            }
        }
        Err(error) => {
            if let Some(password) =
                session_cached_opensubsonic_password(session_passwords, profile_id)
            {
                warn!(
                    "Falling back to session-only OpenSubsonic credential for profile '{}': {}",
                    profile_id, error
                );
                OpenSubsonicPasswordResolution::SessionOnly(password)
            } else {
                OpenSubsonicPasswordResolution::KeyringError(error)
            }
        }
    }
}

/// Returns the configured OpenSubsonic backend profile, if present.
pub fn find_opensubsonic_backend(config: &Config) -> Option<&BackendProfileConfig> {
    config
        .integrations
        .backends
        .iter()
        .find(|backend| backend.profile_id == OPENSUBSONIC_PROFILE_ID)
}

/// Inserts or updates the OpenSubsonic backend entry in config.
pub fn upsert_opensubsonic_backend_config(
    config: &mut Config,
    endpoint: &str,
    username: &str,
    enabled: bool,
) {
    let endpoint = endpoint.trim().trim_end_matches('/').to_string();
    let username = username.trim().to_string();
    if let Some(existing) = config
        .integrations
        .backends
        .iter_mut()
        .find(|backend| backend.profile_id == OPENSUBSONIC_PROFILE_ID)
    {
        existing.backend_kind = IntegrationBackendKind::OpenSubsonic;
        existing.display_name = "OpenSubsonic".to_string();
        existing.endpoint = endpoint;
        existing.username = username;
        existing.enabled = enabled;
        return;
    }
    config.integrations.backends.push(BackendProfileConfig {
        profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
        backend_kind: IntegrationBackendKind::OpenSubsonic,
        display_name: "OpenSubsonic".to_string(),
        endpoint,
        username,
        enabled,
    });
}

/// Converts config-backed OpenSubsonic profile data into a runtime snapshot.
pub fn opensubsonic_profile_snapshot(
    config_backend: &BackendProfileConfig,
    status_text: Option<String>,
) -> protocol::BackendProfileSnapshot {
    protocol::BackendProfileSnapshot {
        profile_id: config_backend.profile_id.clone(),
        backend_kind: protocol::BackendKind::OpenSubsonic,
        display_name: config_backend.display_name.clone(),
        endpoint: config_backend.endpoint.clone(),
        username: config_backend.username.clone(),
        configured: !config_backend.endpoint.trim().is_empty()
            && !config_backend.username.trim().is_empty(),
        connection_state: protocol::BackendConnectionState::Disconnected,
        status_text,
    }
}
