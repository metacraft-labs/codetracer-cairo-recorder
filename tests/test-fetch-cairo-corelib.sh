#!/usr/bin/env bash
# scripts/fetch-cairo-corelib.sh must be re-runnable: `just prepare-ci` is run
# again in a checkout that already holds ./corelib, and a version bump in
# Cargo.toml must replace the corelib rather than keep the old one.
#
# The script runs for real (real curl, tar and filesystem); only the download
# location is a local file:// tarball, so the test needs no network.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
script="$here/scripts/fetch-cairo-corelib.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

make_release() { # version marker
  local v=$1 marker=$2 d="$work/release-$1"
  mkdir -p "$d/cairo-$v/corelib/src" "$d/cairo-$v/other"
  echo "$marker" > "$d/cairo-$v/corelib/src/lib.cairo"
  tar czf "$work/cairo-$v.tar.gz" -C "$d" "cairo-$v"
}
pin() { printf '[dependencies]\ncairo-lang-compiler = "%s"\n' "$1" > "$work/repo/Cargo.toml"; }
fetch() { # version
  (cd "$work/repo" && CAIRO_CORELIB_URL="file://$work/cairo-$1.tar.gz" \
    GITHUB_ENV="$work/github_env" bash "$script") || fail "fetch of $1 exited $?"
}
check() { # marker label
  [ -f "$work/repo/corelib/src/lib.cairo" ] || fail "$2: corelib/src/lib.cairo missing"
  [ "$(cat "$work/repo/corelib/src/lib.cairo")" = "$1" ] || fail "$2: corelib holds '$(cat "$work/repo/corelib/src/lib.cairo")', expected '$1'"
  [ ! -e "$work/repo/corelib/corelib" ] || fail "$2: corelib was nested inside the existing corelib"
  local extra
  extra=$(find "$work/repo" -mindepth 1 -maxdepth 1 ! -name Cargo.toml ! -name corelib)
  [ -z "$extra" ] || fail "$2: left behind: $extra"
}

mkdir -p "$work/repo"
make_release 1.0.0 "corelib 1.0.0"
make_release 2.0.0 "corelib 2.0.0"
pin 1.0.0

fetch 1.0.0; check "corelib 1.0.0" "first run"
fetch 1.0.0; check "corelib 1.0.0" "second run"
fetch 1.0.0; check "corelib 1.0.0" "third run"
pin 2.0.0
fetch 2.0.0; check "corelib 2.0.0" "after a version bump"
grep -qx "CAIRO_CORELIB_DIR=$work/repo/corelib/src" "$work/github_env" || fail "CAIRO_CORELIB_DIR not exported"
echo "test-fetch-cairo-corelib: ok"
