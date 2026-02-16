# Install Guide

This document covers local development installs and local Flatpak packaging for `roqtune`.

## Prerequisites

- Rust + Cargo (`rustup` recommended)
- Linux desktop environment (current supported target)

For Flatpak packaging:

- `flatpak`
- `flatpak-builder`
- Required installed runtime/SDK:
  - `org.freedesktop.Platform//24.08`
  - `org.freedesktop.Sdk//24.08`

## Native Build / Run

- Build debug binary: `cargo build`
- Run debug build: `cargo run`
- Build release binary: `cargo build --release`
- Run release build: `cargo run --release`

### Chromecast Notes

- Add a `Cast` button to a button cluster (default Utility preset includes it).
- Direct cast streams original file bytes without modification.
- Optional fallback (`Settings > Audio > Cast transcode fallback`) uses WAV PCM for receivers that reject direct streams.

## Flatpak (Local Bundle + Local Install)

Use this command from the repo root:

```bash
./scripts/flatpak-local.sh
```

What it does:

1. Builds the latest local `roqtune` source in release mode.
2. Builds a Flatpak repo from `flatpak/io.github.roqtune.Roqtune.json`.
3. Produces a local bundle at `flatpak/dist/io.github.roqtune.Roqtune.flatpak`.
4. Installs (or reinstalls) that local bundle for your user.

The script does not add or use a specific remote automatically. It only uses
standard `flatpak install <ref>` attempts for missing runtime/SDK refs before
failing.

Run the installed Flatpak:

```bash
flatpak run io.github.roqtune.Roqtune
```

### Bundle Only (Skip Install)

```bash
./scripts/flatpak-local.sh --bundle-only
```

Install manually later:

```bash
flatpak install --user flatpak/dist/io.github.roqtune.Roqtune.flatpak
```
