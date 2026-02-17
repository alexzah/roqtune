//! Credential-storage helpers for integration backend secrets.

use keyring::Entry;

const OPENSUBSONIC_SERVICE_NAME: &str = "roqtune.backend.opensubsonic";

fn opensubsonic_entry(profile_id: &str) -> Result<Entry, String> {
    Entry::new(OPENSUBSONIC_SERVICE_NAME, profile_id)
        .map_err(|err| format!("failed to create keyring entry for profile '{profile_id}': {err}"))
}

fn keyring_error_hint(error: &str) -> Option<String> {
    if error.contains("org.freedesktop.DBus.Error.ServiceUnknown") {
        return Some(
            "no Secret Service provider is available. Start GNOME Keyring or KeePassXC Secret Service. In Flatpak, ensure the app has `--talk-name=org.freedesktop.secrets`."
                .to_string(),
        );
    }
    None
}

fn format_keyring_error(operation: &str, profile_id: &str, error: &str) -> String {
    let base = format!("{operation} failed in system keyring for profile '{profile_id}': {error}");
    match keyring_error_hint(error) {
        Some(hint) => format!("{base}. Hint: {hint}"),
        None => base,
    }
}

/// Saves the OpenSubsonic password for a profile into the OS keyring.
pub fn set_opensubsonic_password(profile_id: &str, password: &str) -> Result<(), String> {
    let entry = opensubsonic_entry(profile_id)?;
    entry.set_password(password).map_err(|err| {
        let detail = format!("failed to set keyring password: {err}");
        format_keyring_error("save OpenSubsonic credential", profile_id, detail.as_str())
    })
}

/// Loads the OpenSubsonic password for a profile from the OS keyring.
pub fn get_opensubsonic_password(profile_id: &str) -> Result<Option<String>, String> {
    let entry = opensubsonic_entry(profile_id)?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => {
            let detail = format!("failed to get keyring password: {err}");
            Err(format_keyring_error(
                "load OpenSubsonic credential",
                profile_id,
                detail.as_str(),
            ))
        }
    }
}
