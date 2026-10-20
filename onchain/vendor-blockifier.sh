#!/usr/bin/env bash
# Produce vendor/blockifier: the crates.io release Cargo.lock pins, verified against
# Cargo.lock's own checksum, with blockifier-patch/apply.py applied. The patched tree is
# a BUILD INPUT, never committed (it is ~2.8 MB of StarkWare's source); what is committed
# is the patch, and this script is the only way the tree comes to exist.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"
version="$(awk '/^name = "blockifier"$/{getline; sub(/^version = "/,""); sub(/"$/,""); print; exit}' Cargo.lock)"
want="$(awk '/^name = "blockifier"$/{f=1} f&&/^checksum = /{sub(/^checksum = "/,""); sub(/"$/,""); print; exit}' Cargo.lock)"
if [ -z "$version" ]; then
  echo "vendor-blockifier.sh: Cargo.lock names no blockifier version" >&2
  exit 2
fi
dest="vendor/blockifier"
stamp="$dest/.codetracer-patch-stamp"
patch_id="$version $(cat blockifier-patch/apply.py blockifier-patch/bt_trace.rs | sha256sum | cut -d' ' -f1)"
if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$patch_id" ]; then
  exit 0
fi
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -sSfL -A codetracer-cairo-recorder \
  "https://static.crates.io/crates/blockifier/blockifier-${version}.crate" -o "$tmp/b.crate"
got="$(sha256sum "$tmp/b.crate" | cut -d' ' -f1)"
# A lock with no checksum for the crate (the [patch] entry replaces it) falls back to the
# checksum recorded beside this script for that version.
if [ -z "$want" ]; then
  want="$(awk -v v="$version" '$1==v{print $2}' blockifier-patch/CHECKSUMS)"
fi
if [ "$got" != "$want" ]; then
  echo "vendor-blockifier.sh: blockifier-${version}.crate has sha256 $got, expected $want" >&2
  exit 1
fi
tar -xzf "$tmp/b.crate" -C "$tmp"
rm -rf "$dest"
mkdir -p vendor
mv "$tmp/blockifier-${version}" "$dest"
python3 blockifier-patch/apply.py "$dest"
echo "$patch_id" > "$stamp"
