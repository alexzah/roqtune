use log::{debug, trace};
use slint::Model;

use crate::{
    app_context::AppSharedState,
    protocol::{self, Message, PlaylistMessage},
    AppWindow,
};

pub(crate) fn register_playlist_editing_callbacks(ui: &AppWindow, shared_state: &AppSharedState) {
    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_delete_selected_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                let _ = bus_sender_clone
                    .send(Message::Library(protocol::LibraryMessage::DeleteSelected));
                return;
            }
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        debug!("Delete selected tracks requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeleteSelected));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_reorder_tracks(move |indices, to| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let indices_vec: Vec<usize> = indices.iter().map(|i| i as usize).collect();
        debug!("Reorder tracks requested: from {:?} to {}", indices_vec, to);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ReorderTracks {
            indices: indices_vec,
            to: to as usize,
        }));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_copy_selected_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                let _ =
                    bus_sender_clone.send(Message::Library(protocol::LibraryMessage::CopySelected));
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CopySelectedTracks));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_cut_selected_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                let _ =
                    bus_sender_clone.send(Message::Library(protocol::LibraryMessage::CutSelected));
                return;
            }
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CutSelectedTracks));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_paste_copied_tracks(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_collection_mode() == 1 {
                return;
            }
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::PasteCopiedTracks));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_undo_last_action(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::UndoTrackListEdit));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_redo_last_action(move || {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::RedoTrackListEdit));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_on_pointer_down(move |index, ctrl, shift| {
        trace!(
            "Pointer down at index {:?} (ctrl={}, shift={})",
            index,
            ctrl,
            shift
        );
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnPointerDown {
            index: index as usize,
            ctrl,
            shift,
        }));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_on_drag_start(move |pressed_index| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                crate::flash_read_only_view_indicator(ui_handle_clone.clone());
                return;
            }
        }
        trace!("Drag start at index {:?}", pressed_index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragStart {
            pressed_index: pressed_index as usize,
        }));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    let ui_handle_clone = shared_state.ui_handles.ui_handle.clone();
    ui.on_on_drag_move(move |drop_gap| {
        if let Some(ui) = ui_handle_clone.upgrade() {
            if ui.get_playlist_filter_active() {
                return;
            }
        }
        trace!("Drag move to gap {:?}", drop_gap);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragMove {
            drop_gap: drop_gap as usize,
        }));
    });

    let bus_sender_clone = shared_state.bus_sender.clone();
    ui.on_on_drag_end(move |drop_gap, drag_blocked| {
        trace!(
            "Drag end at gap {:?}, blocked: {:?}",
            drop_gap,
            drag_blocked
        );
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnDragEnd {
            drop_gap: drop_gap as usize,
            drag_blocked,
        }));
    });
}
