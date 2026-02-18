<img src="images/logo.png" width="20%">

# roqtune

`roqtune` is a desktop music player that aims to bring back the fun of curating and enjoying your personal music library. It is designed to be highly customizable, feature rich, and performant. It is written in Rust and uses the cross platform Slint UI framework.

See [ATTRIBUTIONS.md](ATTRIBUTIONS.md) for third-party license information.

<div style="display:flex; gap:12px; align-items:flex-start;">
  <img src="images/screenshot_playlist_mode.png" width="48%" />
  <img src="images/screenshot_library_mode.png" width="48%" />
</div>

## Features

- Easily manage multiple playlists of tracks with a feature rich playlist mode
- Beautiful library mode that scans and sorts your collection for easy browsing and listening
- Custom column support in the track list, based on metadata format strings
- Fully user customizable UI with layout editor based on modular panels
- Broad audio format support with [symphonia](https://crates.io/crates/symphonia) and [lofty](https://crates.io/crates/lofty)

## Project Status

- Project is in early development, and is roughly alpha quality. Major functionality is working but some bugs, especially visual, are still to be expected.
- Only linux is supported currently, though the project is being built cross platform, since that's what I'm able to test at the moment.

## Quick Start

Detailed installation and packaging instructions live in [`INSTALL.md`](INSTALL.md).

### Prerequisites

- Rust toolchain with Cargo (`rustup` recommended)

### Build and Run

- Debug build: `cargo build`
- Run app (unoptimized debug version): `cargo run`
- Release build: `cargo build --release`
- Run release: `cargo run --release`
- Fast compile check: `cargo check`

### Tests and Quality

- Run all tests: `cargo test`
- Run tests with output: `cargo test -- --nocapture`
- Format code: `cargo fmt`
- Lint (deny warnings): `cargo clippy -- -D warnings`

## Keyboard Shortcuts

- Standard cut, copy, paste shortcuts (`Ctrl+X`, `Ctrl+C`, `Ctrl+V`)
- `F6` or `Ctrl+L`: toggle layout editor mode
- `Delete`: delete selected tracks (or active playlist when sidebar is focused)
- `F2`: rename active playlist
- `Escape`: close menus/dialogs and exit layout editor mode

## Architecture Overview

The app is organized into cooperating runtime components connected through an event bus (`tokio::sync::broadcast`):

- `src/protocol.rs`: shared message protocol for all components.
- `src/main.rs`: startup, wiring, Slint callback bindings, and runtime coordination.
- `src/playlist_manager.rs`: playlist edits, playback sequencing, and decode-cache intent.
- `src/audio_decoder.rs`: decode + seek + resample pipeline.
- `src/audio_player.rs`: output stream, queue management, and playback progress.
- `src/ui_manager.rs`: applies bus events to UI state and metadata/cover-art updates.
- `src/layout.rs`: layout tree model and edit operations.
- `src/db_manager.rs`: SQLite persistence for playlists, library index/cache data, and UI metadata.

## Data and Config Files

`roqtune` stores files in OS-appropriate user directories via the `dirs` crate.

- Config file: `<config_dir>/roqtune/config.toml`
- UI Layout file: `<config_dir>/roqtune/layout.toml`
- App-state database (SQLite 3): `<data_dir>/roqtune/roqtune.db`
- Cover art cache: `<cache_dir>/roqtune/covers/`
- Artist enrichment image cache: `<cache_dir>/roqtune/library_enrichment/images/`
- Output probe cache: `<cache_dir>/roqtune/output_probe_cache.json`

Common Linux defaults:

- `~/.config/roqtune/config.toml`
- `~/.config/roqtune/layout.toml`
- `~/.local/share/roqtune/roqtune.db`
- `~/.cache/roqtune/covers/`

System templates in this repo:

- `config/config.system.toml` (copy to `~/.config/roqtune/config.toml` and edit)
- `config/layout.system.toml` (copy to `~/.config/roqtune/layout.toml` and edit)

## UI Development

- Main UI file: `src/roqtune.slint`
- Reusable UI parts: `src/ui/components/*.slint`
- Shared UI model types: `src/ui/types.slint`

## AI Disclosure
- The core event bus architecture, technology choices, and initial implementation were created by hand with minimal AI input
- AI agents were heavily used for feature implementations, resulting in enormous time / effort savings over what I could do by hand
