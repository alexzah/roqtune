mod playlist;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use rodio::source::EmptyCallback;
use rodio::{Decoder, OutputStream, Sink};
use id3::{Tag, TagLike};
use slint::{ModelRc, StandardListViewItem, VecModel};
use crate::playlist::{Playlist, Track};

slint::include_modules!();

fn play_next_track(playlist: &mut std::sync::MutexGuard<'_, Playlist>, sink: &std::sync::MutexGuard<'_, Sink>) {
    if let Some(current_index) = playlist.get_playing_track_index() {
        let next_index = current_index + 1;
        if next_index < playlist.num_tracks() {
            playlist.set_playing_track_index(Some(next_index));
            play_selected_track(playlist, sink);
        } else {
            playlist.set_playing_track_index(None);
            playlist.set_playing(false);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {

    let ui = AppWindow::new()?;
    let (_stream, stream_handle) = OutputStream::try_default()?;

    // Shared state
    let current_file = Mutex::new(None::<PathBuf>);
    let sink = Arc::new(Mutex::new(Sink::try_new(&stream_handle)?));
    let ui_handle = ui.as_weak();
    let playlist = Arc::new(Mutex::new(Playlist::new()));

    // on_open_file
    let ui_handle_clone = ui_handle.clone();
    let sink_clone = sink.clone();

    let track_model: Rc<VecModel<ModelRc<StandardListViewItem>>> = Rc::new(VecModel::from(vec![]));
    let track_model_rc = ModelRc::from(track_model.clone());
    ui.set_track_model(ModelRc::from(track_model_rc));

    let mut playlist_rc = playlist.clone();
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
            track_model.push( ModelRc::new(VecModel::from(vec![
                StandardListViewItem::from(title),
                StandardListViewItem::from(artist),
                StandardListViewItem::from(album),
            ])
            ));

            playlist_rc.lock().unwrap().add_track(Track { path });
        }
    });

    // on_play
    let sink_clone = sink.clone();
    let playlist_clone = playlist.clone();
    ui.on_play(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        let sink = sink_clone.lock().unwrap();
        match playlist.get_playing_track_index() {
            Some(index) => {
                if playlist.get_selected_track_index() != index {
                    let track_index = Some(playlist.get_selected_track_index());
                    // User clicked play on a different track. Stop the current track and play the new one
                    playlist.set_playing_track_index(track_index);
                    play_selected_track(&mut playlist, &sink);
                } else {
                    if playlist.is_playing() {
                        // User clicked play while already playing this track. Restart from the beginning
                        play_selected_track(&mut playlist, &sink);
                    } else {
                        // User clicked play while paused. Resume from where it was paused
                        playlist.set_playing(true);
                        sink.play();
                    }
                }
            }
            None => {
                // No track is currently playing. Start playing the selected track
                let track_index = Some(playlist.get_selected_track_index());
                playlist.set_playing_track_index(track_index);
                play_selected_track(&mut playlist, &sink);
            }
        }
    });

    // on_pause
    let sink_clone = sink.clone();
    let playlist_clone = playlist.clone();
    ui.on_pause(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        playlist.set_playing(false);
        let sink = sink_clone.lock().unwrap();
        sink.pause();
    });

    // on_stop
    let sink_clone = sink.clone();
    let playlist_clone = playlist.clone();
    ui.on_stop(move || {
        let mut playlist = playlist_clone.lock().unwrap();
        playlist.set_playing_track_index(None);
        playlist.set_playing(false);
        let sink = sink_clone.lock().unwrap();
        sink.stop();
    });
    
    // on_playlist_item_click
    let playlist_clone = playlist.clone();
    let sink_clone = sink.clone();
    ui.on_playlist_item_click(move |index, _event, _point| {
        let sink = sink_clone.lock().unwrap();
        // TODO: refactor this to use a real enum check
        if _event.kind.to_string() == "down" {
            let mut playlist = playlist_clone.lock().unwrap();
            playlist.set_selected_track(index as usize);
            play_selected_track(&mut playlist, &sink);
        }
    });

    ui.run()?;
    sink.lock().unwrap().stop();
    sink.lock().unwrap().sleep_until_end();
    let playlist_clone = playlist.clone();
    let sink_clone = sink.clone();
    std::thread::spawn(move || {
        while !sink_clone.lock().unwrap().empty() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut playlist = playlist_clone.lock().unwrap();
        let sink = sink_clone.lock().unwrap();
        play_next_track(&mut playlist, &sink);
    });
    println!("Application exiting");
    Ok(())
}

fn play_selected_track(playlist: &mut std::sync::MutexGuard<'_, Playlist>, sink: &std::sync::MutexGuard<'_, Sink>) {
    playlist.set_playing(true);
    sink.stop();
    let track = playlist.get_selected_track();
    sink.append(Decoder::new(std::fs::File::open(&track.path).unwrap()).unwrap());
    sink.play();
}