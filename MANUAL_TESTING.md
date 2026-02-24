# Manual Testing Checklist

This checklist is intended to cover all currently user-verifiable app functionality.

## Test Setup

- [ ] Build and run the app successfully (`cargo run`).
- [ ] Prepare a local library with:
  - [ ] MP3/FLAC tracks.
  - [ ] At least one track with embedded art.
  - [ ] At least one album with external `cover.*` artwork file.
  - [ ] At least one track with missing metadata fields.
- [ ] Prepare at least two playlists with multiple tracks each.
- [ ] Optional: Have a Cast device available on the same network.
- [ ] Optional: Have an OpenSubsonic server account available.

## Startup, Shutdown, and Persistence

- [ ] App starts without crash and shows main window.
- [ ] Previously selected playlist is restored after restart.
- [ ] Window size is restored after restart.
- [ ] Volume value is restored after restart.
- [ ] Playback order and repeat mode are restored after restart.
- [ ] Custom layout and column settings persist across restart.

## Window and Layout Responsiveness

- [ ] Resize window larger: main content expands (no fixed-size rendering region).
- [ ] Resize window smaller: app remains usable and respects minimum size.
- [ ] Sidebar and track/library panes resize proportionally.
- [ ] Layout workspace updates correctly after repeated resizes.

## Mode Switching and Navigation

- [ ] Switch between `Playlists` and `Library` tabs.
- [ ] Enter library sections (Tracks/Artists/Albums/Genres/Decades).
- [ ] Enter and exit library detail pages.
- [ ] Use library back navigation.
- [ ] Open global search from library and return.
- [ ] Search text clears when switching track-list contexts/views.

## Playback Controls

- [ ] Play selected track.
- [ ] Pause and resume playback.
- [ ] Stop playback.
- [ ] Next/Previous track buttons work.
- [ ] Double-clicking a track starts playback immediately.
- [ ] Track auto-advance works at end of track.
- [ ] First track in a fresh session starts promptly and includes song start (no audible truncation).

## Seek Bar and Progress

- [ ] Drag seek handle: audio position changes only on mouse release.
- [ ] While dragging, visual scrub position is stable (no jitter spikes).
- [ ] After release, seek head converges smoothly to live playback.
- [ ] Repeated seek operations do not desync elapsed/total display.
- [ ] Progress does not continue past total duration after track end/repeat transitions.

## Playback Order and Repeat

- [ ] Cycle playback order: Default -> Shuffle -> Random -> Default.
- [ ] Repeat button cycles: Off -> Playlist -> Track -> Off.
- [ ] Repeat `Track`: natural end repeats same track (audio + now playing indicators stay consistent).
- [ ] Repeat `Playlist`: reaches end and continues from start.
- [ ] Manual next/previous behavior remains correct while repeat is enabled.

## Volume and Audio Output Behavior

- [ ] Volume slider updates audio level in real time.
- [ ] Volume value remains clamped between 0% and 100%.
- [ ] Changing audio settings while playing stages changes until stop/restart playback.
- [ ] After stop/start playback, staged audio settings apply.

## Playlist Management

- [ ] Create playlist.
- [ ] Rename playlist (including `F2` flow).
- [ ] Delete playlist (confirmation works).
- [ ] Switch active playlist.
- [ ] Playlist context menu opens and actions work.
- [ ] Remote playlist detach confirmation/cancel flow works (when applicable).
- [ ] Sync playlist to OpenSubsonic action is available/works for eligible playlists.

## Playlist Track Selection and Editing

- [ ] Single-select track.
- [ ] Multi-select with `Ctrl` click.
- [ ] Range-select with `Shift` click.
- [ ] `Ctrl+A` selects all.
- [ ] Deselect all works.
- [ ] Keyboard navigation works: Up/Down/Home/End/PageUp/PageDown.
- [ ] Delete selected tracks works.
- [ ] Copy selected tracks works.
- [ ] Cut selected tracks works.
- [ ] Paste copied tracks works.
- [ ] Undo/redo track list edits works.
- [ ] Drag-and-drop reorder works.
- [ ] Reorder preserves selection and expected track order.

## Playlist Search, Filter, and Sorting

- [ ] Open playlist search (`Ctrl+F` in playlist mode).
- [ ] Playlist query filters list.
- [ ] Close playlist search (`Esc`) and clear behavior is correct.
- [ ] Cycle sort by clicking/using sort control on columns.
- [ ] Filter view can be applied to overwrite playlist (confirmation shown).
- [ ] Clear filter view restores normal editable playlist view.
- [ ] In read-only filter view, edit actions are blocked and visual feedback appears.

## Playlist Columns

- [ ] Open column header menu.
- [ ] Toggle multiple columns without menu closing after each toggle.
- [ ] At least one column always remains enabled.
- [ ] Add custom column with valid name + format.
- [ ] Delete custom column with confirmation.
- [ ] Reorder columns and verify header + row alignment.
- [ ] Resize column by dragging divider; preview updates smoothly.
- [ ] Commit column width and verify persistence.
- [ ] Reset column width to default.
- [ ] Album art column width respects configured min/max.

## Library: Scanning, Browsing, and Actions

- [ ] Add library folder via settings.
- [ ] Remove library folder via settings.
- [ ] Rescan library works.
- [ ] Drag-and-drop folders into library mode adds folders and scans.
- [ ] Track/artist/album/genre/decade list population is correct after scan.
- [ ] Library list selection supports single/ctrl/shift flows.
- [ ] Library item activation opens expected detail/list.
- [ ] Add selected library items to one or more playlists (dialog confirm/cancel).
- [ ] Remove selected library items from library (confirmation/cancel).
- [ ] Open file location works for selected local track.

## Library Search

- [ ] Open library search (`Ctrl+F` in library mode).
- [ ] Library search query filters visible list.
- [ ] Global library search can be opened and navigated back from.
- [ ] Search closes on `Esc`.

## Metadata and Properties Editing

- [ ] Open properties for current selection.
- [ ] Edit multiple metadata fields.
- [ ] Save properties persists tag edits to file.
- [ ] Cancel properties discards unsaved edits.
- [ ] Busy/working state appears during save.
- [ ] Error text is shown for invalid/unwritable cases.
- [ ] Metadata panel reflects updated values after save.

## Artwork and Image Pipeline

- [ ] List views (playlist/library lists) use thumbnail-sized artwork and remain smooth while scrolling.
- [ ] Detail/viewer panel art renders clearly without severe downscale artifacting.
- [ ] Embedded art is proactively processed (not only after click) when no cover file exists.
- [ ] External cover file takes precedence over embedded art.
- [ ] Stopping playback does not blank visible album art rows/panels.
- [ ] Scrolling large track lists does not stutter due to image loading/eviction.

## Layout Editor

- [ ] Enter layout editor via `F6` or `Ctrl+L`.
- [ ] Exit layout editor via toggle and `Esc`.
- [ ] Intro/tutorial proceed/cancel flow behaves correctly.
- [ ] Select a layout leaf and replace panel type.
- [ ] Split selected leaf (horizontal/vertical) and verify new panel placement.
- [ ] Delete selected leaf where valid.
- [ ] Add root leaf when layout is empty.
- [ ] Drag splitter preview + commit changes layout ratio.
- [ ] Layout undo/redo works.
- [ ] Reset layout to default works.
- [ ] Control-cluster leaf menu opens and can add/remove button actions.
- [ ] Viewer-panel settings menu opens and updates priority/metadata/image source.

## Settings Dialog: General

- [ ] Open settings dialog and switch tabs.
- [ ] Toggle:
  - [ ] Show layout editing tutorial.
  - [ ] Show tooltips.
  - [ ] Auto scroll to playing track.
  - [ ] Dark mode.
- [ ] Cancel closes dialog without applying pending changes.
- [ ] Apply commits changes and updates runtime/UI state.

## Settings Dialog: Audio

- [ ] Change output device (auto + explicit + custom value paths).
- [ ] Change output channels (auto + explicit + custom value paths).
- [ ] Change sample-rate mode (`Match Content` / manual mode).
- [ ] In manual mode, set explicit sample rate.
- [ ] Change output bits-per-sample.
- [ ] Change resampler quality.
- [ ] Toggle dither on bit-depth reduce.
- [ ] Toggle downmix for high-channel-count tracks.
- [ ] Toggle cast transcode fallback.
- [ ] Verify restart-required notice behavior if shown by relevant changes.

## Settings Dialog: Library

- [ ] Add/remove/select library folders from settings page.
- [ ] Trigger rescan from settings page.
- [ ] Toggle online metadata setting.
- [ ] If online metadata prompt appears, accept/deny paths both work.
- [ ] Clear internet metadata cache works.

## Settings Dialog: Integrations (OpenSubsonic)

- [ ] Enable/disable OpenSubsonic backend.
- [ ] Save profile with endpoint/username/password.
- [ ] Test connection updates status text.
- [ ] Sync now updates remote content.
- [ ] Disconnect clears active session connection.
- [ ] Keyring-unavailable notice path is handled gracefully.
- [ ] Session password prompt submit path connects using session-only credential.
- [ ] Session password prompt cancel path disconnects and updates status.

## Favorites / Likes

- [ ] Library root list shows `Favorites` with count.
- [ ] Entering `Favorites` shows exactly 3 rows: `Favorite Tracks`, `Favorite Artists`, `Favorite Albums`.
- [ ] Heart toggle works from library list rows for tracks/artists/albums.
- [ ] Heart toggle works from playlist rows when `Favorite` playlist column is enabled.
- [ ] Heart toggle works for remote playlist tracks that are not in the local library index.
- [ ] Now-playing `Favorite` control button is disabled when there is no current track context.
- [ ] Now-playing `Favorite` control button is enabled for playing and paused current-track context.
- [ ] Favorite state persists across restart for local tracks/artists/albums.
- [ ] OpenSubsonic: connect/sync initially populates track favorites from server.
- [ ] OpenSubsonic: local track favorite/unfavorite pushes to server when online.
- [ ] OpenSubsonic: remote write failures keep local state and are retried on next connect/sync.

## Cast Features (Optional)

- [ ] Open cast menu.
- [ ] Refresh cast device discovery.
- [ ] Connect to a device.
- [ ] Start playback while connected and verify remote playback starts.
- [ ] Seek/next/previous/pause/stop are reflected on cast playback.
- [ ] Disconnect from cast device and return to local playback.

## OS / External Integrations

- [ ] OS media controls (play/pause/next/previous/seek) control roqtune.
- [ ] roqtune updates OS media metadata for the active track.
- [ ] External URL links from metadata/enrichment open in browser.

## Drag-and-Drop Import

- [ ] Drag local audio files into playlist mode imports tracks.
- [ ] Drag folder into playlist mode recursively imports supported audio files.
- [ ] Drag folder(s) into library mode adds folders to library config and scans.
- [ ] Unsupported dropped files are ignored without crashing.

## Keyboard and Focus UX

- [ ] `Ctrl+F` opens appropriate search UI for current mode.
- [ ] `Esc` closes open menus/dialogs in expected priority order.
- [ ] `Delete` behavior differs correctly for sidebar-focused playlist delete vs track delete.
- [ ] `F2` starts playlist rename.
- [ ] `F6`/`Ctrl+L` toggles layout editor.
- [ ] Focus returns to main app after modal/dialog dismissal.

## Performance and Stability Smoke

- [ ] Fast scroll very large track list remains responsive.
- [ ] Repeated mode switches (playlist/library/detail/search) do not freeze UI.
- [ ] Repeated play/stop/play cycles do not introduce long playback delays.
- [ ] App remains stable during prolonged session with metadata/art loading.
