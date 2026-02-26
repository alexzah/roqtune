//! Callback registration for file/folder import and library folder management.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    thread,
    time::Duration,
};

use log::debug;
use slint::winit_030::{winit, EventResult as WinitEventResult, WinitWindowAccessor};
use slint::ComponentHandle;

use crate::{
    app_config_coordinator::apply_config_update,
    app_context::AppSharedState,
    protocol::{self, Message},
    AppWindow,
};

/// Registers import and library-folder callbacks on the root app component.
pub(crate) fn register_imports_library_callbacks(ui: &AppWindow, shared_state: &AppSharedState) {
    let library_folder_import_context = crate::LibraryFolderImportContext {
        shared_state: shared_state.clone(),
    };
    let ui_handle_for_drop = shared_state.ui_handles.ui_handle.clone();
    let pending_drop_paths: Rc<RefCell<Vec<(i32, PathBuf)>>> = Rc::new(RefCell::new(Vec::new()));
    let drop_flush_scheduled = Rc::new(Cell::new(false));
    let playlist_bulk_import_tx_for_drop = shared_state.playlist_bulk_import_tx.clone();
    let bus_sender_for_drop = shared_state.bus_sender.clone();
    let library_context_for_drop = library_folder_import_context.clone();
    ui.window().on_winit_window_event(move |_, event| {
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            debug!("Dropped file: {}", path.display());
            let collection_mode = ui_handle_for_drop
                .upgrade()
                .map(|ui| ui.get_collection_mode())
                .unwrap_or(crate::COLLECTION_MODE_PLAYLIST);
            pending_drop_paths
                .borrow_mut()
                .push((collection_mode, path.clone()));
            if !drop_flush_scheduled.get() {
                drop_flush_scheduled.set(true);
                let pending_drop_paths = pending_drop_paths.clone();
                let drop_flush_scheduled = drop_flush_scheduled.clone();
                let playlist_bulk_import_tx = playlist_bulk_import_tx_for_drop.clone();
                let bus_sender = bus_sender_for_drop.clone();
                let library_context = library_context_for_drop.clone();
                slint::Timer::single_shot(
                    Duration::from_millis(crate::DROP_IMPORT_BATCH_DELAY_MS),
                    move || {
                        drop_flush_scheduled.set(false);
                        let pending = std::mem::take(&mut *pending_drop_paths.borrow_mut());
                        if pending.is_empty() {
                            return;
                        }
                        let mut playlist_paths = Vec::new();
                        let mut library_paths = Vec::new();
                        for (mode, path) in pending {
                            if mode == crate::COLLECTION_MODE_LIBRARY {
                                library_paths.push(path);
                            } else {
                                playlist_paths.push(path);
                            }
                        }
                        if !playlist_paths.is_empty() {
                            let playlist_bulk_import_tx = playlist_bulk_import_tx.clone();
                            let bus_sender = bus_sender.clone();
                            thread::spawn(move || {
                                let tracks =
                                    crate::collect_audio_files_from_dropped_paths(&playlist_paths);
                                if tracks.is_empty() {
                                    debug!("Ignored dropped playlist item(s): no supported tracks");
                                    return;
                                }
                                let source = if playlist_paths.iter().any(|path| path.is_dir()) {
                                    protocol::ImportSource::AddFolderDialog
                                } else {
                                    protocol::ImportSource::AddFilesDialog
                                };
                                let queued = crate::enqueue_playlist_bulk_import(
                                    &playlist_bulk_import_tx,
                                    &bus_sender,
                                    &tracks,
                                    source,
                                );
                                debug!(
                                    "Queued {} track(s) from drag-and-drop into playlist",
                                    queued
                                );
                            });
                        }
                        if !library_paths.is_empty() {
                            let folders =
                                crate::collect_library_folders_from_dropped_paths(&library_paths);
                            if folders.is_empty() {
                                debug!("Ignored dropped library item(s): no usable folders found");
                                return;
                            }
                            let added = crate::add_library_folders_to_config_and_scan(
                                &library_context,
                                folders,
                            );
                            debug!("Added {} folder(s) from drag-and-drop into library", added);
                        }
                    },
                );
            }
        }
        WinitEventResult::Propagate
    });

    let playlist_bulk_import_tx_clone = shared_state.playlist_bulk_import_tx.clone();
    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_open_file(move || {
        debug!("Opening file dialog");
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Audio Files", &crate::SUPPORTED_AUDIO_EXTENSIONS)
            .pick_files()
        {
            let filtered_paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|path| {
                    if crate::is_supported_audio_file(path) {
                        true
                    } else {
                        debug!(
                            "Skipping unsupported file from import dialog: {}",
                            path.display()
                        );
                        false
                    }
                })
                .collect();
            if filtered_paths.is_empty() {
                return;
            }
            let queued = crate::enqueue_playlist_bulk_import(
                &playlist_bulk_import_tx_clone,
                &bus_sender_clone,
                &filtered_paths,
                protocol::ImportSource::AddFilesDialog,
            );
            debug!("Queued {} track(s) from Add files", queued);
        }
    });

    let playlist_bulk_import_tx_clone = shared_state.playlist_bulk_import_tx.clone();
    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_open_folder(move || {
        debug!("Opening folder dialog");
        if let Some(folder_path) = rfd::FileDialog::new().pick_folder() {
            let bus_sender_for_scan = bus_sender_clone.clone();
            let bulk_import_tx = playlist_bulk_import_tx_clone.clone();
            thread::spawn(move || {
                let tracks = crate::collect_audio_files_from_folder(&folder_path);
                debug!(
                    "Found {} track(s) in folder import {}",
                    tracks.len(),
                    folder_path.display()
                );
                let queued = crate::enqueue_playlist_bulk_import(
                    &bulk_import_tx,
                    &bus_sender_for_scan,
                    &tracks,
                    protocol::ImportSource::AddFolderDialog,
                );
                debug!(
                    "Queued {} track(s) from Add folder {}",
                    queued,
                    folder_path.display()
                );
            });
        }
    });

    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_settings_select_library_folder(move |index| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            ui.set_settings_library_selected_folder_index(index);
        }
    });

    let library_folder_import_context_clone = library_folder_import_context.clone();
    ui.on_library_add_folder(move || {
        let Some(folder_path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let folder = folder_path.to_string_lossy().to_string();
        let added = crate::add_library_folders_to_config_and_scan(
            &library_folder_import_context_clone,
            vec![folder],
        );
        debug!("Added {} folder(s) from library folder picker", added);
    });

    let shared_state_clone = shared_state.clone();
    ui.on_library_remove_folder(move |index| {
        if index < 0 {
            return;
        }
        let next_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            if index as usize >= state.library.folders.len() {
                return;
            }
            let mut next = state.clone();
            next.library.folders.remove(index as usize);
            crate::sanitize_config(next)
        };
        apply_config_update(&shared_state_clone, next_config, true);
        let _ = shared_state_clone
            .bus_sender
            .send(Message::Library(protocol::LibraryMessage::RequestScan));
    });

    let shared_state_clone = shared_state.clone();
    ui.on_library_online_metadata_prompt_accept(move || {
        let next_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            next.library.online_metadata_enabled = true;
            next.library.online_metadata_prompt_pending = false;
            crate::sanitize_config(next)
        };
        apply_config_update(&shared_state_clone, next_config, true);
    });

    let shared_state_clone = shared_state.clone();
    ui.on_library_online_metadata_prompt_deny(move || {
        let next_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            next.library.online_metadata_enabled = false;
            next.library.online_metadata_prompt_pending = false;
            crate::sanitize_config(next)
        };
        apply_config_update(&shared_state_clone, next_config, true);
    });

    let shared_state_clone = shared_state.clone();
    ui.on_settings_set_library_online_metadata_enabled(move |enabled| {
        let next_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            next.library.online_metadata_prompt_pending = false;
            next.library.online_metadata_enabled = enabled;
            crate::sanitize_config(next)
        };
        apply_config_update(&shared_state_clone, next_config, true);
    });

    let shared_state_clone = shared_state.clone();
    ui.on_settings_set_library_include_playlist_tracks_in_library(move |enabled| {
        let next_config = {
            let state = shared_state_clone
                .config_state
                .lock()
                .expect("config state lock poisoned");
            let mut next = state.clone();
            next.library.include_playlist_tracks_in_library = enabled;
            crate::sanitize_config(next)
        };
        apply_config_update(&shared_state_clone, next_config, true);
    });
}
