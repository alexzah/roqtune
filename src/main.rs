mod audio_decoder;
mod audio_player;
mod playlist;
mod playlist_manager;
mod protocol;
mod ui_manager;

use std::{
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use audio_decoder::AudioDecoder;
use audio_player::AudioPlayer;
use log::{debug, info};
use playlist::Playlist;
use playlist_manager::PlaylistManager;
use protocol::{Config, ConfigMessage, Message, PlaybackMessage, PlaylistMessage};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::sync::broadcast;
use ui_manager::{UiManager, UiState};

slint::include_modules!();

fn setup_app_state_associations(ui: &AppWindow, ui_state: &UiState) {
    ui.set_track_model(ModelRc::from(ui_state.track_model.clone()));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut clog = colog::default_builder();
    clog.filter(None, log::LevelFilter::Debug);
    clog.init();

    // Setup ui state
    let ui = AppWindow::new()?;

    let ui_state = UiState {
        track_model: Rc::new(VecModel::from(vec![])),
    };

    let config_dir = dirs::config_dir().unwrap();
    let config_file = config_dir.join("music_player.toml");

    if !config_file.exists() {
        let default_config = toml::toml! {
            [output]
            channel_count = 2
        };

        info!(
            "Config file not found. Creating default config. path={}",
            config_file.display()
        );
        std::fs::write(
            config_file.clone(),
            toml::to_string(&default_config).unwrap(),
        )
        .unwrap();
    }

    let config =
        toml::from_str::<Config>(&std::fs::read_to_string(config_file.clone()).unwrap()).unwrap();

    setup_app_state_associations(&ui, &ui_state);

    // Bus for communication between components
    let (bus_sender, _) = broadcast::channel(1024);

    let ui_handle_clone = ui.as_weak().clone();
    let bus_sender_clone = bus_sender.clone();
    let mut last_click_time = Instant::now();

    // Setup file dialog
    ui.on_open_file(move || {
        debug!("Opening file dialog");
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Audio Files", &["mp3", "wav", "ogg", "flac"])
            .pick_file()
        {
            ui_handle_clone
                .upgrade()
                .unwrap()
                .set_file_path(path.to_string_lossy().to_string().into());
            ui_handle_clone
                .upgrade()
                .unwrap()
                .set_status("File selected".to_string().into());
            debug!("Sending load track message");
            let _ = bus_sender_clone.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::LoadTrack(path),
            ));
        }
    });

    // Wire up play button
    let bus_sender_clone = bus_sender.clone();
    ui.on_play(move || {
        debug!("Play button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Play));
    });

    // Wire up stop button
    let bus_sender_clone = bus_sender.clone();
    ui.on_stop(move || {
        debug!("Stop button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Stop));
    });

    // Wire up next button
    let bus_sender_clone = bus_sender.clone();
    ui.on_next(move || {
        debug!("Next button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Next));
    });

    // Wire up previous button
    let bus_sender_clone = bus_sender.clone();
    ui.on_previous(move || {
        debug!("Previous button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Previous));
    });

    // Wire up pause button
    let bus_sender_clone = bus_sender.clone();
    ui.on_pause(move || {
        debug!("Pause button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Pause));
    });

    // Handle playlist item clicks
    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_item_click(move |index, _event, _point| {
        // Only respond to double-click events
        if _event.button.to_string() == "left" && _event.kind.to_string() == "down" {
            if last_click_time.elapsed() < Duration::from_millis(500) {
                debug!("Playlist item double-clicked: {}", index);
                let _ = bus_sender_clone.send(Message::Playback(
                    PlaybackMessage::PlayTrackByIndex(index as usize),
                ));
                ui_handle_clone.unwrap().set_selected_track_index(index);
                ui_handle_clone.unwrap().set_playing_track_index(index);
            } else {
                debug!("Playlist item clicked at index {:?}", index);
                ui_handle_clone.unwrap().set_selected_track_index(index);
                let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::SelectTrack(
                    index as usize,
                )));
            }
            last_click_time = Instant::now();
        }
    });

    // Wire up delete track handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_delete_track(move |index| {
        debug!("Delete track button clicked: {}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeleteTrack(
            index as usize,
        )));
    });

    // Wire up sequence selector
    let bus_sender_clone = bus_sender.clone();
    ui.on_playback_order_changed(move |index| {
        debug!("Playback order changed: {}", index);
        match index {
            0 => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Default),
                ));
            }
            1 => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Shuffle),
                ));
            }
            2 => {
                let _ = bus_sender_clone.send(Message::Playlist(
                    PlaylistMessage::ChangePlaybackOrder(protocol::PlaybackOrder::Random),
                ));
            }
            _ => {
                debug!("Invalid playback order index: {}", index);
            }
        }
    });

    // Wire up repeat toggle
    let bus_sender_clone = bus_sender.clone();
    ui.on_toggle_repeat(move || {
        debug!("Repeat toggle clicked");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ToggleRepeat));
    });

    // Wire up seek handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_seek_to(move |percentage| {
        debug!("Seek requested to {}%", percentage * 100.0);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Seek(percentage)));
    });

    // Setup playlist manager
    let playlist_manager_bus_receiver = bus_sender.subscribe();
    let playlist_manager_bus_sender = bus_sender.clone();
    thread::spawn(move || {
        let mut playlist_manager = PlaylistManager::new(
            Playlist::new(),
            playlist_manager_bus_receiver,
            playlist_manager_bus_sender,
        );
        playlist_manager.run();
    });

    // Setup UI manager
    let ui_manager_bus_sender = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    thread::spawn(move || {
        let mut ui_manager = UiManager::new(
            ui_handle_clone,
            ui_manager_bus_sender.subscribe(),
            ui_manager_bus_sender.clone(),
        );
        ui_manager.run();
    });

    // Setup AudioDecoder
    let decoder_bus_sender = bus_sender.clone();
    let decoder_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_decoder = AudioDecoder::new(decoder_bus_receiver, decoder_bus_sender);
        audio_decoder.run();
    });

    // Setup AudioPlayer
    let player_bus_sender = bus_sender.clone();
    let player_bus_receiver = bus_sender.subscribe();
    thread::spawn(move || {
        let mut audio_player = AudioPlayer::new(player_bus_receiver, player_bus_sender);
        audio_player.run();
    });

    let bus_sender_clone = bus_sender.clone();
    let _ = bus_sender_clone.send(Message::Config(ConfigMessage::ConfigChanged(config)));

    ui.run()?;

    info!("Application exiting");
    Ok(())
}
