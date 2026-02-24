//! UI callback registration for direct event-bus forwarding.

use std::time::Duration;

use log::{debug, warn};
use tokio::sync::broadcast;

use crate::{
    protocol::{self, CastMessage, Message, MetadataMessage, PlaybackMessage, PlaylistMessage},
    AppWindow,
};

/// Context required to register UI callbacks that forward into the bus.
pub struct BusForwardingCallbacksContext {
    /// Shared producer side of the application event bus.
    pub bus_sender: broadcast::Sender<Message>,
    /// Weak app handle used for delayed UI-only effects.
    pub ui_handle: slint::Weak<AppWindow>,
}

/// Registers low-level UI callbacks that forward user actions to bus messages.
pub fn register_bus_forwarding_callbacks(ui: &AppWindow, context: BusForwardingCallbacksContext) {
    let BusForwardingCallbacksContext {
        bus_sender,
        ui_handle,
    } = context;

    let bus_sender_clone = bus_sender.clone();
    ui.on_switch_collection_mode(move |mode| {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::SetCollectionMode(mode),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_select_root(move |section| {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::SelectRootSection(section),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_global_library_search(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::OpenGlobalSearch));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_back(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::NavigateBack));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_select_list_item(move |index, ctrl, shift, context_click| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::SelectListItem {
            index: index as usize,
            ctrl,
            shift,
            context_click,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_item_activated(move |index| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ActivateListItem(index as usize),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_prepare_add_to_playlists(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::PrepareAddToPlaylists,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_toggle_add_to_playlist(move |index| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ToggleAddToPlaylist(index as usize),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_confirm_add_to_playlists(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ConfirmAddToPlaylists,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_cancel_add_to_playlists(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::CancelAddToPlaylists,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_confirm_remove_selection(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ConfirmRemoveSelection,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_cancel_remove_selection(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::CancelRemoveSelection,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_properties_for_current_selection(move || {
        let _ = bus_sender_clone.send(Message::Metadata(
            MetadataMessage::OpenPropertiesForCurrentSelection,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_file_location(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::OpenFileLocation));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_properties_field_edited(move |index, value| {
        if index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Metadata(MetadataMessage::EditPropertiesField {
            index: index as usize,
            value: value.to_string(),
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_properties_save(move || {
        let _ = bus_sender_clone.send(Message::Metadata(MetadataMessage::SaveProperties));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_properties_cancel(move || {
        let _ = bus_sender_clone.send(Message::Metadata(MetadataMessage::CancelProperties));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_rescan(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::RequestScan));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_clear_library_enrichment_cache(move || {
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::ClearEnrichmentCache,
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_remote_detach_confirm(move |playlist_id| {
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::ConfirmDetachRemotePlaylist {
                playlist_id: playlist_id.to_string(),
            },
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_remote_detach_cancel(move |playlist_id| {
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::CancelDetachRemotePlaylist {
                playlist_id: playlist_id.to_string(),
            },
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_play(move || {
        debug!("Play button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::PlayActiveCollection));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_stop(move || {
        debug!("Stop button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Stop));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_next(move || {
        debug!("Next button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Next));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_previous(move || {
        debug!("Previous button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Previous));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_pause(move || {
        debug!("Pause button clicked");
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Pause));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_seek_to(move |percentage| {
        debug!("Seek requested to {}%", percentage * 100.0);
        let _ = bus_sender_clone.send(Message::Playback(PlaybackMessage::Seek(percentage)));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_handle_track_click(move |index, ctrl, shift| {
        debug!(
            "Track clicked at index {:?} (ctrl={}, shift={})",
            index, ctrl, shift
        );
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OnPointerDown {
            index: index as usize,
            ctrl,
            shift,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_deselect_all(move || {
        debug!("Deselect all requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeselectAll));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_select_all(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::SelectAll));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_arrow_key_navigate(move |direction, shift| {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ArrowKeyNavigate {
            direction,
            shift,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_page_navigate(move |action, shift, visible_row_count| {
        use protocol::PageNavigationAction;
        use PlaylistMessage::PageNavigate;
        let nav_action = match action {
            0 => PageNavigationAction::Home,
            1 => PageNavigationAction::End,
            2 => PageNavigationAction::PageUp,
            3 => PageNavigationAction::PageDown,
            _ => return,
        };
        let _ = bus_sender_clone.send(Message::Playlist(PageNavigate {
            action: nav_action,
            shift,
            visible_row_count: visible_row_count as usize,
        }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_playlist_item_double_click(move |index| {
        debug!("Playlist item double-clicked: {}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::PlayTrackByViewIndex(
            index as usize,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_playlist_search(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::OpenPlaylistSearch));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_close_playlist_search(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ClosePlaylistSearch));
    });

    let playlist_search_bus_sender = bus_sender.clone();
    let playlist_search_query_tx =
        crate::spawn_debounced_query_dispatcher(Duration::from_millis(120), move |query| {
            let _ = playlist_search_bus_sender.send(Message::Playlist(
                PlaylistMessage::SetPlaylistSearchQuery(query),
            ));
        });
    ui.on_set_playlist_search_query(move |query| {
        let _ = playlist_search_query_tx.send(query.to_string());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_open_library_search(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::OpenSearch));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_close_library_search(move || {
        let _ = bus_sender_clone.send(Message::Library(protocol::LibraryMessage::CloseSearch));
    });

    let library_search_bus_sender = bus_sender.clone();
    let library_search_query_tx =
        crate::spawn_debounced_query_dispatcher(Duration::from_millis(120), move |query| {
            let _ = library_search_bus_sender.send(Message::Library(
                protocol::LibraryMessage::SetSearchQuery(query),
            ));
        });
    ui.on_library_search_query_edited(move |query| {
        let _ = library_search_query_tx.send(query.to_string());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_library_viewport_changed(move |first_row, row_count| {
        if first_row < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Library(
            protocol::LibraryMessage::LibraryViewportChanged {
                first_row: first_row as usize,
                row_count: row_count.max(0) as usize,
            },
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_playlist_viewport_changed(move |first_row, row_count| {
        if first_row < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::PlaylistViewportChanged {
                first_row: first_row as usize,
                row_count: row_count.max(0) as usize,
            },
        ));
    });

    ui.on_open_external_url(move |url| {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Err(err) = webbrowser::open(trimmed) {
            warn!("Failed to open external URL '{}': {}", trimmed, err);
        }
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_clear_playlist_filter_view(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::ClearPlaylistFilterView));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cycle_playlist_sort_by_column(move |column_index| {
        if column_index < 0 {
            return;
        }
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::CyclePlaylistSortByColumn(column_index as usize),
        ));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_apply_filter_view_to_playlist(move || {
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::RequestApplyFilterView));
    });

    let ui_handle_clone = ui_handle.clone();
    ui.on_playlist_modification_blocked(move || {
        crate::flash_read_only_view_indicator(ui_handle_clone.clone());
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cast_refresh_devices(move || {
        let _ = bus_sender_clone.send(Message::Cast(CastMessage::DiscoverDevices));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cast_connect_device(move |device_id| {
        let id = device_id.trim().to_string();
        if id.is_empty() {
            return;
        }
        let _ = bus_sender_clone.send(Message::Cast(CastMessage::Connect { device_id: id }));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_cast_disconnect(move || {
        let _ = bus_sender_clone.send(Message::Cast(CastMessage::Disconnect));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_create_playlist(move || {
        debug!("Create playlist requested");
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::CreatePlaylist {
            name: String::new(),
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

    let bus_sender_clone = bus_sender.clone();
    ui.on_delete_playlist(move |index| {
        debug!("Delete playlist requested: index={}", index);
        let _ = bus_sender_clone.send(Message::Playlist(PlaylistMessage::DeletePlaylistByIndex(
            index as usize,
        )));
    });

    let bus_sender_clone = bus_sender.clone();
    ui.on_sync_playlist_to_opensubsonic(move |index| {
        debug!("Sync playlist to OpenSubsonic requested: index={}", index);
        let _ = bus_sender_clone.send(Message::Playlist(
            PlaylistMessage::SyncPlaylistToOpenSubsonicByIndex(index as usize),
        ));
    });
}
