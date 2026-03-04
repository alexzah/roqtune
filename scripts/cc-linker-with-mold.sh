#!/usr/bin/env sh
set -eu

# Use explicit CC only when provided; otherwise default to cc.
cc_bin="${CC:-cc}"

try_link() {
  "$@" >/dev/null 2>&1
}

# Prefer mold when both the binary exists and the selected compiler accepts it.
if command -v mold >/dev/null 2>&1 && try_link "$cc_bin" -fuse-ld=mold -Wl,--version; then
  exec "$cc_bin" -fuse-ld=mold "$@"
fi

# Flatpak SDKs can lack plain `ld`; fall back to lld/bfd when available.
if command -v ld.lld >/dev/null 2>&1 && try_link "$cc_bin" -fuse-ld=lld -Wl,--version; then
  exec "$cc_bin" -fuse-ld=lld "$@"
fi

if command -v ld.bfd >/dev/null 2>&1 && try_link "$cc_bin" -fuse-ld=bfd -Wl,--version; then
  exec "$cc_bin" -fuse-ld=bfd "$@"
fi

# Normal toolchain path (works on most non-Flatpak hosts).
if "$cc_bin" "$@"; then
  exit 0
fi

exit 1
