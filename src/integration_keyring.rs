//! Keyring helpers for integration backend credentials.

use keyring::Entry;

const OPENSUBSONIC_SERVICE_NAME: &str = "roqtune.backend.opensubsonic";

fn opensubsonic_entry(profile_id: &str) -> Result<Entry, String> {
    Entry::new(OPENSUBSONIC_SERVICE_NAME, profile_id)
        .map_err(|err| format!("failed to create keyring entry: {err}"))
}

/// Saves the OpenSubsonic password for a profile into the OS keyring.
pub fn set_opensubsonic_password(profile_id: &str, password: &str) -> Result<(), String> {
    let entry = opensubsonic_entry(profile_id)?;
    entry
        .set_password(password)
        .map_err(|err| format!("failed to set keyring password: {err}"))
}

/// Loads the OpenSubsonic password for a profile from the OS keyring.
pub fn get_opensubsonic_password(profile_id: &str) -> Result<Option<String>, String> {
    let entry = opensubsonic_entry(profile_id)?;
    match entry.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(format!("failed to get keyring password: {err}")),
    }
}

/// Deletes the OpenSubsonic password for a profile from the OS keyring.
#[allow(dead_code)]
pub fn delete_opensubsonic_password(profile_id: &str) -> Result<(), String> {
    let entry = opensubsonic_entry(profile_id)?;
    match entry.delete_password() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("failed to delete keyring password: {err}")),
    }
}
