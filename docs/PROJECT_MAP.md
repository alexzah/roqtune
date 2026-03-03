# Project Map

This repository is split into two crates:

- `roqtune` (`src/`): app shell + Slint UI + callback wiring.
- `roqtune-core` (`crates/roqtune-core/src/`): non-UI domain/runtime logic.

## First Places To Check

| Task | Primary Location |
| --- | --- |
| Event bus message shape / flow | `crates/roqtune-core/src/protocol.rs` |
| Playback/decode/output runtime behavior | `crates/roqtune-core/src/audio/` |
| Playlist model + orchestration | `crates/roqtune-core/src/playlist.rs`, `crates/roqtune-core/src/playlist_manager.rs` |
| Library scan/enrichment | `crates/roqtune-core/src/library/` |
| Metadata/tag read/write and ReplayGain | `crates/roqtune-core/src/metadata/` |
| Integrations (OpenSubsonic, keyring, URI) | `crates/roqtune-core/src/integration/` |
| Cast logic | `crates/roqtune-core/src/cast/` |
| Config model/persistence | `crates/roqtune-core/src/config.rs`, `crates/roqtune-core/src/config_persistence.rs` |
| Layout model and persistence schema | `crates/roqtune-core/src/layout.rs` |
| App startup/bootstrap | `src/main.rs`, `src/app_runtime.rs`, `src/app_bootstrap/services.rs` |
| Slint callback wiring | `src/app_callbacks/` |
| UI state synchronization | `src/ui_manager.rs` |
| Slint views/components | `src/roqtune.slint`, `src/ui/components/`, `src/ui/types.slint` |

## Practical Dev Loops

- Full workspace checks: `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`
- Core-only fast loop: `cargo test -p roqtune-core --locked`
- Run app: `cargo run`
