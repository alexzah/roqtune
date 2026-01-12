# AGENTS.md - Development Guidelines for Music Player

This document provides instructions and guidelines for AI agents and developers working on the Music Player project.
It aims to ensure consistency, quality, and adherence to the project's architectural standards.

## 1. Build, Lint, and Test Commands

The project uses standard Cargo commands. Ensure you run these checks before submitting changes.

### Build
- **Debug Build**: `cargo build`
  - Use for general development.
- **Release Build**: `cargo build --release`
  - Use when performance is critical or for final artifacts.
- **Check**: `cargo check`
  - Fast check to verify code compilation without generating binaries.

### Run
- **Run Application**: `cargo run`
  - Runs the debug build. For longer debugging sessions, it is preferred that you add logs, run the build, and instruct me what to manually test so that you can verify the output
- **Run Release**: `cargo run --release`

### Testing
- There are currently no unit tests, so you will have to add them for the below commands to work properly
- **Run All Tests**: `cargo test`
  - Executes unit and integration tests.
- **Run Single Test**: `cargo test <test_name>`
  - Example: `cargo test test_playlist_shuffle`
  - Use this when focusing on a specific feature or bug fix.
- **Run Tests with Output**: `cargo test -- --nocapture`
  - Useful for debugging tests with `println!` debugging.

### Code Quality & Formatting
- **Format Code**: `cargo fmt`
  - **MANDATORY**: Always run this before committing.
  - Follows standard Rust formatting rules.
- **Lint Code**: `cargo clippy`
  - **MANDATORY**: Address all warnings.
  - Helps catch common mistakes and improve code quality.
  - Run `cargo clippy -- -D warnings` to treat warnings as errors.

---

## 2. Code Style & Conventions

Adhere strictly to the following conventions to maintain codebase consistency.

### General Philosophy
- **Idiomatic Rust**: Prefer standard Rust idioms (e.g., iterators over loops where appropriate, `Result` for errors).
- **Safety**: Avoid `unsafe` code unless absolutely necessary and documented.
- **Clarity**: Write code that is easy to read and understand. self-documenting code is preferred over excessive comments.

### Imports
- **Grouping**:
  1. Standard Library (`use std::...`)
  2. External Crates (`use slint::...`, `use log::...`)
  3. Local Modules (`use crate::...`, `use audio_decoder::...`)
- **Sorting**: Alphabetical within groups (handled by `cargo fmt`).
- **Granularity**: Import specific types/functions rather than wildcards (`*`), except for preludes or when explicitly justified.

### Formatting
- **Indentation**: 4 spaces.
- **Line Length**: Default standard (usually 100 chars), handled by `rustfmt`.
- **Braces**: K&R style (opening brace on same line).

### Naming Conventions
- **Variables & Functions**: `snake_case`
  - `let track_count = ...`
  - `fn generate_shuffle_order(...)`
- **Types (Structs, Enums, Traits)**: `PascalCase` (CamelCase)
  - `struct AudioDecoder`
  - `enum PlaybackOrder`
- **Constants & Statics**: `SCREAMING_SNAKE_CASE`
  - `const MAX_VOLUME: u32 = 100;`
- **Modules**: `snake_case`
  - `mod playlist_manager`
- **Filenames**: `snake_case.rs`
  - `audio_player.rs`

### Error Handling
- **Result Type**: Use `Result<T, E>` for functions that can fail.
- **Propagation**: Use the `?` operator to propagate errors.
- **Custom Errors**: Define meaningful error types using `thiserror` (if available) or standard enum errors when a module has multiple failure modes.
- **Unwrap/Expect**:
  - Avoid `unwrap()` in production logic/libraries.
  - Use `expect("reason")` if you are certain it won't fail and want to document why.
  - Allowed in `main` setup or tests.

### Logging
- Use the `log` crate macros: `error!`, `warn!`, `info!`, `debug!`, `trace!`.
- **Error**: Critical failures that might stop the app.
- **Warn**: Recoverable issues or unexpected states.
- **Info**: High-level application events (startup, shutdown, configuration load).
- **Debug**: Detailed flow information (button clicks, state transitions).

### Architecture & specific patterns
- **UI (Slint)**:
  - Define UI in `.slint` files.
  - Use `slint::include_modules!()` in Rust.
  - Wire callbacks in `main` or specific UI managers.
  - Keep UI logic separate from core business logic.
- **Concurrency**:
  - Use `tokio` for async operations where suitable.
  - Use `std::thread` for dedicated long-running background workers (decoder, player).
  - **Communication**: Use `tokio::sync::broadcast` for messaging between components (Event Bus pattern).
  - **Shared State**: Use `Arc<Mutex<T>>` or `Rc` (for UI thread local) cautiously.
- **Project Structure**:
  - `main.rs`: Entry point, setup, wiring.
  - `audio_decoder.rs`: Audio file decoding logic.
  - `playlist.rs`: Data structure for playlist management.
  - `playlist_manager.rs`: Logic for manipulating the playlist.
  - `ui_manager.rs`: UI state handling and updates.
  - `protocol.rs`: Message definitions for the event bus.

### Documentation
- **Public API**: Use doc comments (`///`) for public structs, enums, and functions.
- **Complex Logic**: Add brief comments (`//`) explaining *why* complex blocks exist.
- **TODOs**: Mark incomplete work with `// TODO: description`.

---

## 3. Existing Rules & configurations

### Cursor Rules
This project includes specific rules for the Cursor editor.
Refer to `.cursorrules` in the project root for specific AI behavior instructions.

### CLAUDE.md
The `CLAUDE.md` file contains high-level project guidelines.
If there is a conflict, `CLAUDE.md` and this file (`AGENTS.md`) take precedence for development standards.

### Copilot
Check `.github/copilot-instructions.md` if available for GitHub Copilot specific instructions.
(Note: Currently not present in this repo).

---

## 4. Agent specific instructions

When implementing features or fixing bugs:
1.  **Analyze**: Understand the component interaction via the Event Bus (`protocol.rs`).
2.  **Verify**: Check if your changes affect `main.rs` wiring.
3.  **Test**: Write unit tests for logic in modules like `playlist.rs`.
4.  **UI**: If modifying UI, ensure `.slint` files compile and bindings match `main.rs`.
