use std::hash::{Hash, Hasher};
use std::io::Write;
use std::{collections::hash_map::DefaultHasher, path::PathBuf, rc::Rc};

use id3::{Tag, TagLike};
use log::debug;
use slint::{Model, ModelRc, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{protocol, AppWindow};

pub struct UiState {
    pub track_model: Rc<VecModel<ModelRc<StandardListViewItem>>>,
}

// Manages the playlist
pub struct UiManager {
    ui: slint::Weak<AppWindow>,
    bus_receiver: Receiver<protocol::Message>,
    bus_sender: Sender<protocol::Message>,
    active_playlist_id: String,
    playlist_ids: Vec<String>,
    track_ids: Vec<String>,
    track_paths: Vec<PathBuf>,
    track_metadata: Vec<TrackMetadata>,
}

#[derive(Clone)]
struct TrackMetadata {
    title: String,
    artist: String,
    album: String,
    date: String,
    genre: String,
}

impl UiManager {
    pub fn new(
        ui: slint::Weak<AppWindow>,
        bus_receiver: Receiver<protocol::Message>,
        bus_sender: Sender<protocol::Message>,
    ) -> Self {
        Self {
            ui: ui.clone(),
            bus_receiver,
            bus_sender,
            active_playlist_id: String::new(),
            playlist_ids: Vec::new(),
            track_ids: Vec::new(),
            track_paths: Vec::new(),
            track_metadata: Vec::new(),
        }
    }

    fn find_cover_art(&self, track_path: &PathBuf) -> Option<PathBuf> {
        let parent = track_path.parent()?;
        let names = ["cover", "front", "folder", "album", "art"];
        let extensions = ["jpg", "jpeg", "png", "webp"];

        // Priority 1: External files
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut found_files = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let file_stem_lower = file_stem.to_lowercase();
                        if names.iter().any(|&n| file_stem_lower == n) {
                            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                                let ext_lower = ext.to_lowercase();
                                if extensions.iter().any(|&e| ext_lower == e) {
                                    found_files.push(path);
                                }
                            }
                        }
                    }
                }
            }
            // Sort to have some deterministic behavior if multiple files exist
            found_files.sort();
            if let Some(found) = found_files.into_iter().next() {
                return Some(found);
            }
        }

        // Priority 2: Embedded art
        self.extract_embedded_art(track_path)
    }

    fn extract_embedded_art(&self, track_path: &PathBuf) -> Option<PathBuf> {
        let mut hasher = DefaultHasher::new();
        track_path.hash(&mut hasher);
        let hash = hasher.finish();
        let cache_dir = dirs::cache_dir()?.join("music_player").join("covers");
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).ok()?;
        }

        let cache_path = cache_dir.join(format!("{:x}.png", hash));

        // If already cached, return it
        if cache_path.exists() {
            return Some(cache_path);
        }

        // Try extracting from ID3
        if let Ok(tag) = Tag::read_from_path(track_path) {
            if let Some(pic) = tag.pictures().next() {
                if let Ok(mut file) = std::fs::File::create(&cache_path) {
                    if file.write_all(&pic.data).is_ok() {
                        return Some(cache_path);
                    }
                }
            }
        }

        // Try extracting from FLAC
        if let Ok(tag) = metaflac::Tag::read_from_path(track_path) {
            if let Some(pic) = tag.pictures().next() {
                if let Ok(mut file) = std::fs::File::create(&cache_path) {
                    if file.write_all(&pic.data).is_ok() {
                        return Some(cache_path);
                    }
                }
            }
        }

        None
    }

    fn update_cover_art(&self, track_path: Option<&PathBuf>) {
        let cover_art_path = track_path.and_then(|p| self.find_cover_art(p));
        let _ = self.bus_sender.send(protocol::Message::Playback(
            protocol::PlaybackMessage::CoverArtChanged(cover_art_path),
        ));
    }

    fn read_track_metadata(&self, path: &PathBuf) -> TrackMetadata {
        debug!("Reading metadata for: {}", path.display());

        // Try ID3 tags first
        if let Ok(tag) = Tag::read_from_path(path) {
            let title = tag.title().unwrap_or("");
            let artist = tag.artist().unwrap_or("");
            let album = tag.album().unwrap_or("");
            let date = tag
                .date_recorded()
                .map(|d| d.to_string())
                .or_else(|| tag.year().map(|y| y.to_string()))
                .unwrap_or_default();
            let genre = tag.genre().unwrap_or("").to_string();

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    date,
                    genre,
                };
            }
        }

        // Fall back to APE tags
        if let Ok(ape_tag) = ape::read_from_path(path) {
            let title: &str = ape_tag
                .item("title")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let artist: &str = ape_tag
                .item("artist")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let album: &str = ape_tag
                .item("album")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let date: &str = ape_tag
                .item("year")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");
            let genre: &str = ape_tag
                .item("genre")
                .and_then(|i| i.try_into().ok())
                .unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    date: date.to_string(),
                    genre: genre.to_string(),
                };
            }
        }

        // Fall back to FLAC tags
        if let Ok(flac_tag) = metaflac::Tag::read_from_path(path) {
            let title = flac_tag
                .get_vorbis("title")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let artist = flac_tag
                .get_vorbis("artist")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let album = flac_tag
                .get_vorbis("album")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let date = flac_tag
                .get_vorbis("date")
                .and_then(|mut i| i.next())
                .unwrap_or("");
            let genre = flac_tag
                .get_vorbis("genre")
                .and_then(|mut i| i.next())
                .unwrap_or("");

            if !title.is_empty() || !artist.is_empty() || !album.is_empty() {
                return TrackMetadata {
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    date: date.to_string(),
                    genre: genre.to_string(),
                };
            }
        }

        // If no tags found, use filename as title
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();

        TrackMetadata {
            title: filename,
            artist: "".to_string(),
            album: "".to_string(),
            date: "".to_string(),
            genre: "".to_string(),
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistsRestored(
                        playlists,
                    )) => {
                        self.playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
                        let mut slint_playlists = Vec::new();
                        for p in playlists {
                            slint_playlists.push(StandardListViewItem::from(p.name.as_str()));
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_playlists(ModelRc::from(Rc::new(VecModel::from(
                                slint_playlists,
                            ))));
                        });
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::ActivePlaylistChanged(id),
                    ) => {
                        self.active_playlist_id = id.clone();
                        if let Some(index) = self.playlist_ids.iter().position(|p_id| p_id == &id) {
                            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                ui.set_active_playlist_index(index as i32);
                            });
                        }
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistRestored(
                        tracks,
                    )) => {
                        self.track_ids.clear();
                        self.track_paths.clear();
                        self.track_metadata.clear();

                        let mut track_data = Vec::new();
                        for track in tracks {
                            self.track_ids.push(track.id.clone());
                            self.track_paths.push(track.path.clone());
                            self.track_metadata.push(TrackMetadata {
                                title: track.metadata.title.clone(),
                                artist: track.metadata.artist.clone(),
                                album: track.metadata.album.clone(),
                                date: track.metadata.date.clone(),
                                genre: track.metadata.genre.clone(),
                            });

                            track_data.push((
                                track.metadata.title.clone(),
                                track.metadata.artist.clone(),
                                track.metadata.album.clone(),
                            ));
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");

                            // Clear existing models
                            while track_model.row_count() > 0 {
                                track_model.remove(0);
                            }
                            while selection_model.row_count() > 0 {
                                selection_model.remove(0);
                            }

                            for (title, artist, album) in track_data {
                                track_model.push(ModelRc::new(VecModel::from(vec![
                                    StandardListViewItem::from(""),
                                    StandardListViewItem::from(title.as_str()),
                                    StandardListViewItem::from(artist.as_str()),
                                    StandardListViewItem::from(album.as_str()),
                                ])));
                                selection_model.push(false);
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackAdded {
                        id,
                        path,
                    }) => {
                        self.track_ids.push(id.clone());
                        self.track_paths.push(path.clone());
                        let tags = self.read_track_metadata(&path);
                        self.track_metadata.push(tags.clone());

                        // Send metadata update back to bus for persistence
                        let _ = self.bus_sender.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::UpdateMetadata {
                                id,
                                metadata: protocol::DetailedMetadata {
                                    title: tags.title.clone(),
                                    artist: tags.artist.clone(),
                                    album: tags.album.clone(),
                                    date: tags.date.clone(),
                                    genre: tags.genre.clone(),
                                },
                            },
                        ));

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");
                            track_model.push(ModelRc::new(VecModel::from(vec![
                                StandardListViewItem::from(""),
                                StandardListViewItem::from(tags.title.as_str()),
                                StandardListViewItem::from(tags.artist.as_str()),
                                StandardListViewItem::from(tags.album.as_str()),
                            ])));

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");
                            selection_model.push(false);
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeleteTracks(
                        mut indices,
                    )) => {
                        indices.sort_by(|a, b| b.cmp(a));
                        let indices_to_remove = indices.clone();

                        for index in indices {
                            if index < self.track_ids.len() {
                                self.track_ids.remove(index);
                            }
                            if index < self.track_paths.len() {
                                self.track_paths.remove(index);
                            }
                            if index < self.track_metadata.len() {
                                self.track_metadata.remove(index);
                            }
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");

                            for index in indices_to_remove {
                                if index < track_model.row_count() {
                                    track_model.remove(index);
                                }
                                if index < selection_model.row_count() {
                                    selection_model.remove(index);
                                }
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackByIndex(
                        index,
                    )) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let mut title = String::new();
                            let mut artist = String::new();

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if i == index {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        title = item.row_data(1).unwrap().text.to_string();
                                        artist = item.row_data(2).unwrap().text.to_string();
                                    } else if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                            ui.set_playing_track_index(index as i32);
                            if !artist.is_empty() {
                                ui.set_status_text(format!("{} - {}", artist, title).into());
                            } else {
                                ui.set_status_text(title.into());
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Play) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let playing_index = ui.get_playing_track_index();
                            if playing_index >= 0 {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                    .expect("VecModel expected");
                                if (playing_index as usize) < track_model.row_count() {
                                    if let Some(item) = track_model.row_data(playing_index as usize)
                                    {
                                        item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        let title = item.row_data(1).unwrap().text.to_string();
                                        let artist = item.row_data(2).unwrap().text.to_string();
                                        if !artist.is_empty() {
                                            ui.set_status_text(
                                                format!("{} - {}", artist, title).into(),
                                            );
                                        } else {
                                            ui.set_status_text(title.into());
                                        }
                                    }
                                }
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Pause) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let playing_index = ui.get_playing_track_index();
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            if playing_index >= 0
                                && (playing_index as usize) < track_model.row_count()
                            {
                                if let Some(item) = track_model.row_data(playing_index as usize) {
                                    item.set_row_data(0, StandardListViewItem::from("⏸️"));
                                }
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlaybackProgress {
                        elapsed_ms,
                        total_ms,
                    }) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let elapsed_secs = elapsed_ms / 1000;
                            let elapsed_mins = elapsed_secs / 60;
                            let elapsed_rem_secs = elapsed_secs % 60;
                            let total_secs = total_ms / 1000;
                            let total_mins = total_secs / 60;
                            let total_rem_secs = total_secs % 60;

                            ui.set_time_info(
                                format!(
                                    "{:02}:{:02} / {:02}:{:02}",
                                    elapsed_mins, elapsed_rem_secs, total_mins, total_rem_secs
                                )
                                .into(),
                            );
                            if total_ms > 0 {
                                ui.set_position_percentage(elapsed_ms as f32 / total_ms as f32);
                            }
                            ui.set_current_position_text(
                                format!("{:02}:{:02}", elapsed_mins, elapsed_rem_secs).into(),
                            );
                            ui.set_total_duration_text(
                                format!("{:02}:{:02}", total_mins, total_rem_secs).into(),
                            );
                        });
                    }
                    protocol::Message::Playback(
                        protocol::PlaybackMessage::TechnicalMetadataChanged(meta),
                    ) => {
                        debug!("UiManager: Technical metadata changed: {:?}", meta);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_technical_info(
                                format!(
                                    "{} | {} kbps | {} Hz",
                                    meta.format, meta.bitrate_kbps, meta.sample_rate_hz
                                )
                                .into(),
                            );
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::Stop) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_technical_info("".into());
                            ui.set_time_info("".into());
                            ui.set_status_text("No track selected".into());
                            ui.set_position_percentage(0.0);
                            ui.set_current_position_text("0:00".into());
                            ui.set_total_duration_text("0:00".into());

                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                            ui.set_playing_track_index(-1);
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::TrackStarted {
                        index,
                        playlist_id,
                        metadata,
                    }) => {
                        let is_active_playlist = playlist_id == self.active_playlist_id;
                        let title = metadata.title.clone();
                        let artist = metadata.artist.clone();

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            if is_active_playlist {
                                let track_model_strong = ui.get_track_model();
                                let track_model = track_model_strong
                                    .as_any()
                                    .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                    .expect("VecModel expected");

                                for i in 0..track_model.row_count() {
                                    if let Some(item) = track_model.row_data(i) {
                                        if i == index {
                                            item.set_row_data(0, StandardListViewItem::from("▶️"));
                                        } else if let Some(first_col) = item.row_data(0) {
                                            if !first_col.text.is_empty() {
                                                item.set_row_data(
                                                    0,
                                                    StandardListViewItem::from(""),
                                                );
                                            }
                                        }
                                    }
                                }
                                ui.set_selected_track_index(index as i32);
                                ui.set_playing_track_index(index as i32);
                            } else {
                                ui.set_playing_track_index(-1);
                            }

                            if !artist.is_empty() {
                                ui.set_status_text(format!("{} - {}", artist, title).into());
                            } else {
                                ui.set_status_text(title.into());
                            }
                        });
                    }
                    protocol::Message::Playlist(
                        protocol::PlaylistMessage::PlaylistIndicesChanged {
                            playing_playlist_id,
                            playing_index,
                            selected_indices,
                            is_playing,
                            repeat_on,
                        },
                    ) => {
                        let selected_indices_clone = selected_indices.clone();
                        let is_playing_active_playlist =
                            playing_playlist_id.as_ref() == Some(&self.active_playlist_id);

                        // Priority: 1. First selected track, 2. Playing track (if active), 3. None
                        let display_index = if !selected_indices.is_empty() {
                            Some(selected_indices[0])
                        } else if is_playing_active_playlist {
                            playing_index
                        } else {
                            None
                        };

                        if let Some(idx) = display_index {
                            if let Some(path) = self.track_paths.get(idx) {
                                self.update_cover_art(Some(path));
                            }
                            if let Some(meta) = self.track_metadata.get(idx) {
                                let _ = self.bus_sender.send(protocol::Message::Playback(
                                    protocol::PlaybackMessage::MetadataDisplayChanged(Some(
                                        protocol::DetailedMetadata {
                                            title: meta.title.clone(),
                                            artist: meta.artist.clone(),
                                            album: meta.album.clone(),
                                            date: meta.date.clone(),
                                            genre: meta.genre.clone(),
                                        },
                                    )),
                                ));
                            }
                        } else {
                            self.update_cover_art(None);
                            let _ = self.bus_sender.send(protocol::Message::Playback(
                                protocol::PlaybackMessage::MetadataDisplayChanged(None),
                            ));
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            if is_playing_active_playlist {
                                ui.set_playing_track_index(
                                    playing_index.map(|i| i as i32).unwrap_or(-1),
                                );
                            } else {
                                ui.set_playing_track_index(-1);
                            }
                            ui.set_repeat_on(repeat_on);

                            // Set generic selected index
                            ui.set_selected_track_index(
                                selected_indices_clone
                                    .first()
                                    .copied()
                                    .map(|i| i as i32)
                                    .unwrap_or(-1),
                            );

                            // Set all selected indices for UI highlighting
                            let slint_indices: Vec<i32> =
                                selected_indices_clone.iter().map(|&i| i as i32).collect();
                            ui.set_selected_indices(slint::ModelRc::from(Rc::new(
                                slint::VecModel::from(slint_indices),
                            )));

                            // Update selection models
                            let track_model_strong = ui.get_track_model();
                            let num_tracks = track_model_strong.row_count();
                            let mut selections = vec![false; num_tracks];
                            for &idx in selected_indices_clone.iter() {
                                if idx < num_tracks {
                                    selections[idx] = true;
                                }
                            }
                            ui.set_selection_model(ModelRc::from(Rc::new(VecModel::from(
                                selections,
                            ))));

                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            for i in 0..track_model.row_count() {
                                if let Some(item) = track_model.row_data(i) {
                                    if is_playing_active_playlist && playing_index == Some(i) {
                                        let emoji = if is_playing { "▶️" } else { "⏸️" };
                                        item.set_row_data(0, StandardListViewItem::from(emoji));
                                    } else if let Some(first_col) = item.row_data(0) {
                                        if !first_col.text.is_empty() {
                                            item.set_row_data(0, StandardListViewItem::from(""));
                                        }
                                    }
                                }
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                        indices,
                        to,
                    }) => {
                        let indices_clone = indices.clone();

                        // Update track_paths, track_ids, track_metadata
                        let mut sorted_indices = indices.clone();
                        sorted_indices.sort_by(|a, b| b.cmp(a));
                        let mut moved_paths = Vec::new();
                        let mut moved_ids = Vec::new();
                        let mut moved_metadata = Vec::new();
                        for &idx in sorted_indices.iter() {
                            if idx < self.track_paths.len() {
                                moved_paths.push(self.track_paths.remove(idx));
                            }
                            if idx < self.track_ids.len() {
                                moved_ids.push(self.track_ids.remove(idx));
                            }
                            if idx < self.track_metadata.len() {
                                moved_metadata.push(self.track_metadata.remove(idx));
                            }
                        }
                        moved_paths.reverse();
                        moved_ids.reverse();
                        moved_metadata.reverse();
                        let mut actual_to = to;
                        for &idx in indices.iter() {
                            if idx < to {
                                actual_to -= 1;
                            }
                        }
                        actual_to = actual_to.min(self.track_paths.len());
                        for (i, path) in moved_paths.into_iter().enumerate() {
                            self.track_paths.insert(actual_to + i, path);
                        }
                        for (i, id) in moved_ids.into_iter().enumerate() {
                            self.track_ids.insert(actual_to + i, id);
                        }
                        for (i, metadata) in moved_metadata.into_iter().enumerate() {
                            self.track_metadata.insert(actual_to + i, metadata);
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<ModelRc<StandardListViewItem>>>()
                                .expect("VecModel expected");

                            let selection_model_strong = ui.get_selection_model();
                            let selection_model = selection_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<bool>>()
                                .expect("VecModel expected");

                            let mut sorted_indices = indices_clone.clone();
                            sorted_indices.sort_by(|a, b| b.cmp(a)); // reverse for safe removal

                            let mut moved_rows = Vec::new();
                            let mut moved_selections = Vec::new();

                            for &idx in sorted_indices.iter() {
                                if idx < track_model.row_count() {
                                    moved_rows.push(track_model.row_data(idx).unwrap());
                                    moved_selections.push(selection_model.row_data(idx).unwrap());
                                    track_model.remove(idx);
                                    selection_model.remove(idx);
                                }
                            }
                            moved_rows.reverse();
                            moved_selections.reverse();

                            let mut actual_to = to;
                            for &idx in indices_clone.iter() {
                                if idx < to {
                                    actual_to -= 1;
                                }
                            }
                            actual_to = actual_to.min(track_model.row_count());

                            for (i, row) in moved_rows.into_iter().enumerate() {
                                track_model.insert(actual_to + i, row);
                                selection_model.insert(actual_to + i, moved_selections[i]);
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::CoverArtChanged(
                        path,
                    )) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            if let Some(path) = path {
                                if let Ok(img) = slint::Image::load_from_path(&path) {
                                    ui.set_current_cover_art(img);
                                } else {
                                    ui.set_current_cover_art(slint::Image::default());
                                }
                            } else {
                                ui.set_current_cover_art(slint::Image::default());
                            }
                        });
                    }
                    protocol::Message::Playback(
                        protocol::PlaybackMessage::MetadataDisplayChanged(meta),
                    ) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            if let Some(meta) = meta {
                                ui.set_display_title(meta.title.into());
                                ui.set_display_artist(meta.artist.into());
                                ui.set_display_album(meta.album.into());
                                ui.set_display_date(meta.date.into());
                                ui.set_display_genre(meta.genre.into());
                            } else {
                                ui.set_display_title("".into());
                                ui.set_display_artist("".into());
                                ui.set_display_album("".into());
                                ui.set_display_date("".into());
                                ui.set_display_genre("".into());
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::RepeatModeChanged(
                        repeat_on,
                    )) => {
                        debug!("UiManager: Repeat mode changed: {}", repeat_on);
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_repeat_on(repeat_on);
                        });
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}
