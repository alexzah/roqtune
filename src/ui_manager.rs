use std::hash::{Hash, Hasher};
use std::io::Write;
use std::time::Duration;
use std::{collections::hash_map::DefaultHasher, path::PathBuf, rc::Rc};

use governor::state::NotKeyed;
use id3::{Tag, TagLike};
use log::debug;
use slint::{Model, ModelRc, StandardListViewItem, VecModel};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::{protocol, AppWindow, TrackRowData};
use governor::{Quota, RateLimiter};

pub struct UiState {
    pub track_model: Rc<VecModel<TrackRowData>>,
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
    selected_indices: Vec<usize>,
    drag_indices: Vec<usize>,
    is_dragging: bool,
    pressed_index: Option<usize>,
    progress_rl:
        RateLimiter<NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>,
    // Cached progress values to avoid unnecessary UI updates
    last_elapsed_ms: u64,
    last_total_ms: u64,
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
            selected_indices: Vec::new(),
            drag_indices: Vec::new(),
            is_dragging: false,
            pressed_index: None,
            progress_rl: RateLimiter::direct(
                Quota::with_period(Duration::from_millis(30)).unwrap(),
            ),
            last_elapsed_ms: 0,
            last_total_ms: 0,
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

    pub fn on_pointer_down(&mut self, pressed_index: usize, ctrl: bool, shift: bool) {
        debug!(
            "on_pointer_down: index={}, ctrl={}, shift={}",
            pressed_index, ctrl, shift
        );
        self.pressed_index = Some(pressed_index);

        let is_already_selected = self.selected_indices.contains(&pressed_index);
        if !is_already_selected || ctrl || shift {
            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::SelectTrackMulti {
                    index: pressed_index,
                    ctrl,
                    shift,
                },
            ));
        }
    }

    pub fn on_drag_start(&mut self, pressed_index: usize) {
        debug!(
            ">>> on_drag_start START: pressed_index={}, self.selected_indices={:?}",
            pressed_index, self.selected_indices
        );
        if self.selected_indices.contains(&pressed_index) {
            self.drag_indices = self.selected_indices.clone();
            debug!(
                ">>> MULTI-SELECT DRAG: using drag_indices {:?}",
                self.drag_indices
            );
        } else {
            self.drag_indices = vec![pressed_index];
            debug!(">>> SINGLE DRAG: using index {:?}", self.drag_indices);
        }
        self.drag_indices.sort();
        debug!(
            ">>> on_drag_start END: drag_indices={:?}",
            self.drag_indices
        );
        self.is_dragging = true;
    }

    pub fn on_drag_move(&mut self, drop_gap: usize) {
        if self.is_dragging {
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                ui.set_drop_index(drop_gap as i32);
            });
        }
    }

    pub fn on_drag_end(&mut self, drop_gap: usize) {
        debug!(
            ">>> on_drag_end START: is_dragging={}, drop_gap={}, drag_indices={:?}",
            self.is_dragging, drop_gap, self.drag_indices
        );
        if self.is_dragging && !self.drag_indices.is_empty() {
            let indices = self.drag_indices.clone();
            let to = drop_gap;
            debug!(
                ">>> on_drag_end SENDING ReorderTracks: indices={:?}, to={}",
                indices, to
            );

            let _ = self.bus_sender.send(protocol::Message::Playlist(
                protocol::PlaylistMessage::ReorderTracks { indices, to },
            ));
        }

        self.drag_indices.clear();
        self.is_dragging = false;
        self.pressed_index = None;

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_is_dragging(false);
            ui.set_drop_index(-1);
            ui.set_pressed_index(-1);
        });
    }

    pub fn run(&mut self) {
        loop {
            match self.bus_receiver.blocking_recv() {
                Ok(message) => match message {
                    protocol::Message::Playlist(protocol::PlaylistMessage::PlaylistsRestored(
                        playlists,
                    )) => {
                        let old_len = self.playlist_ids.len();
                        self.playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
                        let new_len = self.playlist_ids.len();
                        let mut slint_playlists = Vec::new();
                        for p in playlists {
                            slint_playlists.push(StandardListViewItem::from(p.name.as_str()));
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_playlists(ModelRc::from(Rc::new(VecModel::from(
                                slint_playlists,
                            ))));
                            if new_len > old_len && old_len > 0 {
                                ui.set_editing_playlist_index((new_len - 1) as i32);
                            }
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

                            track_data.push(TrackRowData {
                                status: "".into(),
                                title: track.metadata.title.clone().into(),
                                artist: track.metadata.artist.clone().into(),
                                album: track.metadata.album.clone().into(),
                                selected: false,
                            });
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            while track_model.row_count() > 0 {
                                track_model.remove(0);
                            }

                            for track in track_data {
                                track_model.push(track);
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
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");
                            track_model.push(TrackRowData {
                                status: "".into(),
                                title: tags.title.clone().into(),
                                artist: tags.artist.clone().into(),
                                album: tags.album.clone().into(),
                                selected: false,
                            });
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
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            for index in indices_to_remove {
                                if index < track_model.row_count() {
                                    track_model.remove(index);
                                }
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::DeleteSelected) => {
                        if self.selected_indices.is_empty() {
                            continue;
                        }
                        let indices = self.selected_indices.clone();
                        let _ = self.bus_sender.send(protocol::Message::Playlist(
                            protocol::PlaylistMessage::DeleteTracks(indices),
                        ));
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlayTrackByIndex(
                        index,
                    )) => {
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            let mut title = slint::SharedString::new();
                            let mut artist = slint::SharedString::new();

                            for i in 0..track_model.row_count() {
                                let mut row = track_model.row_data(i).unwrap();
                                if i == index {
                                    row.status = "▶️".into();
                                    title = row.title.clone();
                                    artist = row.artist.clone();
                                    track_model.set_row_data(i, row);
                                } else if !row.status.is_empty() {
                                    row.status = "".into();
                                    track_model.set_row_data(i, row);
                                }
                            }
                            ui.set_playing_track_index(index as i32);
                            if !artist.is_empty() {
                                ui.set_status_text(format!("{} - {}", artist, title).into());
                            } else {
                                ui.set_status_text(title);
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
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");
                                if (playing_index as usize) < track_model.row_count() {
                                    let mut row =
                                        track_model.row_data(playing_index as usize).unwrap();
                                    let title = row.title.clone();
                                    let artist = row.artist.clone();
                                    row.status = "▶️".into();
                                    track_model.set_row_data(playing_index as usize, row);
                                    if !artist.is_empty() {
                                        ui.set_status_text(
                                            format!("{} - {}", artist, title).into(),
                                        );
                                    } else {
                                        ui.set_status_text(title);
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
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            if playing_index >= 0
                                && (playing_index as usize) < track_model.row_count()
                            {
                                let mut row = track_model.row_data(playing_index as usize).unwrap();
                                row.status = "⏸️".into();
                                track_model.set_row_data(playing_index as usize, row);
                            }
                        });
                    }
                    protocol::Message::Playback(protocol::PlaybackMessage::PlaybackProgress {
                        elapsed_ms,
                        total_ms,
                    }) => {
                        if self.progress_rl.check().is_ok() {
                            // Check if the displayed second has changed (for text updates)
                            let elapsed_secs = elapsed_ms / 1000;
                            let last_elapsed_secs = self.last_elapsed_ms / 1000;
                            let time_text_changed =
                                elapsed_secs != last_elapsed_secs || total_ms != self.last_total_ms;

                            // Always compute percentage for smooth progress bar
                            let percentage = if total_ms > 0 {
                                elapsed_ms as f32 / total_ms as f32
                            } else {
                                0.0
                            };

                            if time_text_changed {
                                // Update cached values and set integer ms properties
                                // This triggers Slint's pure functions to recompute text
                                self.last_elapsed_ms = elapsed_ms;
                                self.last_total_ms = total_ms;

                                let elapsed_ms_i32 = elapsed_ms as i32;
                                let total_ms_i32 = total_ms as i32;

                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_elapsed_ms(elapsed_ms_i32);
                                    ui.set_total_ms(total_ms_i32);
                                    ui.set_position_percentage(percentage);
                                });
                            } else {
                                // Just update the progress bar percentage for smooth animation
                                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                                    ui.set_position_percentage(percentage);
                                });
                            }
                        }
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
                        // Reset cached progress values
                        self.last_elapsed_ms = 0;
                        self.last_total_ms = 0;

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_technical_info("".into());
                            ui.set_status_text("No track selected".into());
                            ui.set_position_percentage(0.0);
                            ui.set_elapsed_ms(0);
                            ui.set_total_ms(0);

                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            for i in 0..track_model.row_count() {
                                let mut row = track_model.row_data(i).unwrap();
                                if !row.status.is_empty() {
                                    row.status = "".into();
                                    track_model.set_row_data(i, row);
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
                                    .downcast_ref::<VecModel<TrackRowData>>()
                                    .expect("VecModel<TrackRowData> expected");

                                for i in 0..track_model.row_count() {
                                    let mut row = track_model.row_data(i).unwrap();
                                    if i == index {
                                        row.status = "▶️".into();
                                        track_model.set_row_data(i, row);
                                    } else if !row.status.is_empty() {
                                        row.status = "".into();
                                        track_model.set_row_data(i, row);
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
                            playing_track_path,
                            playing_track_metadata,
                            selected_indices,
                            is_playing,
                            playback_order,
                            repeat_mode,
                        },
                    ) => {
                        let selected_indices_clone = selected_indices.clone();
                        self.selected_indices = selected_indices_clone.clone();
                        let is_playing_active_playlist =
                            playing_playlist_id.as_ref() == Some(&self.active_playlist_id);

                        if !selected_indices.is_empty() {
                            let idx = selected_indices[0];
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
                        } else if let (Some(path), Some(meta)) =
                            (playing_track_path, playing_track_metadata)
                        {
                            self.update_cover_art(Some(&path));
                            let _ = self.bus_sender.send(protocol::Message::Playback(
                                protocol::PlaybackMessage::MetadataDisplayChanged(Some(meta)),
                            ));
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

                            let repeat_int = match repeat_mode {
                                protocol::RepeatMode::Off => 0,
                                protocol::RepeatMode::Playlist => 1,
                                protocol::RepeatMode::Track => 2,
                            };
                            ui.set_repeat_mode(repeat_int);

                            let order_int = match playback_order {
                                protocol::PlaybackOrder::Default => 0,
                                protocol::PlaybackOrder::Shuffle => 1,
                                protocol::PlaybackOrder::Random => 2,
                            };
                            ui.set_playback_order_index(order_int);

                            ui.set_selected_track_index(
                                selected_indices_clone
                                    .first()
                                    .copied()
                                    .map(|i| i as i32)
                                    .unwrap_or(-1),
                            );

                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            for i in 0..track_model.row_count() {
                                let mut row = track_model.row_data(i).unwrap();
                                let was_selected = row.selected;
                                let should_be_selected = selected_indices_clone.contains(&i);

                                if was_selected != should_be_selected {
                                    row.selected = should_be_selected;
                                    track_model.set_row_data(i, row);
                                }
                            }

                            for i in 0..track_model.row_count() {
                                let mut row = track_model.row_data(i).unwrap();
                                if is_playing_active_playlist && playing_index == Some(i) {
                                    let emoji = if is_playing { "▶️" } else { "⏸️" };
                                    row.status = emoji.into();
                                    track_model.set_row_data(i, row);
                                } else if !row.status.is_empty() {
                                    row.status = "".into();
                                    track_model.set_row_data(i, row);
                                }
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::SelectionChanged(
                        indices,
                    )) => {
                        debug!(
                            "SelectionChanged: setting selected_indices to {:?}",
                            indices
                        );
                        let indices_clone = indices.clone();
                        self.selected_indices = indices_clone.clone();
                        debug!(
                            "After SelectionChanged: self.selected_indices = {:?}",
                            self.selected_indices
                        );

                        // Update cover art and metadata display based on selection
                        if !indices.is_empty() {
                            let idx = indices[0];
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
                        }

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            for i in 0..track_model.row_count() {
                                let mut row = track_model.row_data(i).unwrap();
                                let should_be_selected = indices_clone.contains(&i);
                                if row.selected != should_be_selected {
                                    row.selected = should_be_selected;
                                    track_model.set_row_data(i, row);
                                }
                            }
                        });
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::OnPointerDown {
                        index,
                        ctrl,
                        shift,
                    }) => {
                        self.on_pointer_down(index, ctrl, shift);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::OnDragStart {
                        pressed_index,
                    }) => {
                        self.on_drag_start(pressed_index);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::OnDragMove {
                        drop_gap,
                    }) => {
                        self.on_drag_move(drop_gap);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::OnDragEnd {
                        drop_gap,
                    }) => {
                        self.on_drag_end(drop_gap);
                    }
                    protocol::Message::Playlist(protocol::PlaylistMessage::ReorderTracks {
                        indices,
                        to,
                    }) => {
                        debug!("ReorderTracks: indices={:?}, to={}", indices, to);

                        let indices_clone = indices.clone();

                        let mut sorted_indices = indices.clone();
                        sorted_indices.sort_unstable();
                        sorted_indices.dedup();
                        sorted_indices.retain(|&i| i < self.track_paths.len());

                        if sorted_indices.is_empty() {
                            continue;
                        }

                        let to = to.min(self.track_paths.len());

                        let first = sorted_indices[0];
                        let last = *sorted_indices.last().unwrap();
                        let block_len = sorted_indices.len();

                        let is_contiguous = sorted_indices
                            .iter()
                            .enumerate()
                            .all(|(k, &i)| i == first + k);

                        if is_contiguous && to >= first && to <= last + 1 {
                            continue;
                        }

                        let mut moved_paths = Vec::new();
                        let mut moved_ids = Vec::new();
                        let mut moved_metadata = Vec::new();

                        for &idx in sorted_indices.iter().rev() {
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

                        let removed_before = sorted_indices.iter().filter(|&&i| i < to).count();
                        let insert_at = to.saturating_sub(removed_before);

                        for (i, path) in moved_paths.into_iter().enumerate() {
                            self.track_paths.insert(insert_at + i, path);
                        }
                        for (i, id) in moved_ids.into_iter().enumerate() {
                            self.track_ids.insert(insert_at + i, id);
                        }
                        for (i, metadata) in moved_metadata.into_iter().enumerate() {
                            self.track_metadata.insert(insert_at + i, metadata);
                        }

                        self.selected_indices = (insert_at..insert_at + block_len).collect();

                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            let track_model_strong = ui.get_track_model();
                            let track_model = track_model_strong
                                .as_any()
                                .downcast_ref::<VecModel<TrackRowData>>()
                                .expect("VecModel<TrackRowData> expected");

                            let mut sorted_indices = indices_clone.clone();
                            sorted_indices.sort_unstable();
                            sorted_indices.dedup();
                            sorted_indices.retain(|&i| i < track_model.row_count());

                            if sorted_indices.is_empty() {
                                return;
                            }

                            let to = to.min(track_model.row_count());
                            let first = sorted_indices[0];
                            let last = *sorted_indices.last().unwrap();

                            if is_contiguous && to >= first && to <= last + 1 {
                                return;
                            }

                            let mut moved_rows = Vec::new();
                            for &idx in sorted_indices.iter().rev() {
                                if idx < track_model.row_count() {
                                    moved_rows.push(track_model.row_data(idx).unwrap());
                                    track_model.remove(idx);
                                }
                            }
                            moved_rows.reverse();

                            let removed_before = sorted_indices.iter().filter(|&&i| i < to).count();
                            let insert_at = to.saturating_sub(removed_before);

                            for (i, row) in moved_rows.into_iter().enumerate() {
                                track_model.insert(insert_at + i, row);
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
                        repeat_mode,
                    )) => {
                        debug!("UiManager: Repeat mode changed: {:?}", repeat_mode);
                        let repeat_int = match repeat_mode {
                            protocol::RepeatMode::Off => 0,
                            protocol::RepeatMode::Playlist => 1,
                            protocol::RepeatMode::Track => 2,
                        };
                        let _ = self.ui.upgrade_in_event_loop(move |ui| {
                            ui.set_repeat_mode(repeat_int);
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

#[cfg(test)]
mod tests {
    fn apply_reorder(paths: &mut Vec<String>, indices: &[usize], gap: usize) -> Vec<usize> {
        let len = paths.len();
        if len == 0 {
            return vec![];
        }

        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        indices.retain(|&i| i < len);

        if indices.is_empty() {
            return vec![];
        }

        let gap = gap.min(len);

        let first = indices[0];
        let last = *indices.last().unwrap();
        let block_len = indices.len();

        let is_contiguous = indices.iter().enumerate().all(|(k, &i)| i == first + k);

        if is_contiguous && gap >= first && gap <= last + 1 {
            return indices;
        }

        let mut moved = Vec::new();
        for &idx in indices.iter().rev() {
            if idx < paths.len() {
                moved.push(paths.remove(idx));
            }
        }
        moved.reverse();

        let removed_before = indices.iter().filter(|&&i| i < gap).count();
        let insert_at = gap.saturating_sub(removed_before);

        for (offset, item) in moved.into_iter().enumerate() {
            paths.insert(insert_at + offset, item);
        }

        (insert_at..insert_at + block_len).collect()
    }

    // Gap-index semantic tests
    // Gap 0 = before element 0
    // Gap 1 = between element 0 and 1
    // Gap 2 = between element 1 and 2
    // Gap N = after element N-1

    fn make_test_paths() -> Vec<String> {
        vec![
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            "D".to_string(),
        ]
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_0_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 0);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_2() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 2);
        assert_eq!(paths, vec!["B", "A", "C", "D"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_3() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 3);
        assert_eq!(paths, vec!["B", "C", "A", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_first_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0], 4);
        assert_eq!(paths, vec!["B", "C", "D", "A"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 0);
        assert_eq!(paths, vec!["C", "A", "B", "D"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 1);
        assert_eq!(paths, vec!["A", "C", "B", "D"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_middle_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 0);
        assert_eq!(paths, vec!["D", "A", "B", "C"]);
        assert_eq!(new_selection, vec![0]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_1() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 1);
        assert_eq!(paths, vec!["A", "D", "B", "C"]);
        assert_eq!(new_selection, vec![1]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_2() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 2);
        assert_eq!(paths, vec!["A", "B", "D", "C"]);
        assert_eq!(new_selection, vec![2]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_single_last_to_gap_4_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[3], 4);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![3]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_0_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 0);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_3() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 3);
        assert_eq!(paths, vec!["C", "A", "B", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_first_two_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1], 4);
        assert_eq!(paths, vec!["C", "D", "A", "B"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 0);
        assert_eq!(paths, vec!["B", "C", "A", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_1_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 1);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_middle_two_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2], 4);
        assert_eq!(paths, vec!["A", "D", "B", "C"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 0);
        assert_eq!(paths, vec!["C", "D", "A", "B"]);
        assert_eq!(new_selection, vec![0, 1]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_1() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 1);
        assert_eq!(paths, vec!["A", "C", "D", "B"]);
        assert_eq!(new_selection, vec![1, 2]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_2_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 2);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_3_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 3);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_multi_last_two_to_gap_4_noop() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[2, 3], 4);
        assert_eq!(paths, vec!["A", "B", "C", "D"]);
        assert_eq!(new_selection, vec![2, 3]);
    }

    #[test]
    fn test_gap_reorder_all_three_to_gap_4() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1, 2], 4);
        assert_eq!(paths, vec!["D", "A", "B", "C"]);
        assert_eq!(new_selection, vec![1, 2, 3]);
    }

    #[test]
    fn test_gap_reorder_all_three_from_middle_to_gap_0() {
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[1, 2, 3], 0);
        assert_eq!(paths, vec!["B", "C", "D", "A"]);
        assert_eq!(new_selection, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_reorder_non_contiguous_not_noop() {
        // Select A, B, D (non-contiguous) and drop at gap 2 (after B)
        // Should bring D up before C, even though A and B don't move
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 1, 3], 2);
        assert_eq!(paths, vec!["A", "B", "D", "C"]);
        assert_eq!(new_selection, vec![0, 1, 2]);
    }

    #[test]
    fn test_gap_reorder_non_contiguous_middle_gap() {
        // Select A, C (non-contiguous) and drop at gap 1 (after A)
        // Should move C to after A, resulting in [A, C, B, D]
        let mut paths = make_test_paths();
        let new_selection = apply_reorder(&mut paths, &[0, 2], 1);
        assert_eq!(paths, vec!["A", "C", "B", "D"]);
        assert_eq!(new_selection, vec![0, 1]);
    }
}
