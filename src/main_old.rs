mod audio_decoder;
mod audio_player;
mod playlist;
mod playlist_manager;
mod protocol;

use crate::playlist::{PlaybackOrder, Playlist, RepeatMode, Track};
use audio_decoder::{AudioDecoder, Command, Event};
use id3::{Tag, TagLike};
use log::{debug, info};
use playlist_manager::PlaylistManager;
use slint::{ModelRc, StandardListViewItem, VecModel};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// Helper function to format time as MM:SS
fn format_time(milliseconds: u64) -> String {
    if milliseconds == 0 {
        return "0:00".to_string();
    }

    let total_secs = milliseconds / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;

    if secs < 10 {
        format!("{}:0{}", mins, secs)
    } else {
        format!("{}:{}", mins, secs)
    }
}
slint::include_modules!();

fn play_next_track(
    playlist: Arc<Mutex<Playlist>>,
    decoder_cmd_sender: Arc<Mutex<Sender<Command>>>,
) {
    {
        let playlist_clone = playlist.clone();
        let mut playlist_unlocked = playlist_clone.lock().unwrap();
        if let Some(current_index) = playlist_unlocked.get_playing_track_index() {
            // Play the next track
            debug!("Found playing track");

            // Use the playlist's playback order to determine next track
            if let Some(next_index) = playlist_unlocked.get_next_track_index(current_index) {
                // There is a next track
                debug!("Found next track at index {}", next_index);
                playlist_unlocked.set_playing_track_index(Some(next_index));
                playlist_unlocked.set_selected_track(next_index);
            } else {
                // There is no next track
                debug!("No next track");
                playlist_unlocked.set_playing_track_index(None);
                playlist_unlocked.set_playing(false);
                return;
            }
        } else {
            // No track is currently playing
            debug!("No playing track");
            return;
        }
    }

    // This is in its own block to avoid a deadlock
    debug!("Playing next track");
    play_selected_track(playlist, decoder_cmd_sender);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut clog = colog::default_builder();
    clog.filter(None, log::LevelFilter::Debug);
    clog.init();

    //Setup audio decoder
    let (decoder_cmd_sender, command_receiver) = std::sync::mpsc::channel();
    let (event_sender, event_receiver) = std::sync::mpsc::channel();
    let mut audio_decoder = AudioDecoder::new(command_receiver);
    audio_decoder.set_event_sender(event_sender);

    // let (bus_sender, bus_receiver) = crossbeam_channel::unbounded();

    thread::spawn(move || {
        audio_decoder.run();
    });

    let decoder_cmd_sender = Arc::new(Mutex::new(decoder_cmd_sender));
    let ui = AppWindow::new()?;

    // Shared state
    let current_file = Mutex::new(None::<PathBuf>);
    let ui_handle = ui.as_weak();
    // let playlist_manager = PlaylistManager::new(playlist, bus_receiver, bus_sender);
    let playlist = Arc::new(Mutex::new(Playlist::new()));

    // Track state for UI updates
    let track_duration = Arc::new(Mutex::new(0u64));
    let track_duration_for_events = track_duration.clone();

    // Setup track completion and position update handler
    let playlist_for_events = playlist.clone();
    let decoder_cmd_sender_for_events = decoder_cmd_sender.clone();
    let ui_handle_for_events = ui_handle.clone();
    // Update event handling thread
    thread::spawn(move || loop {
        if let Ok(event) = event_receiver.recv() {
            match event {
                Event::TrackFinished => {
                    debug!("Received track finished event");
                    play_next_track(
                        playlist_for_events.clone(),
                        decoder_cmd_sender_for_events.clone(),
                    );
                }
                Event::PositionChanged(current_position) => {
                    let duration = *track_duration_for_events.lock().unwrap();

                    // Calculate percentage for UI
                    let percentage = if duration > 0 {
                        current_position as f32 / duration as f32
                    } else {
                        // For unknown duration, cycle every 4 minutes
                        (current_position % 240000) as f32 / 240000.0
                    };

                    if let Some(ui) = ui_handle_for_events.upgrade() {
                        // Format time values
                        let current_pos_text = format_time(current_position);
                        let total_duration_text = format_time(duration);

                        debug!(
                            "Updating UI: pos={}, dur={}, pct={}, pos_text={}, dur_text={}",
                            current_position,
                            duration,
                            percentage,
                            current_pos_text,
                            total_duration_text
                        );

                        // Update UI elements
                        ui.set_position_percentage(percentage.clamp(0.0, 1.0));
                        ui.set_current_position_text(current_pos_text.into());
                        ui.set_total_duration_text(total_duration_text.into());
                    }
                }
                Event::TrackLoaded {
                    duration_ms,
                    sample_rate: _,
                    channels: _,
                } => {
                    let mut duration = track_duration_for_events.lock().unwrap();
                    *duration = duration_ms;

                    if let Some(ui) = ui_handle_for_events.upgrade() {
                        ui.set_total_duration_text(format_time(duration_ms).into());
                    }
                }
                Event::Error(error_msg) => {
                    debug!("Playback error: {}", error_msg);
                    if let Some(ui) = ui_handle_for_events.upgrade() {
                        ui.set_status(format!("Error: {}", error_msg).into());
                    }
                }
                Event::PlaybackPaused | Event::PlaybackResumed | Event::PlaybackStopped => {
                    // These events can be used to update UI state if needed
                    // debug!("Received playback state change: {:?}", event);
                }
            }
        }

        thread::sleep(Duration::from_millis(20));
    });

    // on_open_file
    let ui_handle_clone = ui_handle.clone();

    let track_model: Rc<VecModel<ModelRc<StandardListViewItem>>> = Rc::new(VecModel::from(vec![]));
    let track_model_rc = ModelRc::from(track_model.clone());
    ui.set_track_model(ModelRc::from(track_model_rc));

    let playlist_rc = playlist.clone();
    ui.on_open_file(move || {
        let ui = ui_handle_clone.upgrade().unwrap();
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Audio Files", &["mp3", "wav", "ogg", "flac"])
            .pick_file()
        {
            *current_file.lock().unwrap() = Some(path.clone());
            ui.set_file_path(path.to_string_lossy().to_string().into());
            ui.set_status("File selected".to_string().into());

            let tag = Tag::read_from_path(&path).unwrap_or_default();
            let artist = tag.artist().unwrap_or("");
            let title = tag.title().unwrap_or("");
            let album = tag.album().unwrap_or("");

            // Push the track to the playlist
            let track_number = playlist_rc.lock().unwrap().num_tracks() + 1;
            track_model.push(ModelRc::new(VecModel::from(vec![
                StandardListViewItem::from(track_number.to_string().as_str()),
                StandardListViewItem::from(title),
                StandardListViewItem::from(artist),
                StandardListViewItem::from(album),
            ])));

            playlist_rc.lock().unwrap().add_track(Track { path });
        }
    });

    // on_play
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_play(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        match playlist.get_playing_track_index() {
            Some(index) => {
                if playlist.get_selected_track_index() != index {
                    let _track_index = Some(playlist.get_selected_track_index());
                    // User clicked play on a different track. Stop the current track and play the new one
                    play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
                } else {
                    if playlist.is_playing() {
                        // User clicked play while already playing this track. Restart from the beginning
                        play_selected_track(
                            playlist_clone.clone(),
                            decoder_cmd_sender_clone.clone(),
                        );
                    } else {
                        // User clicked play while paused. Resume from where it was paused
                        playlist.set_playing(true);
                        let decoder_cmd_sender_unlocked = decoder_cmd_sender_clone.lock().unwrap();
                        if let Err(e) =
                            decoder_cmd_sender_unlocked.send(audio_decoder::Command::Resume)
                        {
                            debug!("Failed to send resume command: {}", e);
                        }
                    }
                }
            }
            None => {
                // No track is currently playing
                if playlist.num_tracks() > 0 {
                    // If there's a playlist, start playing based on playback order
                    if let Some(first_index) = playlist.get_first_track_index() {
                        playlist.set_selected_track(first_index);
                        playlist.set_playing_track_index(Some(first_index));
                        drop(playlist); // Release the lock before calling play_selected_track
                        play_selected_track(
                            playlist_clone.clone(),
                            decoder_cmd_sender_clone.clone(),
                        );
                    }
                }
            }
        }
    });

    // on_pause
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_pause(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        playlist.set_playing(false);
        if let Err(e) = decoder_cmd_sender_clone
            .lock()
            .unwrap()
            .send(audio_decoder::Command::Pause)
        {
            debug!("Failed to send pause command: {}", e);
        }
    });

    // on_stop
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_stop(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        playlist.set_playing_track_index(None);
        playlist.set_playing(false);
        if let Err(e) = decoder_cmd_sender_clone
            .lock()
            .unwrap()
            .send(audio_decoder::Command::Stop)
        {
            debug!("Failed to send stop command: {}", e);
        }
    });

    // on_playlist_item_click
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_playlist_item_click(move |index, _event, _point| {
        // Only respond to double-click events
        if _event.kind.to_string() == "double-click" {
            {
                debug!("Attempting to play track at index {}", index);
                let mut playlist = playlist_clone.lock().unwrap();
                playlist.set_selected_track(index as usize);
            }
            play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
        } else if _event.kind.to_string() == "down" {
            // Single click just selects the track
            let mut playlist = playlist_clone.lock().unwrap();
            playlist.set_selected_track(index as usize);
        }
    });

    // Handle playback order changes
    let playlist_clone = playlist.clone();
    ui.on_playback_order_changed(move |order_index| {
        let playback_order = match order_index {
            0 => PlaybackOrder::Sequential,
            1 => PlaybackOrder::Shuffle,
            2 => PlaybackOrder::Random,
            _ => PlaybackOrder::Sequential, // Default case
        };

        debug!("Changing playback order to {:?}", playback_order);
        let mut playlist = playlist_clone.lock().unwrap();
        playlist.set_playback_order(playback_order);
    });

    // Handle repeat mode toggle
    let playlist_clone = playlist.clone();
    let ui_weak = ui.as_weak();
    ui.on_toggle_repeat(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        let current_mode = playlist.get_repeat_mode();
        let new_mode = match current_mode {
            RepeatMode::Off => RepeatMode::On,
            RepeatMode::On => RepeatMode::Off,
        };

        debug!("Changing repeat mode to {:?}", new_mode);
        playlist.set_repeat_mode(new_mode);

        // Update the UI
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_repeat_on(new_mode == RepeatMode::On);
        }
    });

    // Handle next button
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_next(move || {
        debug!("Next button clicked");
        let mut playlist = playlist_clone.lock().unwrap();

        if let Some(current_index) = playlist.get_playing_track_index() {
            // Find the next track based on current playback order
            if let Some(next_index) = playlist.get_next_track_index(current_index) {
                debug!("Going to next track: {}", next_index);
                playlist.set_playing_track_index(Some(next_index));
                playlist.set_selected_track(next_index);
                drop(playlist); // Release the lock before calling play_selected_track
                play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
            } else {
                debug!("No next track available");
            }
        } else {
            debug!("No track is currently playing");
            // If no track is playing, start playing the first track
            if playlist.num_tracks() > 0 {
                playlist.set_selected_track(0);
                playlist.set_playing_track_index(Some(0));
                drop(playlist); // Release the lock before calling play_selected_track
                play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
            }
        }
    });

    // Handle previous button
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_previous(move || {
        debug!("Previous button clicked");
        let mut playlist = playlist_clone.lock().unwrap();

        if let Some(current_index) = playlist.get_playing_track_index() {
            // Go to previous track (if possible)
            if current_index > 0 {
                let prev_index = current_index - 1;
                debug!("Going to previous track: {}", prev_index);
                playlist.set_playing_track_index(Some(prev_index));
                playlist.set_selected_track(prev_index);
                drop(playlist); // Release the lock before calling play_selected_track
                play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
            } else {
                debug!("Already at the first track");
                // Optionally restart the current track
                drop(playlist); // Release the lock before calling play_selected_track
                play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
            }
        } else {
            debug!("No track is currently playing");
            // If no track is playing, start playing the first track
            if playlist.num_tracks() > 0 {
                playlist.set_selected_track(0);
                playlist.set_playing_track_index(Some(0));
                drop(playlist); // Release the lock before calling play_selected_track
                play_selected_track(playlist_clone.clone(), decoder_cmd_sender_clone.clone());
            }
        }
    });

    // Handle seeking
    let decoder_cmd_sender_clone = decoder_cmd_sender.clone();
    ui.on_seek_to(move |percentage: f32| {
        let duration = *track_duration.lock().unwrap();
        if duration > 0 {
            let position_ms = (percentage * duration as f32) as u64;
            debug!("Seek requested to position: {}ms", position_ms);
            if let Err(e) = decoder_cmd_sender_clone
                .lock()
                .unwrap()
                .send(Command::Seek(position_ms))
            {
                debug!("Failed to send seek command: {}", e);
            }
        }
    });

    ui.run()?;
    info!("Application exiting");
    Ok(())
}

fn play_selected_track(
    playlist: Arc<Mutex<Playlist>>,
    decoder_command_sender: Arc<Mutex<Sender<Command>>>,
) {
    debug!("Locking playlist and decoder command sender");
    let playlist_clone = playlist.clone();
    let decoder_cmd_sender_clone = decoder_command_sender.clone();
    let mut playlist_unlocked = playlist_clone.lock().unwrap();
    let decoder_cmd_sender_unlocked = decoder_cmd_sender_clone.lock().unwrap();

    playlist_unlocked.set_playing(true);

    let selected_track_index = playlist_unlocked.get_selected_track_index();
    playlist_unlocked.set_playing_track_index(Some(selected_track_index));

    let track = playlist_unlocked.get_selected_track();

    decoder_cmd_sender_unlocked
        .send(Command::Play(track.path.clone()))
        .unwrap();
}
