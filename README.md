<img src="images/logo.png" width="50%">

# roqtune

`roqtune` is a desktop music player that aims to bring back the fun of curating and enjoying your personal music library. It is designed to be highly customizable, feature rich, and performant. It is written in Rust and uses the cross platform Slint UI framework.

## Features

- Easily manage multiple playlists of songs with a feature rich playlist mode
- Beautiful library mode that scans and sorts your collection for easy browsing and listening
- Custom column support in the track list, based on metadata format strings
- Fully user customizable UI with layout editor based on modular panels
- Broad audio format support with [symphonia](https://crates.io/crates/symphonia) and [lofty](https://crates.io/crates/lofty)

## Quick Start

### Prerequisites

- Rust toolchain with Cargo (`rustup` recommended)

### Build and Run

- Debug build: `cargo build`
- Run app: `cargo run`
- Release build: `cargo build --release`
- Run release: `cargo run --release`
- Fast compile check: `cargo check`

### Tests and Quality

- Run all tests: `cargo test`
- Run tests with output: `cargo test -- --nocapture`
- Format code: `cargo fmt`
- Lint (deny warnings): `cargo clippy -- -D warnings`

## Keyboard Shortcuts

- `F6` or `Ctrl+L`: toggle layout editor mode
- `Delete`: delete selected tracks (or active playlist when sidebar is focused)
- `F2`: rename active playlist
- `Escape`: close menus/dialogs and exit layout editor mode

## Architecture Overview

The app is organized into cooperating runtime components connected through an event bus (`tokio::sync::broadcast`):

- `src/main.rs`: startup, wiring, Slint callback bindings, and runtime coordination.
- `src/playlist_manager.rs`: playlist edits, playback sequencing, and decode-cache intent.
- `src/audio_decoder.rs`: decode + seek + resample pipeline.
- `src/audio_player.rs`: output stream, queue management, and playback progress.
- `src/ui_manager.rs`: applies bus events to UI state and metadata/cover-art updates.
- `src/protocol.rs`: shared message protocol for all components.
- `src/layout.rs`: layout tree model and edit operations.
- `src/db_manager.rs`: SQLite persistence for playlists, tracks, and playlist UI metadata.

## Data and Config Files

`roqtune` stores files in OS-appropriate user directories via the `dirs` crate.

- Config file: `<config_dir>/roqtune/config.toml`
- Layout file: `<config_dir>/roqtune/layout.toml`
- Playlist database: `<data_dir>/roqtune/playlist.db`
- Cover art cache: `<cache_dir>/roqtune/covers/`
- Output probe cache: `<cache_dir>/roqtune/output_probe_cache.json`

Common Linux defaults:

- `~/.config/roqtune/config.toml`
- `~/.config/roqtune/layout.toml`
- `~/.local/share/roqtune/playlist.db`
- `~/.cache/roqtune/covers/`

System templates in this repo:

- `config/config.system.toml` (copy to `~/.config/roqtune/config.toml` and edit)
- `config/layout.system.toml` (copy to `~/.config/roqtune/layout.toml` and edit)

## Audio Output Behavior

- Match Content mode (default): probes and caches verified device rates, then switches output rate per track boundary when supported.
- Manual mode: keeps one fixed output sample rate (`sample_rate_auto = false` and `sample_rate_khz`).
- Output conversion quality is configurable via `resampler_quality` (`high`/`highest`).
- Optional dither is available for float-to-integer output conversion (`dither_on_bitdepth_reduce`).

## UI Development

- Main UI file: `src/roqtune.slint`
- Reusable UI parts: `src/ui/components/*.slint`
- Shared UI model types: `src/ui/types.slint`

## Project Status

This project is under active development, with ongoing work on playback flow, UI ergonomics, and configuration flexibility.
