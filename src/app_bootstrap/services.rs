use std::{
    any::Any,
    sync::mpsc::{Receiver, SyncSender},
    thread,
};

use tokio::sync::broadcast;

use crate::{
    audio_decoder::AudioDecoder,
    audio_player::AudioPlayer,
    cast_manager::CastManager,
    db_manager::DbManager,
    integration_manager::IntegrationManager,
    library_enrichment_manager::LibraryEnrichmentManager,
    library_manager::LibraryManager,
    media_controls_manager::MediaControlsManager,
    metadata_manager::MetadataManager,
    playlist::Playlist,
    playlist_manager::PlaylistManager,
    protocol::{self, IntegrationMessage, Message},
    ui_manager::UiManager,
    AppWindow,
};

pub struct BackgroundServicesConfig {
    pub bus_sender: broadcast::Sender<Message>,
    pub ui_handle: slint::Weak<AppWindow>,
    pub initial_online_metadata_enabled: bool,
    pub initial_online_metadata_prompt_pending: bool,
    pub playlist_bulk_import_rx: Receiver<protocol::PlaylistBulkImportRequest>,
    pub library_scan_progress_tx: SyncSender<protocol::LibraryMessage>,
    pub library_scan_progress_rx: Receiver<protocol::LibraryMessage>,
    pub startup_opensubsonic_seed: Option<(protocol::BackendProfileSnapshot, Option<String>, bool)>,
}

fn panic_payload_to_string(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "non-string panic payload".to_string()
}

pub fn spawn_background_services(config: BackgroundServicesConfig) {
    let BackgroundServicesConfig {
        bus_sender,
        ui_handle,
        initial_online_metadata_enabled,
        initial_online_metadata_prompt_pending,
        playlist_bulk_import_rx,
        library_scan_progress_tx,
        library_scan_progress_rx,
        startup_opensubsonic_seed,
    } = config;

    let integration_manager_bus_receiver = bus_sender.subscribe();
    let integration_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut integration_manager = IntegrationManager::new(
            integration_manager_bus_receiver,
            integration_manager_bus_sender,
        );
        integration_manager.run();
    });

    let playlist_manager_bus_receiver = bus_sender.subscribe();
    let playlist_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut playlist_manager = PlaylistManager::new(
            Playlist::new(),
            playlist_manager_bus_receiver,
            playlist_manager_bus_sender,
            db_manager,
            playlist_bulk_import_rx,
        );
        playlist_manager.run();
    });

    let library_manager_bus_receiver = bus_sender.subscribe();
    let library_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut library_manager = LibraryManager::new(
            library_manager_bus_receiver,
            library_manager_bus_sender,
            db_manager,
            library_scan_progress_tx,
        );
        library_manager.run();
    });

    let enrichment_manager_bus_receiver = bus_sender.subscribe();
    let enrichment_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut enrichment_manager = LibraryEnrichmentManager::new(
            enrichment_manager_bus_receiver,
            enrichment_manager_bus_sender,
            db_manager,
        );
        enrichment_manager.run();
    });

    let metadata_manager_bus_receiver = bus_sender.subscribe();
    let metadata_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let db_manager = DbManager::new().expect("Failed to initialize database");
        let mut metadata_manager = MetadataManager::new(
            metadata_manager_bus_receiver,
            metadata_manager_bus_sender,
            db_manager,
        );
        metadata_manager.run();
    });

    let media_controls_bus_receiver = bus_sender.subscribe();
    let media_controls_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut media_controls_manager =
            MediaControlsManager::new(media_controls_bus_receiver, media_controls_bus_sender);
        media_controls_manager.run();
    });

    let cast_manager_bus_receiver = bus_sender.subscribe();
    let cast_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut cast_manager = CastManager::new(cast_manager_bus_receiver, cast_manager_bus_sender);
        cast_manager.run();
    });

    let ui_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ui_manager = UiManager::new(
                ui_handle,
                ui_manager_bus_sender.subscribe(),
                ui_manager_bus_sender.clone(),
                initial_online_metadata_enabled,
                initial_online_metadata_prompt_pending,
                library_scan_progress_rx,
            );
            ui_manager.run();
        }));
        if let Err(payload) = run_result {
            log::error!(
                "UiManager thread terminated due to panic: {}",
                panic_payload_to_string(payload.as_ref())
            );
        }
    });

    let decoder_bus_sender = bus_sender.clone();
    let decoder_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_decoder = AudioDecoder::new(decoder_bus_receiver, decoder_bus_sender);
        audio_decoder.run();
    });

    if let Some((snapshot, password, connect_now)) = startup_opensubsonic_seed {
        let _ = bus_sender.send(Message::Integration(
            IntegrationMessage::UpsertBackendProfile {
                profile: snapshot,
                password,
                connect_now,
            },
        ));
    }

    let player_bus_sender = bus_sender.clone();
    let player_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_player = AudioPlayer::new(player_bus_receiver, player_bus_sender);
        audio_player.run();
    });
}
