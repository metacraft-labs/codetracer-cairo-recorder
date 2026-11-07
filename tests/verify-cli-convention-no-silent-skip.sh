#!/usr/bin/env bash
# Verify that the codetracer-cairo-recorder CLI complies with
# `Recorder-CLI-Conventions.md` (no silent skip — every assertion
# either passes or fails loudly):
#
#   * `--format` is absent from `--help` (CTFS-only — convention §4)
#   * `CODETRACER_FORMAT` is absent from `--help` (convention §5)
#   * `--out-dir` and `--version` are present in `--help` (§3)
#   * `--help` mentions `ct print` (the canonical conversion tool, §4)
#   * `CODETRACER_CAIRO_RECORDER_OUT_DIR` is referenced in source so the
#     env-var fallback (§5) cannot regress silently.
#
# Wire-up: see `Justfile` (`just lint` and `just test` both run this
# script).
#
# Exit codes:
#   0  all assertions held
#   1  at least one assertion failed (the failing line is printed to
#      stderr and the script exits at the first failure for clarity)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Build the binary if it isn't already built (cargo build is a no-op
# when nothing has changed).  We use --quiet so the output of this
# script stays focused on verification results.
( cd "${REPO_ROOT}" && cargo build --locked --quiet )

# Ask cargo where it put the binary instead of assuming `target/`.
# `CARGO_TARGET_DIR` (set on the CI runners) or a `build.target-dir`
# in a cargo config moves the output elsewhere.  A hard-coded
# `target/debug` then either misses the fresh build entirely or, worse,
# picks up a stale binary from an earlier local build and verifies that
# instead of the code just compiled.  `cargo metadata` resolves every
# one of those settings exactly as `cargo build` did:
# https://doc.rust-lang.org/cargo/commands/cargo-metadata.html
#
# The JSON is parsed with a bash regex so the script needs no tool
# beyond the ones its edge already registers.  JSON strings may carry
# escapes; `\\` and `\"` are decoded, and any other escape (`\uXXXX`,
# `\n`, ...) is refused loudly rather than guessed at.
CARGO_METADATA="$( cd "${REPO_ROOT}" && cargo metadata --no-deps --format-version 1 )"
TARGET_DIR_RE='"target_directory":"(([^"\\]|\\.)*)"'
if [[ ! "${CARGO_METADATA}" =~ ${TARGET_DIR_RE} ]]; then
  echo "ERROR: 'cargo metadata' reported no target_directory" >&2
  exit 1
fi
TARGET_DIR_JSON="${BASH_REMATCH[1]}"
# Decode left to right: a backslash always pairs with the character
# after it, so the JSON `\\n` is a backslash followed by `n`, never a
# newline.  Decoding with two global substitutions would get that wrong.
SIMPLE_ESCAPE_RE='^\\(["\\])'
TARGET_DIR=""
while [[ -n "${TARGET_DIR_JSON}" ]]; do
  if [[ "${TARGET_DIR_JSON}" =~ ${SIMPLE_ESCAPE_RE} ]]; then
    TARGET_DIR+="${BASH_REMATCH[1]}"
    TARGET_DIR_JSON="${TARGET_DIR_JSON:2}"
  elif [[ "${TARGET_DIR_JSON:0:1}" == '\' ]]; then
    echo "ERROR: unsupported JSON escape in cargo's target_directory" \
      "at: ${TARGET_DIR_JSON}" >&2
    exit 1
  else
    TARGET_DIR+="${TARGET_DIR_JSON:0:1}"
    TARGET_DIR_JSON="${TARGET_DIR_JSON:1}"
  fi
done

BIN="${TARGET_DIR}/debug/codetracer-cairo-recorder"
if [[ ! -x "${BIN}" ]]; then
  echo "ERROR: recorder binary not found at ${BIN}" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

assert_absent() {
  # assert_absent <needle> <haystack-description> <haystack>
  local needle="$1"
  local desc="$2"
  local haystack="$3"
  if grep -qF -- "${needle}" <<< "${haystack}"; then
    echo "FAIL: ${desc} must NOT contain '${needle}'" >&2
    echo "----- ${desc} -----" >&2
    echo "${haystack}" >&2
    echo "-------------------" >&2
    exit 1
  fi
  echo "ok: '${needle}' absent from ${desc}"
}

assert_present() {
  # assert_present <needle> <haystack-description> <haystack>
  local needle="$1"
  local desc="$2"
  local haystack="$3"
  if ! grep -qF -- "${needle}" <<< "${haystack}"; then
    echo "FAIL: ${desc} must contain '${needle}'" >&2
    echo "----- ${desc} -----" >&2
    echo "${haystack}" >&2
    echo "-------------------" >&2
    exit 1
  fi
  echo "ok: '${needle}' present in ${desc}"
}

# ---------------------------------------------------------------------------
# Top-level --help
# ---------------------------------------------------------------------------

TOP_HELP="$("${BIN}" --help)"

assert_absent "--format" "top-level --help" "${TOP_HELP}"
assert_absent "CODETRACER_FORMAT" "top-level --help" "${TOP_HELP}"
assert_present "--help" "top-level --help" "${TOP_HELP}"
assert_present "--version" "top-level --help" "${TOP_HELP}"
assert_present "ct print" "top-level --help" "${TOP_HELP}"

# ---------------------------------------------------------------------------
# `record` subcommand --help
# ---------------------------------------------------------------------------

RECORD_HELP="$("${BIN}" record --help)"

assert_absent "--format" "record --help" "${RECORD_HELP}"
assert_absent "CODETRACER_FORMAT" "record --help" "${RECORD_HELP}"
assert_present "--out-dir" "record --help" "${RECORD_HELP}"

# ---------------------------------------------------------------------------
# `trace-starknet` subcommand --help
# ---------------------------------------------------------------------------

TRACE_HELP="$("${BIN}" trace-starknet --help)"

assert_absent "--format" "trace-starknet --help" "${TRACE_HELP}"
assert_absent "CODETRACER_FORMAT" "trace-starknet --help" "${TRACE_HELP}"
assert_present "--out-dir" "trace-starknet --help" "${TRACE_HELP}"

# ---------------------------------------------------------------------------
# --version output
# ---------------------------------------------------------------------------

VERSION_OUT="$("${BIN}" --version)"
assert_present "codetracer-cairo-recorder" "--version output" "${VERSION_OUT}"

# ---------------------------------------------------------------------------
# Source-level reference for the env-var fallback
# ---------------------------------------------------------------------------

# The recorder must reference CODETRACER_CAIRO_RECORDER_OUT_DIR in
# source (otherwise the env-var fallback either doesn't exist or has
# been silently removed).  We grep recursively under src/.
if ! grep -rqF "CODETRACER_CAIRO_RECORDER_OUT_DIR" "${REPO_ROOT}/src"; then
  echo "FAIL: CODETRACER_CAIRO_RECORDER_OUT_DIR must be referenced in src/" >&2
  exit 1
fi
echo "ok: CODETRACER_CAIRO_RECORDER_OUT_DIR referenced in src/"

if ! grep -rqF "CODETRACER_CAIRO_RECORDER_DISABLED" "${REPO_ROOT}/src"; then
  echo "FAIL: CODETRACER_CAIRO_RECORDER_DISABLED must be referenced in src/" >&2
  exit 1
fi
echo "ok: CODETRACER_CAIRO_RECORDER_DISABLED referenced in src/"

echo "verify-cli-convention-no-silent-skip: all checks passed"
