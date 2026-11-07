#!/usr/bin/env bash
# Fetch the Cairo corelib version pinned in Cargo.toml into ./corelib (the
# recorder loads it via $CAIRO_CORELIB_DIR). CAIRO_CORELIB_URL overrides the
# release tarball location.
#
# Re-runnable: a ./corelib already holding the pinned version is kept, and any
# other ./corelib (an older pin, or one of unknown origin) is replaced. The
# download is unpacked in a temporary directory and swapped in only once it
# is complete, so a failed run leaves the previous corelib intact.
set -euo pipefail
CAIRO_VERSION=$(grep 'cairo-lang-compiler' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
[ -n "$CAIRO_VERSION" ] || { echo "cairo-lang-compiler version not found in Cargo.toml" >&2; exit 1; }
stamp=corelib/.cairo-version

if [ ! -f "$stamp" ] || [ "$(cat "$stamp")" != "$CAIRO_VERSION" ]; then
  url="${CAIRO_CORELIB_URL:-https://github.com/starkware-libs/cairo/archive/refs/tags/v${CAIRO_VERSION}.tar.gz}"
  tmp=$(mktemp -d "$PWD/.corelib-fetch.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL "$url" -o "$tmp/cairo-src.tar.gz"
  tar xzf "$tmp/cairo-src.tar.gz" -C "$tmp" "cairo-${CAIRO_VERSION}/corelib"
  echo "$CAIRO_VERSION" > "$tmp/cairo-${CAIRO_VERSION}/corelib/.cairo-version"
  rm -rf corelib
  mv "$tmp/cairo-${CAIRO_VERSION}/corelib" corelib
fi

echo "CAIRO_CORELIB_DIR=$PWD/corelib/src" >> "${GITHUB_ENV:-/dev/null}"
