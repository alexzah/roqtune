mod audio_decoder;
mod audio_player;
mod db_manager;
mod playlist;
mod playlist_manager;
mod protocol;
mod ui_manager;

use std::{rc::Rc, thread};

use audio_decoder::AudioDecoder;
use audio_player::AudioPlayer;
use db_manager::DbManager;
use log::{debug, info};
use playlist::Playlist;
use playlist_manager::PlaylistManager;
use protocol::{Config, ConfigMessage, Message, PlaybackMessage, PlaylistMessage};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
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
        let default_config = protocol::Config {
            output: protocol::OutputConfig {
                channel_count: 2,
                sample_rate_khz: 44100,
                bits_per_sample: 16,
            },
            ui: protocol::UiConfig {
                show_album_art: true,
            },
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

    let config_content = std::fs::read_to_string(config_file.clone()).unwrap();
    let config = toml::from_str::<Config>(&config_content).unwrap();

    setup_app_state_associations(&ui, &ui_state);
    ui.set_show_album_art(config.ui.show_album_art);

    // Bus for communication between components
    let (bus_sender, _) = broadcast::channel(1024);

    let bus_sender_clone = bus_sender.clone();

    // Setup file dialog
    ui.on_open_file(move || {
        debug!("Opening file dialog");
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Audio Files", &["mp3", "wav", "ogg", "flac"])
            .pick_files()
        {
            for path in paths {
                debug!("Sending load track message for {:?}", path);
                let _ = bus_sender_clone.send(protocol::Message::Playlist(
                    protocol::PlaylistMessage::LoadTrack(path),
                ));
            }
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
    ui.on_playlist_item_click(move |index, _event, _point| {
        if _event.button.to_string() == "left" && _event.kind.to_string() == "down" {
            let ctrl = _event.modifiers.control;
            let shift = _event.modifiers.shift;

            debug!(
                "Playlist item clicked at index {:?} (ctrl={}, shift={})",
                index, ctrl, shift
            );
            let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::SelectTrackMulti {
                index: index as usize,
                ctrl,
                shift,
            }));
        }
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_deselect_all(move || {
        debug!("Deselect all requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeselectAll));
    });

    let bus_sender_clone = bus_sender.clone();
    let ui_handle_clone = ui.as_weak().clone();
    ui.on_playlist_item_double_click(move |index| {
        debug!("Playlist item double-clicked: {}", index);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::PlayTrackByIndex(
            index as usize,
        )));
        ui_handle_clone
            .upgrade()
            .unwrap()
            .set_selected_track_index(index);
        ui_handle_clone
            .upgrade()
            .unwrap()
            .set_playing_track_index(index);
    });

    // Wire up delete track handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_delete_track(move |indices| {
        let indices_vec: Vec<usize> = indices.iter().map(|i| i as usize).collect();
        debug!("Delete tracks requested: {:?}", indices_vec);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeleteTracks(
            indices_vec,
        )));
    });

    // Wire up reorder track handler
    let bus_sender_clone = bus_sender.clone();
    ui.on_reorder_tracks(move |indices, to| {
        let indices_vec: Vec<usize> = indices.iter().map(|i| i as usize).collect();
        debug!("Reorder tracks requested: from {:?} to {}", indices_vec, to);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ReorderTracks {
            indices: indices_vec,
            to: to as usize,
        }));
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

    // Wire up playlist management
    let bus_sender_clone = bus_sender.clone();
    ui.on_create_playlist(move || {
        debug!("Create playlist requested");
        // For now let's just create one with a default name.
        // We could add a dialog later if the framework supports it easily or just prompt in CLI.
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CreatePlaylist {
            name: "New Playlist".to_string(),
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_switch_playlist(move |index| {
        debug!("Switch playlist requested: {}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::SwitchPlaylistByIndex(
            index as usize,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_rename_playlist(move |index, name| {
        debug!("Rename playlist requested: index={}, name={}", index, name);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::RenamePlaylistByIndex(
            index as usize,
            name.to_string(),
        )));
    });

    // Setup playlist manager
    let playlist_manager_bus_receiver = bus_sender.subscribe();
    let playlist_manager_bus_sender = bus_sender.clone();
    let db_manager = DbManager::new().expect("Failed to initialize database");
    thread::spawn(move || {
        let mut playlist_manager = PlaylistManager::new(
            Playlist::new(),
            playlist_manager_bus_receiver,
            playlist_manager_bus_sender,
            db_manager,
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
