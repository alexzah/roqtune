# CLAUDE.md - Rust Music Player Project Guidelines

## Build Commands
- Build project: `cargo build`
- Run application: `cargo run`
- Release build: `cargo build --release`
- Check code for errors: `cargo check`
- Format code: `cargo fmt`
- Lint code: `cargo clippy`

## Code Style Guidelines
- **Imports**: Group standard library imports first, then external crates, then local modules.
- **Error Handling**: Use Result with meaningful error types; propagate errors with `?` operator.
- **Naming**: Use snake_case for variables/functions, CamelCase for types/enums/structs.
- **Comments**: Use doc comments `///` for public API documentation.
- **Threading**: Properly handle thread safety with Arc/Mutex when sharing data.
- **UI Components**: Define Slint components in separate .slint files with proper naming.
- **Logging**: Use appropriate log levels (debug, info, error) based on severity.
- **State Management**: Use proper encapsulation for state with getters/setters.

## Project Structure
- UI definitions in .slint files
- Audio handling logic in audio_decoder.rs
- Playlist management in playlist.rs
- Main application logic in main.rs