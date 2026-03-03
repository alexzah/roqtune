#!/usr/bin/env sh
set -eu

cc_bin="${CC:-cc}"

if command -v mold >/dev/null 2>&1; then
  exec "$cc_bin" -fuse-ld=mold "$@"
fi

exec "$cc_bin" "$@"
