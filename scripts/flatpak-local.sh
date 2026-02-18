#!/usr/bin/env bash
set -euo pipefail

APP_ID="io.github.roqtune.Roqtune"
APP_BRANCH="stable"
RUNTIME="org.freedesktop.Platform//24.08"
SDK="org.freedesktop.Sdk//24.08"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
FLATPAK_DIR="${ROOT_DIR}/flatpak"
MANIFEST="${FLATPAK_DIR}/${APP_ID}.json"
BUILD_DIR="${FLATPAK_DIR}/.build"
REPO_DIR="${FLATPAK_DIR}/.repo"
DIST_DIR="${FLATPAK_DIR}/dist"
BUNDLE_PATH="${DIST_DIR}/${APP_ID}.flatpak"

usage() {
    cat <<'EOF'
Usage: scripts/flatpak-local.sh [--bundle-only]

Builds roqtune from the current workspace, creates a local Flatpak bundle, and
installs it for the current user.

Options:
  --bundle-only  Build the .flatpak bundle but skip local installation.
EOF
}

BUNDLE_ONLY=0
for arg in "$@"; do
    case "${arg}" in
        --bundle-only)
            BUNDLE_ONLY=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: ${arg}" >&2
            usage
            exit 1
            ;;
    esac
done

for cmd in cargo flatpak flatpak-builder; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        echo "Missing required command: ${cmd}" >&2
        exit 1
    fi
done

ensure_flatpak_ref_installed() {
    local ref="$1"
    local ref_label="$2"

    if flatpak info "${ref}" >/dev/null 2>&1; then
        return 0
    fi

    echo "Missing Flatpak ${ref_label}: ${ref}"
    echo "Attempting: flatpak install ${ref}"
    if flatpak install -y "${ref}"; then
        return 0
    fi

    echo "Unable to install ${ref} automatically." >&2
    echo "Run 'flatpak install ${ref}' manually, then retry." >&2
    return 1
}

ensure_flatpak_ref_installed "${RUNTIME}" "runtime"
ensure_flatpak_ref_installed "${SDK}" "SDK"

cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"

mkdir -p "${DIST_DIR}"

flatpak-builder \
    --user \
    --force-clean \
    --disable-rofiles-fuse \
    --default-branch="${APP_BRANCH}" \
    --repo="${REPO_DIR}" \
    "${BUILD_DIR}" \
    "${MANIFEST}"

flatpak build-bundle "${REPO_DIR}" "${BUNDLE_PATH}" "${APP_ID}" "${APP_BRANCH}"

if [[ "${BUNDLE_ONLY}" -eq 0 ]]; then
    flatpak install --user -y --reinstall "${BUNDLE_PATH}"
fi

echo
echo "Flatpak bundle: ${BUNDLE_PATH}"
if [[ "${BUNDLE_ONLY}" -eq 0 ]]; then
    echo "Installed app ID: ${APP_ID}"
fi
echo "Run with: flatpak run ${APP_ID}"
