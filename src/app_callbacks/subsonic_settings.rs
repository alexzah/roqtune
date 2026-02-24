//! Callback registration for OpenSubsonic settings and session credential flows.

use log::warn;

use crate::{
    app_config_coordinator::apply_config_update,
    app_context::AppSharedState,
    integration_keyring::set_opensubsonic_password,
    opensubsonic_controller::{
        find_opensubsonic_backend, keyring_unavailable_error, opensubsonic_profile_snapshot,
        resolve_opensubsonic_password, upsert_opensubsonic_backend_config,
        OpenSubsonicPasswordResolution, OPENSUBSONIC_PROFILE_ID,
        OPENSUBSONIC_SESSION_KEYRING_NOTICE,
    },
    protocol::{self, IntegrationMessage, Message},
    AppWindow,
};

/// Registers settings callbacks for saving/testing/syncing OpenSubsonic integration state.
pub(crate) fn register_subsonic_settings_callbacks(ui: &AppWindow, shared_state: &AppSharedState) {
    let shared_state_clone = shared_state.clone();
    ui.on_settings_save_subsonic_profile(move |enabled, endpoint, username, password| {
        let endpoint_trimmed = endpoint.trim().trim_end_matches('/').to_string();
        let username_trimmed = username.trim().to_string();
        let password_trimmed = password.trim().to_string();

        let mut status_message = "OpenSubsonic profile saved".to_string();
        let mut show_keyring_notice = false;
        let mut keyring_notice_message = String::new();
        if !password_trimmed.is_empty() {
            {
                let mut session_passwords = shared_state_clone
                    .opensubsonic_session_passwords
                    .lock()
                    .expect("session password cache lock poisoned");
                session_passwords.insert(
                    OPENSUBSONIC_PROFILE_ID.to_string(),
                    password_trimmed.clone(),
                );
            }
            if let Err(error) =
                set_opensubsonic_password(OPENSUBSONIC_PROFILE_ID, password_trimmed.as_str())
            {
                warn!(
                    "Failed to save OpenSubsonic credential for profile '{}': {}",
                    OPENSUBSONIC_PROFILE_ID, error
                );
                status_message =
                    "System keyring unavailable; password cached for this session only".to_string();
                if keyring_unavailable_error(error.as_str()) {
                    show_keyring_notice = true;
                    keyring_notice_message = OPENSUBSONIC_SESSION_KEYRING_NOTICE.to_string();
                }
            }
        }

        let next_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            upsert_opensubsonic_backend_config(
                &mut next,
                endpoint_trimmed.as_str(),
                username_trimmed.as_str(),
                enabled,
            );
            crate::sanitize_config(next)
        };
        apply_config_update(&shared_state_clone, next_config.clone(), true);

        let password_for_upsert = if !password_trimmed.is_empty() {
            Some(password_trimmed)
        } else {
            match resolve_opensubsonic_password(
                OPENSUBSONIC_PROFILE_ID,
                &shared_state_clone.opensubsonic_session_passwords,
            ) {
                OpenSubsonicPasswordResolution::Saved(password) => Some(password),
                OpenSubsonicPasswordResolution::SessionOnly(password) => {
                    status_message =
                        "Using session-only OpenSubsonic credential (not saved)".to_string();
                    Some(password)
                }
                OpenSubsonicPasswordResolution::Missing => None,
                OpenSubsonicPasswordResolution::KeyringError(error) => {
                    warn!(
                        "Failed to load OpenSubsonic credential for profile '{}': {}",
                        OPENSUBSONIC_PROFILE_ID, error
                    );
                    status_message =
                        "Could not read saved OpenSubsonic credential from the system keyring"
                            .to_string();
                    if keyring_unavailable_error(error.as_str()) {
                        show_keyring_notice = true;
                        keyring_notice_message = OPENSUBSONIC_SESSION_KEYRING_NOTICE.to_string();
                    }
                    None
                }
            }
        };

        if let Some(backend) = find_opensubsonic_backend(&next_config) {
            let snapshot = opensubsonic_profile_snapshot(backend, Some(status_message.clone()));
            let connect_now = enabled && password_for_upsert.is_some();
            let _ = shared_state_clone.bus_sender.send(Message::Integration(
                IntegrationMessage::UpsertBackendProfile {
                    profile: snapshot,
                    password: password_for_upsert,
                    connect_now,
                },
            ));
            if !enabled {
                let _ = shared_state_clone.bus_sender.send(Message::Integration(
                    IntegrationMessage::DisconnectBackendProfile {
                        profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
                    },
                ));
            }
        }

        if let Some(ui) = shared_state_clone.ui_handles.ui_handle.upgrade() {
            ui.set_settings_subsonic_status(status_message.into());
            ui.set_settings_subsonic_password("".into());
            if show_keyring_notice {
                ui.set_subsonic_keyring_notice_message(keyring_notice_message.into());
                ui.set_show_subsonic_keyring_notice(true);
            }
        }
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    let opensubsonic_session_passwords_clone = shared_state.opensubsonic_session_passwords.clone();
    ui.on_settings_test_subsonic_connection(move || {
        let Some(ui) = ui_handle_clone.upgrade() else {
            return;
        };

        let endpoint_trimmed = ui
            .get_settings_subsonic_endpoint()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let username_trimmed = ui.get_settings_subsonic_username().trim().to_string();
        let password_trimmed = ui.get_settings_subsonic_password().trim().to_string();
        if endpoint_trimmed.is_empty() || username_trimmed.is_empty() {
            ui.set_settings_subsonic_status("Enter server URL and username first".into());
            return;
        }

        let password_for_upsert = if password_trimmed.is_empty() {
            match resolve_opensubsonic_password(
                OPENSUBSONIC_PROFILE_ID,
                &opensubsonic_session_passwords_clone,
            ) {
                OpenSubsonicPasswordResolution::Saved(password) => Some(password),
                OpenSubsonicPasswordResolution::SessionOnly(password) => {
                    ui.set_settings_subsonic_status(
                        "Testing connection with session-only credential".into(),
                    );
                    Some(password)
                }
                OpenSubsonicPasswordResolution::Missing => None,
                OpenSubsonicPasswordResolution::KeyringError(error) => {
                    warn!(
                        "Failed to load OpenSubsonic credential for profile '{}': {}",
                        OPENSUBSONIC_PROFILE_ID, error
                    );
                    ui.set_settings_subsonic_status(
                        "System keyring unavailable. Enter password to test for this session only."
                            .into(),
                    );
                    return;
                }
            }
        } else {
            {
                let mut session_passwords = opensubsonic_session_passwords_clone
                    .lock()
                    .expect("session password cache lock poisoned");
                session_passwords.insert(
                    OPENSUBSONIC_PROFILE_ID.to_string(),
                    password_trimmed.clone(),
                );
            }
            Some(password_trimmed)
        };
        let profile = protocol::BackendProfileSnapshot {
            profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            backend_kind: protocol::BackendKind::OpenSubsonic,
            display_name: "OpenSubsonic".to_string(),
            endpoint: endpoint_trimmed,
            username: username_trimmed,
            configured: true,
            connection_state: protocol::BackendConnectionState::Disconnected,
            status_text: Some("Testing connection...".to_string()),
        };
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::UpsertBackendProfile {
                profile,
                password: password_for_upsert,
                connect_now: false,
            },
        ));
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::TestBackendConnection {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
        ui.set_settings_subsonic_status("Testing connection...".into());
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_settings_sync_subsonic_now(move || {
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::SyncBackendProfile {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let opensubsonic_session_passwords_clone = shared_state.opensubsonic_session_passwords.clone();
    ui.on_settings_disconnect_subsonic(move || {
        let mut session_passwords = opensubsonic_session_passwords_clone
            .lock()
            .expect("session password cache lock poisoned");
        session_passwords.remove(OPENSUBSONIC_PROFILE_ID);
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::DisconnectBackendProfile {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let config_state_clone = shared_state.config_state.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    let opensubsonic_session_passwords_clone = shared_state.opensubsonic_session_passwords.clone();
    ui.on_subsonic_session_password_submit(move |password| {
        let Some(ui) = ui_handle_clone.upgrade() else {
            return;
        };
        let password_trimmed = password.trim().to_string();
        if password_trimmed.is_empty() {
            ui.set_subsonic_session_prompt_status("Enter password to continue".into());
            return;
        }

        let backend = {
            let state = config_state_clone
                .lock()
                .expect("config state lock poisoned");
            find_opensubsonic_backend(&state).cloned()
        };
        let Some(backend) = backend else {
            ui.set_subsonic_session_prompt_status(
                "OpenSubsonic profile is not configured in settings".into(),
            );
            return;
        };

        {
            let mut session_passwords = opensubsonic_session_passwords_clone
                .lock()
                .expect("session password cache lock poisoned");
            session_passwords.insert(
                OPENSUBSONIC_PROFILE_ID.to_string(),
                password_trimmed.clone(),
            );
        }

        let status_message = "Using session-only OpenSubsonic credential (not saved)";
        let snapshot = opensubsonic_profile_snapshot(&backend, Some(status_message.to_string()));
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::UpsertBackendProfile {
                profile: snapshot,
                password: Some(password_trimmed),
                connect_now: backend.enabled,
            },
        ));

        ui.set_settings_subsonic_status(status_message.into());
        ui.set_settings_subsonic_password("".into());
        ui.set_subsonic_session_prompt_password("".into());
        ui.set_subsonic_session_prompt_status("".into());
        ui.set_show_subsonic_session_password_prompt(false);
        ui.invoke_refocus_main();
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_subsonic_session_password_cancel(move || {
        let _ = bus_sender_clone.send(Message::Integration(
            IntegrationMessage::DisconnectBackendProfile {
                profile_id: OPENSUBSONIC_PROFILE_ID.to_string(),
            },
        ));
        let Some(ui) = ui_handle_clone.upgrade() else {
            return;
        };
        ui.set_subsonic_session_prompt_password("".into());
        ui.set_subsonic_session_prompt_status("Session credential prompt dismissed".into());
        ui.set_show_subsonic_session_password_prompt(false);
        ui.set_settings_subsonic_status(
            "OpenSubsonic disconnected (session password not provided)".into(),
        );
        ui.invoke_refocus_main();
    });
}
