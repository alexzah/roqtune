# AGENTS.md

## 1. Purpose
Project-specific instructions for AI agents and contributors working in this repository.

## 2. Priority Rules
1. Do exactly what the current user request asks.
2. Keep changes minimal and scoped to the task.
3. Preserve behavior unless behavior change is explicitly requested.
4. Maintain performance and low resource usage.

## 3. Required Workflow
1. Analyze affected component flow through `src/protocol.rs` (event bus messages).
2. Implement only in relevant modules.
3. Verify `main`/runtime wiring when callbacks, bus messages, or startup flow change.
4. Run validation commands before handoff (see section 4).

## 4. Validation Commands
Run these before handing work back:
- `cargo fmt`
- `cargo clippy -- -D warnings`
- `cargo test`

If any cannot be run, state that explicitly and why.

## 5. Non-Negotiables
- Never commit changes. The user manages commits.
- Do not use destructive git/file commands unless explicitly requested.
- Do not use raw shell replacement tools (for example `sed`) to edit source files.
- Keep edits straightforward and reviewable.
- Do not modify unrelated code unless required for correctness of the requested task.

## 6. Architecture Snapshot
- UI: Slint (`src/roqtune.slint`, `src/ui/components/*.slint`, `src/ui/types.slint`).
- Entry/runtime orchestration:
  - `src/main.rs`
  - `src/app_runtime.rs`
  - `src/app_bootstrap/services.rs`
  - `src/app_callbacks/*`
  - `src/runtime/audio_runtime_reactor.rs`
- Core subsystems:
  - `src/audio/*` (decoder/player/probe/output options)
  - `src/playlist/*`
  - `src/library/*`
  - `src/metadata/*`
  - `src/integration/*`
  - `src/cast/*`
  - `src/ui_manager.rs`
- Shared models/persistence:
  - `src/protocol.rs`
  - `src/layout.rs`
  - `src/config.rs`
  - `src/config_persistence.rs`
  - `src/db_manager.rs`

## 7. Persistence Boundaries
- Layout-owned settings must persist in `layout.toml` (`LayoutConfig`).
- `config.toml` is for non-layout runtime preferences.
- Keep this boundary intact unless explicitly asked to change it.

## 8. Code Style
- Idiomatic Rust, clear naming, small focused functions.
- Avoid `unsafe` unless absolutely required and documented.
- Prefer `Result` + `?` propagation.
- Avoid `unwrap()` in production paths (acceptable in tests/setup with clear reason).
- Use `log` macros with appropriate severity.
- Add/maintain rustdoc for public APIs.

## 9. UI / Slint Guidelines
- Keep business logic in Rust modules, not in Slint where avoidable.
- When changing UI contracts, verify Slint callbacks/properties and Rust bindings stay aligned.
- Ensure `.slint` changes still compile and behavior is consistent with callback wiring.

## 10. Testing Expectations
- Add or update tests for logic changes where practical.
- For orchestration-heavy behavior (playback transitions, seeking, auto-advance, drag/drop), verify cross-component flow.
- Manual test guidance is acceptable when interactive validation is required.
