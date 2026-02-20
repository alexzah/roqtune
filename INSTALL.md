# Install Guide

This document covers local development installs and local Flatpak packaging for `roqtune`.

## Prerequisites

- Rust + Cargo (`rustup` recommended)
- Linux desktop environment (current supported target)
- System keyring installed and active (for native runs, external integration credentials are saved there)
  - For systems without an existing keyring, gnome-keyring is a good default and works with minimal setup
  - Flatpak runs use the system Secret Service via `org.freedesktop.secrets` (granted in the Flatpak manifest)

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

## Flatpak (Local Bundle + Local Install)

Use this command from the repo root:

```bash
./scripts/flatpak-local.sh
```

What it does:

1. Builds the latest local `roqtune` source in release mode.
2. Builds a Flatpak repo from `flatpak/io.github.alexzah.roqtune.json`.
3. Produces a local bundle at `flatpak/dist/io.github.alexzah.roqtune.flatpak`.
4. Installs (or reinstalls) that local bundle for your user.

The script does not add or use a specific remote automatically. It only uses
standard `flatpak install <ref>` attempts for missing runtime/SDK refs before
failing.

Run the installed Flatpak:

```bash
flatpak run io.github.alexzah.roqtune
```

### Bundle Only (Skip Install)

```bash
./scripts/flatpak-local.sh --bundle-only
```

Install manually later:

```bash
flatpak install --user flatpak/dist/io.github.alexzah.roqtune.flatpak
```
