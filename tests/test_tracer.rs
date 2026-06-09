// The Sequence/at-index docblock layout below intentionally mixes
// prose with multi-line continuations; clippy::doc_lazy_continuation
// flags the indentation but the wording is the same that appears in
// the surrounding test-tracing literature.
#![allow(clippy::doc_lazy_continuation)]

//! Integration tests for the Cairo tracer.
//!
//! These tests cover three areas:
//!
//! 1. Pure-data conversion of snforge JSON traces to the in-memory
//!    [`TraceEvent`] shape — these don't touch the recorder writer at
//!    all and are the source of truth for the snforge parser.
//! 2. End-to-end recording of a real Cairo program through the
//!    Cairo → Sierra → CASM → CTFS pipeline; assertions are made on
//!    the CTFS bundle either directly (via the magic bytes /
//!    `.ct` file presence) or via `ct print` (the canonical conversion
//!    tool shipped with `codetracer-trace-format-nim`).
//! 3. Env-var contract for the recorder CLI
//!    (`CODETRACER_CAIRO_RECORDER_OUT_DIR` /
//!    `CODETRACER_CAIRO_RECORDER_DISABLED`).
//!
//! History note: pre-2026-05-08 the recorder shipped a `--format
//! ctfs|binary|json` flag and this file held a parallel set of tests
//! that asserted on a legacy 3-file `trace.json`/`trace_metadata.json`/
//! `trace_paths.json` output shape (the M33 commit b31d8d7 already broke
//! those tests; they were quarantined with `#[ignore]`).  When the
//! convention switched to CTFS-only, the `--format` flag was removed
//! and those quarantined tests were deleted — they were redundant with
//! the CTFS coverage in `test_ctfs_audit.rs` and could no longer be
//! revived without rewriting against a CTFS reader.  See
//! `AUDIT-CTFS-2026-05.md` ("Convention compliance follow-up") for the
//! full record.

use std::path::PathBuf;
use std::process::Command;

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
///
/// The Cairo recorder is CTFS-only; tests that need to make
/// content-level assertions on a recorded trace pipe the `.ct`
/// container through `ct-print --json` and assert on the resulting
/// JSON.  This is the same workflow that `Recorder-CLI-Conventions.md`
/// §4 prescribes for downstream tools / golden snapshots.
fn ct_print_path() -> PathBuf {
    // The trace-format-nim sibling lives at a fixed relative path within
    // the workspace.  Tests skip gracefully when it's not present (e.g.
    // when this crate is built outside the metacraft workspace).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX))
}

/// Helper: path to the starknet test-programs directory.
fn starknet_test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs/starknet")
}

/// Helper: path to the Cairo test-programs directory.
fn cairo_test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs/cairo")
}

/// Helper: collect every `.ct` file in `out_dir`.
fn ct_files_in(out_dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(out_dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect()
}

// ===========================================================================
// snforge parse / convert — pure data (no recorder writer involved)
// ===========================================================================

#[test]
fn test_parse_mock_snforge_trace() {
    use codetracer_cairo_recorder::starknet::{parse_snforge_trace, TraceEntry};

    let trace_path = starknet_test_dir().join("mock_trace.json");
    let entries = parse_snforge_trace(&trace_path).expect("should parse mock trace");

    assert_eq!(entries.len(), 6, "mock trace should have 6 entries");

    // Verify entry types in order.
    assert!(
        matches!(&entries[0], TraceEntry::ContractCall { selector, .. } if selector == "increase_balance")
    );
    assert!(matches!(&entries[1], TraceEntry::StorageRead { value, .. } if value == "0"));
    assert!(
        matches!(&entries[2], TraceEntry::StorageWrite { old_value, new_value, .. }
        if old_value == "0" && new_value == "42")
    );
    assert!(matches!(&entries[3], TraceEntry::Event { contract, .. } if contract == "0x2"));
    assert!(
        matches!(&entries[4], TraceEntry::ContractCall { selector, .. } if selector == "get_balance")
    );
    assert!(matches!(&entries[5], TraceEntry::StorageRead { value, .. } if value == "42"));
}

#[test]
fn test_starknet_contract_calls_captured() {
    use codetracer_cairo_recorder::starknet::{
        convert_snforge_trace, parse_snforge_trace, TraceEvent,
    };

    let trace_path = starknet_test_dir().join("mock_trace.json");
    let entries = parse_snforge_trace(&trace_path).unwrap();
    let events = convert_snforge_trace(&entries);

    let call_names: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Call { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        call_names.contains(&"0x2::increase_balance"),
        "should capture increase_balance call, got: {:?}",
        call_names
    );
    assert!(
        call_names.contains(&"0x2::get_balance"),
        "should capture get_balance call, got: {:?}",
        call_names
    );
}

#[test]
fn test_starknet_storage_ops_captured() {
    use codetracer_cairo_recorder::starknet::{
        convert_snforge_trace, parse_snforge_trace, TraceEvent,
    };

    let trace_path = starknet_test_dir().join("mock_trace.json");
    let entries = parse_snforge_trace(&trace_path).unwrap();
    let events = convert_snforge_trace(&entries);

    let call_names: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Call { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        call_names.contains(&"0x2::storage_read"),
        "should capture storage_read, got: {:?}",
        call_names
    );
    assert!(
        call_names.contains(&"0x2::storage_write"),
        "should capture storage_write, got: {:?}",
        call_names
    );

    // Verify the storage write captured old and new values.
    let variable_pairs: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Variable { name, value } => Some((name.as_str(), value.as_str())),
            _ => None,
        })
        .collect();

    assert!(
        variable_pairs.contains(&("old_value", "0")),
        "should capture old_value=0"
    );
    assert!(
        variable_pairs.contains(&("new_value", "42")),
        "should capture new_value=42"
    );
}

#[test]
fn test_starknet_events_captured() {
    use codetracer_cairo_recorder::starknet::{
        convert_snforge_trace, parse_snforge_trace, TraceEvent,
    };

    let trace_path = starknet_test_dir().join("mock_trace.json");
    let entries = parse_snforge_trace(&trace_path).unwrap();
    let events = convert_snforge_trace(&entries);

    let call_names: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Call { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        call_names.contains(&"0x2::emit_event"),
        "should capture emit_event, got: {:?}",
        call_names
    );

    // Verify event keys and data are captured.
    let variable_pairs: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            TraceEvent::Variable { name, value } => Some((name.as_str(), value.as_str())),
            _ => None,
        })
        .collect();

    assert!(
        variable_pairs.contains(&("event_keys", "[BalanceIncreased]")),
        "should capture event keys"
    );
    assert!(
        variable_pairs.contains(&("event_data", "[0x1, 42, 42]")),
        "should capture event data"
    );
}

// ===========================================================================
// CTFS content via `ct-print` — replaces the legacy `--format json` tests
// ===========================================================================

/// Record `flow_test.cairo`, then convert the produced `.ct` container to
/// JSON via `ct-print` and assert on:
///
/// 1. **Structural anchors** (legacy layer): `ct-print --json` output
///    contains the source filename / variable names / canonical felt252
///    values somewhere in the textual rendering.
/// 2. **Exact decoded values** (the layer enabled by `ct-print --full`):
///    the `flow_test.cairo` program executes `(10 + 32) * 2 + 10 = 94`
///    via the `compute()` function, with intermediate let-bindings
///    `a=10`, `b=32`, `sum_val=42`, `doubled=84`, `final_result=94`.
///    Each binding must surface in the trace as a step event with a
///    decoded `Int` ValueRecord whose `i` field matches the literal
///    value from the source program.
///
/// Pre-2026-05-08 this assertion was made directly on a recorder-emitted
/// `trace.json` file (via `--format json`).  The convention now mandates
/// CTFS-only output; `ct print` is the canonical conversion tool.  See
/// `Recorder-CLI-Conventions.md` §4.  `ct-print --full` (added 2026-05
/// in `codetracer-trace-format-nim`) is what enables the exact-value
/// layer — its output is a deterministic JSON document with every CBOR
/// `ValueRecord` decoded to a structured form like
/// `{"kind":"Int","i":42,"type_id":7}`.
#[test]
fn test_recorded_trace_via_ct_print_json() {
    let ct_print = ct_print_path();
    if !ct_print.exists() {
        eprintln!(
            "SKIP: ct-print not found at {} — only available within the \
             metacraft workspace where codetracer-trace-format-nim is a sibling.",
            ct_print.display()
        );
        return;
    }

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = cairo_test_dir().join("flow_test.cairo");
    codetracer_cairo_recorder::recorder::record(&source_path, &out_dir)
        .expect("recorder::record should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    // -----------------------------------------------------------------
    // Layer 1 (legacy): ct-print --json — substring presence checks.
    // Kept as a safety net so a regression in the textual rendering
    // is caught even if --full's JSON shape evolves.
    // -----------------------------------------------------------------
    let output = Command::new(&ct_print)
        .args(["--json"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print");

    assert!(
        output.status.success(),
        "ct-print --json should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_json = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout_json.is_empty(),
        "ct-print --json produced empty output"
    );
    let mentions_value = ["10", "32", "42", "84", "94"]
        .iter()
        .any(|v| stdout_json.contains(v));
    assert!(
        mentions_value,
        "ct-print --json output should mention one of the program's \
         computed felt252 values (10/32/42/84/94); got:\n{stdout_json}"
    );

    // -----------------------------------------------------------------
    // Layer 2 (the upgrade): ct-print --full — exact decoded values.
    // -----------------------------------------------------------------
    let full_output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");

    assert!(
        full_output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&full_output.stderr)
    );

    let doc: serde_json::Value = serde_json::from_slice(&full_output.stdout)
        .expect("ct-print --full should emit valid JSON");

    // ----- Function table: compute() and main() must both appear ------
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        functions.iter().any(|f| f.ends_with("::compute")),
        "expected `compute` in functions table; got {:?}",
        functions
    );
    assert!(
        functions.iter().any(|f| f.ends_with("::main")),
        "expected `main` in functions table; got {:?}",
        functions
    );

    // ----- Path table: the canonical fixture path must appear ---------
    let paths: Vec<&str> = doc["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("flow_test.cairo")),
        "expected flow_test.cairo in paths table; got {:?}",
        paths
    );

    // ----- Step / call counts ----------------------------------------
    // The Cairo Sierra runner emits one step per executed source line
    // for both `compute` (lines 1..6) and `main` (lines 7, 10, 11),
    // for a total of 10 step events.  Two call_entry events: compute,
    // then main.  These are stable properties of the canonical fixture
    // — if they change, that's a real regression to investigate, not
    // a flake.
    let counts = &doc["counts"];
    assert_eq!(
        counts["steps"].as_u64(),
        Some(10),
        "expected 10 step events for flow_test.cairo; counts={counts}",
    );
    assert_eq!(
        counts["calls"].as_u64(),
        Some(2),
        "expected 2 call events (compute + main); counts={counts}",
    );

    let events = doc["events"].as_array().expect("events array");

    // ----- Call sequence: main first, then compute (dynamic order) ----
    // The recorder DFS-walks the static call graph from `main`, so the
    // first call_entry event is always the program entry point and the
    // callees follow in source-text order as the DFS recurses through
    // their invocation sites.  See `tracer.rs::emit_function_dfs`.
    let call_sequence: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .filter_map(|e| e["function"].as_str())
        .collect();
    assert_eq!(
        call_sequence.len(),
        2,
        "expected exactly 2 call_entry events; got {:?}",
        call_sequence
    );
    assert!(
        call_sequence[0].ends_with("::main"),
        "expected first call to be `main` (program entry point); got {:?}",
        call_sequence
    );
    assert!(
        call_sequence[1].ends_with("::compute"),
        "expected second call to be `compute` (called from main); got {:?}",
        call_sequence
    );

    // ----- Exact decoded variable values ------------------------------
    // Collect every (varname, i64) pair surfaced by step events.  These
    // come from the recorder writing `ValueRecord::Int` CBOR blobs, then
    // ct-print --full decoding them back to `{"kind":"Int","i":<n>,...}`.
    let observed_vars: Vec<(String, i64)> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| {
            e["vars"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .filter_map(|v| {
            let name = v["varname"].as_str()?.to_string();
            let value = &v["value"];
            // The cairo recorder encodes felt252 values as ValueRecord::Int.
            // If something else surfaces (e.g. BigInt for out-of-range felts),
            // fail loudly so the test author can decide whether to extend
            // the assertions or accept the new variant.
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "variable `{}` should decode as Int, got {}; \
                 if a new ValueRecord variant has landed for cairo felts, \
                 extend this test to assert on it explicitly rather than \
                 weakening the check",
                name,
                value
            );
            let i = value["i"]
                .as_i64()
                .unwrap_or_else(|| panic!("Int.i must be i64 for `{name}`; got {value}"));
            Some((name, i))
        })
        .collect();

    // The canonical flow: a=10, b=32, sum_val=a+b=42, doubled=sum_val*2=84,
    // final_result=doubled+a=94.  The `return_value` synthetic binding for
    // the tuple result of main() also evaluates to 94 (Cairo flattens the
    // 5-tuple to its terminal felt in the trace's return slot).
    let expected: &[(&str, i64)] = &[
        ("a", 10),
        ("b", 32),
        ("sum_val", 42),
        ("doubled", 84),
        ("final_result", 94),
    ];
    for (name, value) in expected {
        assert!(
            observed_vars.iter().any(|(n, v)| n == name && v == value),
            "expected step variable `{name}` = {value} in --full output; \
             observed = {observed_vars:?}"
        );
    }
}

// ===========================================================================
// CLI env-var contract
// ===========================================================================

/// `CODETRACER_CAIRO_RECORDER_OUT_DIR` must be honoured as a fallback
/// for `--out-dir`.  Convention: `Recorder-CLI-Conventions.md` §5.
#[test]
fn test_env_out_dir_used_when_flag_omitted() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let env_out_dir = tmp_dir.path().join("via-env");

    let source_path = cairo_test_dir().join("flow_test.cairo");

    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-recorder"))
        .args(["record"])
        .arg(&source_path)
        .env("CODETRACER_CAIRO_RECORDER_OUT_DIR", &env_out_dir)
        // Make sure the env-var doesn't bleed in from the developer's shell.
        .env_remove("CODETRACER_CAIRO_RECORDER_DISABLED")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed when CODETRACER_CAIRO_RECORDER_OUT_DIR is set; \
         stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let ct_files = ct_files_in(&env_out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected the env-supplied output dir {:?} to receive the .ct container",
        env_out_dir
    );
}

/// `CODETRACER_CAIRO_RECORDER_DISABLED=1` must skip recording entirely.
/// The recorder process should still exit 0 (the Cairo recorder doesn't
/// run a separate target subprocess — it compiles & executes the Cairo
/// source itself — so "disabled" simply means "don't write any
/// trace artefacts").
#[test]
fn test_env_disabled_skips_recording() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("should-stay-empty");

    let source_path = cairo_test_dir().join("flow_test.cairo");

    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-recorder"))
        .args(["record"])
        .arg(&source_path)
        .args(["--out-dir"])
        .arg(&out_dir)
        .env("CODETRACER_CAIRO_RECORDER_DISABLED", "1")
        .output()
        .expect("failed to run recorder");

    assert!(
        output.status.success(),
        "recorder should succeed in disabled mode; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // No .ct file should have been written.
    assert!(
        !out_dir.exists() || ct_files_in(&out_dir).is_empty(),
        "no .ct container should be written when CODETRACER_CAIRO_RECORDER_DISABLED=1; \
         got files in {:?}",
        out_dir
    );
}

/// `--format` is no longer accepted at any level — clap must reject it.
/// Convention: §4 (CTFS-only).
#[test]
fn test_format_flag_rejected_by_clap() {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    let source_path = cairo_test_dir().join("flow_test.cairo");

    let output = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-recorder"))
        .args(["record"])
        .arg(&source_path)
        .args(["--out-dir"])
        .arg(&out_dir)
        .args(["--format", "json"])
        .output()
        .expect("failed to run recorder");

    assert!(
        !output.status.success(),
        "--format should be rejected by clap; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--format")
            || stderr.contains("unexpected argument")
            || stderr.contains("unrecognized")
            || stderr.contains("found argument"),
        "clap error should mention the unknown --format flag; got stderr:\n{stderr}"
    );
}

// ===========================================================================
// Per-program ct-print --full coverage tests
// ===========================================================================
//
// These tests follow the recorder-test-requirements policy
// (`metacraft-specs/policies/recorder-test-requirements.md`):
//
// * Each test records one Cairo program through the recorder's
//   normal entry point (`recorder::record`).
// * The produced `.ct` is piped through `ct-print --full --strip-paths`.
// * Assertions are made on the **decoded JSON document** with EXACT
//   counts (`assert_eq!(events.len(), N)` — never `>=`), EXACT
//   ordering of call_entry / call_exit / step events, and EXACT
//   decoded values (`value["i"] == 42`, `value["kind"] == "Int"`).
//
// `ValueRecord` variants outside the expected set are rejected with
// a hard error message asking the test author to extend the test
// rather than weaken the assertion.
//
// Pre-2026-05-13 the recorder deviated from spec on four counts —
// (1) `call_entry` was emitted in lexical source order rather than
// dynamic execution order, (2) `call_exit` events did not follow LIFO
// ordering, (3) `call_exit.return_value` was always `Void`, and (4)
// on a panic the source-level let-bindings were overwritten by the
// panic-payload felts by index.  Each deviation was tracked by a
// `#[ignore]`'d sibling test capturing the spec-correct expectation.
// All four were fixed in `src/tracer.rs::emit_function_dfs` /
// `compute_function_return_values` / `parse_let_binding_literals`;
// the `#[ignore]` attributes have been removed and the per-program
// strict tests above now assert on the spec-correct shapes directly.

/// Skip-helper: returns `Some(path)` to ct-print or logs a clear
/// `SKIP:` diagnostic and returns `None`.  The
/// `verify-cli-convention-no-silent-skip.sh` script greps for the
/// literal `SKIP:` token, so silent skips remain forbidden.
fn ct_print_or_skip(test_name: &str) -> Option<PathBuf> {
    let p = ct_print_path();
    if !p.exists() {
        eprintln!(
            "SKIP: {test_name} requires ct-print at {} — only available \
             within the metacraft workspace where codetracer-trace-format-nim \
             is a sibling.",
            p.display()
        );
        return None;
    }
    Some(p)
}

/// Record a program and return the `ct-print --full --strip-paths`
/// JSON document plus the absolute path to the source file (so the
/// caller can match `metadata.program`).  Returns `None` when
/// `ct-print` is unavailable (the caller has already emitted a
/// `SKIP:` line via `ct_print_or_skip`).
fn record_and_dump_full(test_name: &str, program: &str) -> Option<(serde_json::Value, PathBuf)> {
    let ct_print = ct_print_or_skip(test_name)?;

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = cairo_test_dir().join(program);
    codetracer_cairo_recorder::recorder::record(&source_path, &out_dir)
        .expect("recorder::record should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");

    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full should emit valid JSON");

    // Preserve the temp dir until after the JSON is parsed, then drop.
    drop(tmp_dir);

    Some((doc, source_path))
}

/// Decode every (varname, i64) pair from step events in event-emission
/// order.  Rejects any `ValueRecord` variant other than `Int` with a
/// hard error that asks the test author to extend the test rather
/// than weaken it.
fn observed_var_sequence(doc: &serde_json::Value) -> Vec<(String, i64)> {
    let events = doc["events"].as_array().expect("events array");
    let mut out = Vec::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let name = v["varname"].as_str().expect("varname str").to_string();
            let value = &v["value"];
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "variable `{}` should decode as Int, got {}; \
                 if a new ValueRecord variant has landed for cairo \
                 (e.g. Sequence/Tuple/Struct for arrays, tuples, structs), \
                 extend this test to assert on it explicitly rather than \
                 weakening the check",
                name,
                value
            );
            let i = value["i"]
                .as_i64()
                .unwrap_or_else(|| panic!("Int.i must be i64 for `{name}`; got {value}"));
            out.push((name, i));
        }
    }
    out
}

/// Like `observed_var_sequence` but skips any variable whose name is
/// in `compound_names` — for tests where a few specific bindings now
/// emit non-Int variants (e.g. Sequence/Tuple) and the rest are still
/// expected to be Int.  The strictness of the underlying helper is
/// preserved for every other variable.
fn observed_var_sequence_filtered(
    doc: &serde_json::Value,
    compound_names: &[&str],
) -> Vec<(String, i64)> {
    let events = doc["events"].as_array().expect("events array");
    let mut out = Vec::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let name = v["varname"].as_str().expect("varname str").to_string();
            if compound_names.contains(&name.as_str()) {
                continue;
            }
            let value = &v["value"];
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "variable `{}` should decode as Int, got {}; \
                 if a new ValueRecord variant has landed for cairo \
                 (e.g. Sequence/Tuple/Struct for arrays, tuples, structs), \
                 extend this test to assert on it explicitly rather than \
                 weakening the check",
                name,
                value
            );
            let i = value["i"]
                .as_i64()
                .unwrap_or_else(|| panic!("Int.i must be i64 for `{name}`; got {value}"));
            out.push((name, i));
        }
    }
    out
}

/// Decode every (varname, ValueRecord-kind) pair from step events in
/// event-emission order.  Unlike `observed_var_sequence`, this helper
/// does not constrain the kind — callers use it to assert on the full
/// kind sequence (Int / Sequence / Tuple / Struct / ...).
fn observed_var_kinds(doc: &serde_json::Value) -> Vec<(String, String)> {
    let events = doc["events"].as_array().expect("events array");
    let mut out = Vec::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(vars) = ev["vars"].as_array() else {
            continue;
        };
        for v in vars {
            let name = v["varname"].as_str().expect("varname str").to_string();
            let kind = v["value"]["kind"]
                .as_str()
                .expect("ValueRecord must carry a `kind` tag")
                .to_string();
            out.push((name, kind));
        }
    }
    out
}

/// Find the first step variable matching `target` and return its raw
/// `value` JSON object (so callers can drill into variant-specific
/// fields like `elements`, `is_slice`, `i`).  Returns `None` if no
/// step variable with that name exists in the trace.
fn find_var_value<'a>(doc: &'a serde_json::Value, target: &str) -> Option<&'a serde_json::Value> {
    let events = doc["events"].as_array()?;
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let vars = ev["vars"].as_array()?;
        for v in vars {
            if v["varname"].as_str() == Some(target) {
                return Some(&v["value"]);
            }
        }
    }
    None
}

/// Decode the call-entry sequence as a vector of bare function names
/// (last `::` segment).  The Cairo recorder fully-qualifies functions
/// as `<crate>::<crate>::<name>`; the bare-name view keeps assertions
/// stable across crate-renames.
fn observed_call_sequence(doc: &serde_json::Value) -> Vec<String> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            e["function"]
                .as_str()
                .expect("call_entry.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string()
        })
        .collect()
}

/// Decode the call-exit sequence as a vector of bare function names.
fn observed_exit_sequence(doc: &serde_json::Value) -> Vec<String> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string()
        })
        .collect()
}

/// Assert that every `step` event carries a strictly increasing
/// `step_index`.  This is the recorder's only ordering guarantee
/// against duplicates / reorderings.
fn assert_step_indices_monotonic(doc: &serde_json::Value) {
    let mut last = -1i64;
    for ev in doc["events"].as_array().expect("events array") {
        if ev["kind"] != "step" {
            continue;
        }
        let idx = ev["step_index"]
            .as_i64()
            .expect("step_index must be present on step events");
        assert!(
            idx > last,
            "step_index must strictly increase; got {idx} after {last}"
        );
        last = idx;
    }
}

/// Assert `metadata.program` ends with the expected source filename.
fn assert_metadata_program_ends_with(doc: &serde_json::Value, source_path: &std::path::Path) {
    let prog = doc["metadata"]["program"]
        .as_str()
        .expect("metadata.program str");
    let want = source_path.file_name().unwrap().to_string_lossy();
    assert!(
        prog.ends_with(&*want),
        "metadata.program {prog} must end with {want}"
    );
}

// Pre bug-fix 3 the test suite shared an `assert_all_call_exits_return_void`
// helper because every `register_return` site in the recorder passed
// `NONE_VALUE`.  Post-fix the per-program tests assert on the
// per-callee real return values directly, so the shared helper is no
// longer needed.  See `compute_function_return_values` in
// `src/tracer.rs` for the value-recovery logic.

// --- control_flow_test.cairo -----------------------------------------------

/// Records `control_flow_test.cairo` and asserts on the **exact**
/// event shape.  The program exercises if/else (with early `return`),
/// `while` loops with mutable state, and `match` expressions.  Every
/// branch is reachable under the chosen inputs so every let-binding
/// in `compute()`'s tuple return surfaces with its evaluated value.
#[test]
fn test_control_flow_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_control_flow_test_via_ct_print_full",
        "control_flow_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    // ----- Function table — entries are ID'd in DFS visit order ------
    // The recorder DFS-walks from `main`, so function ids follow the
    // dynamic call order: main → compute → classify → loop_sum →
    // match_pick.  Pre-fix the table reflected lexical declaration
    // order (`[classify, loop_sum, match_pick, compute, main]`) but
    // that violated the spec — see the bug-fix 1+2 comment in
    // `tracer.rs::emit_source_trace`.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(functions.len(), 5, "functions: {:?}", functions);
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(
        bare_fns,
        vec!["main", "compute", "classify", "loop_sum", "match_pick"]
    );

    // ----- Path table -------------------------------------------------
    let paths: Vec<&str> = doc["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("control_flow_test.cairo"));

    // ----- Counts -----------------------------------------------------
    // 28 step events: classify(6) + loop_sum(8) + match_pick(4) +
    // compute(7) + main(2) + 1 trailing return_value step.  5 calls.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(28), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(5), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );
    assert_eq!(
        counts["values"].as_u64(),
        Some(28),
        "values; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 28 steps + 5 call_entry + 5 call_exit = 38 events.
    assert_eq!(events.len(), 38, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Call sequence: dynamic execution order --------------------
    // Post bug-fix 1+2: the DFS rooted at `main` recurses through
    // each callee's invocation site as it appears in the parent body,
    // so the recorded call_entry sequence is the dynamic call order.
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "classify".to_string(),
            "loop_sum".to_string(),
            "match_pick".to_string(),
        ]
    );

    // ----- Call-exit order: entry-key flush at top frame ------------
    // Bug-fix 1+2 gives the LIFO closing order for inner siblings:
    // each callee's call_exit fires before the caller's, so the first
    // three sibling leaves of `compute` (classify, loop_sum,
    // match_pick) appear in lexical/entry order.  Re-pinned against
    // trace-format-nim eec665b: call_key is now allocated at
    // registerCall and completed CallRecords are flushed from the
    // buffer in entry-key order, so main and compute (which co-exit
    // at the same step) now appear in entry-key order (main before
    // compute) instead of LIFO.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "classify".to_string(),
            "loop_sum".to_string(),
            "match_pick".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // ----- Call-exit return values (bug-fix 3) -----------------------
    // Each call_exit now carries the function's actual return value
    // (`Int { i }`) instead of the legacy `Void`.  Values are
    // recovered by `tracer.rs::compute_function_return_values` from
    // the VM's `RunResultValue::Success` payload mapped onto the
    // `let X = callee(...)` source pattern, then propagated through
    // the `compute()` tuple-return slots.
    let exit_returns: Vec<(String, i64)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            let i = e["return_value"]["i"].as_i64().unwrap_or_else(|| {
                panic!(
                    "call_exit.return_value must be Int; got {}",
                    e["return_value"]
                )
            });
            (name, i)
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: the call_exit
    // sequence is now flushed in entry-key order at the top frame, so
    // main precedes compute (both co-exit at the same step).
    assert_eq!(
        exit_returns,
        vec![
            ("classify".to_string(), 20),
            ("loop_sum".to_string(), 3),
            ("match_pick".to_string(), 100),
            ("main".to_string(), 123),
            ("compute".to_string(), 123),
        ]
    );

    // ----- Decoded variable values -----------------------------------
    // M10 round-2 numeric-width pin: `let mut i: u32 = 0;` in
    // `loop_sum` now surfaces as a typed-Int step variable (`i = 0`)
    // at its declaration line — pre-fix only `compute()`'s tuple-
    // return slots carried values.  The remaining bindings still
    // surface from the tuple-return-slot mapping.
    assert_eq!(
        observed_var_sequence(&doc),
        vec![
            ("raw".to_string(), 2),
            ("sign".to_string(), 20),
            ("loop_total".to_string(), 3),
            ("i".to_string(), 0),
            ("picked".to_string(), 100),
            ("combined".to_string(), 123),
            ("return_value".to_string(), 123),
        ]
    );
}

/// Regression pin for bug-fix 1+2: the dynamic-call-order DFS must
/// surface `[main, compute, classify, loop_sum, match_pick]` as the
/// first call_entry events of the trace.  Pre-fix the recorder walked
/// the source linearly and reported lexical order.
#[test]
fn test_control_flow_test_call_sequence_dynamic_order() {
    let Some((doc, _)) = record_and_dump_full(
        "test_control_flow_test_call_sequence_dynamic_order",
        "control_flow_test.cairo",
    ) else {
        return;
    };
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "classify".to_string(),
            "loop_sum".to_string(),
            "match_pick".to_string(),
        ]
    );
}

// --- nested_calls_test.cairo -----------------------------------------------

/// Records `nested_calls_test.cairo`.  The program defines a 4-deep
/// chain `outer → middle → inner` plus a `compute` driver, so the
/// recorder is forced to register at least three sibling functions
/// before reaching the entry point.  Asserts on the exact step /
/// call counts and on every let-binding's evaluated value.
#[test]
fn test_nested_calls_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_nested_calls_test_via_ct_print_full",
        "nested_calls_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // Function table follows DFS visit order (bug-fix 1+2): main is
    // discovered first, then compute, then compute's three callees in
    // source-text order (`inner` on line 15 before `middle` on 16
    // before `outer` on 17).  middle / outer also call inner / middle
    // respectively but those callees were already visited at the
    // compute level, so the visited-set short-circuits keep the call
    // table at the spec-pinned 5 entries.
    assert_eq!(
        bare_fns,
        vec!["main", "compute", "inner", "middle", "outer"]
    );

    // ----- counts -----------------------------------------------------
    // 15 steps: inner(2) + middle(2) + outer(2) + compute(6) + main(2)
    // + 1 trailing.  5 calls, 0 io_events.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(15), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(5), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );
    assert_eq!(
        counts["values"].as_u64(),
        Some(15),
        "values; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 15 steps + 5 call_entry + 5 call_exit = 25 events.
    assert_eq!(events.len(), 25, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Call sequence: dynamic execution order --------------------
    // Bug-fix 1+2: DFS from `main` recurses through compute's three
    // call sites in source-text order (`inner`, `middle`, `outer`).
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
        ]
    );

    // ----- Call-exit order: entry-key flush at top frame ------------
    // Bug-fix 1+2: each callee's call_exit fires before its caller's
    // for the inner sub-chain, so inner / middle / outer appear
    // depth-first.  Re-pinned against trace-format-nim eec665b:
    // call_key is now allocated at registerCall and completed
    // CallRecords are flushed from the buffer in entry-key order, so
    // main and compute (which co-exit at the same step) now appear in
    // entry-key order (main before compute) instead of LIFO.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // ----- Call-exit return values (bug-fix 3) -----------------------
    // Each function surfaces its real return value on call_exit.
    // Values flow from compute's tuple-return slots:
    //   inner(1, 2)  = 3  → b → inner=3
    //   middle(1)    = 11 → c → middle=11
    //   outer(1)     = 111 → d → outer=111
    // compute's tail tuple's last named slot is `d=111`; main
    // delegates to compute.
    let exit_returns: Vec<(String, i64)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            let i = e["return_value"]["i"].as_i64().unwrap_or_else(|| {
                panic!(
                    "call_exit.return_value must be Int; got {}",
                    e["return_value"]
                )
            });
            (name, i)
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: the call_exit
    // sequence is now flushed in entry-key order at the top frame, so
    // main precedes compute (both co-exit at the same step).
    assert_eq!(
        exit_returns,
        vec![
            ("inner".to_string(), 3),
            ("middle".to_string(), 11),
            ("outer".to_string(), 111),
            ("main".to_string(), 111),
            ("compute".to_string(), 111),
        ]
    );

    // ----- Decoded values --------------------------------------------
    // The chain executes:
    //   inner(1, 2)  = 3  → b
    //   middle(1)    = inner(1, 10) = 11  → c
    //   outer(1)     = middle(1) + 100 = 111  → d
    // compute returns (a=1, b=3, c=11, d=111); main delegates.
    assert_eq!(
        observed_var_sequence(&doc),
        vec![
            ("a".to_string(), 1),
            ("b".to_string(), 3),
            ("c".to_string(), 11),
            ("d".to_string(), 111),
            ("return_value".to_string(), 111),
        ]
    );
}

/// Regression pin for bug-fix 1+2: nested calls produce LIFO
/// call_exit ordering for the inner sub-chain (inner before middle
/// before outer) so depth-aware consumers can reconstruct the call
/// tree.  Re-pinned against trace-format-nim eec665b: call_key is now
/// allocated at registerCall and completed CallRecords are flushed
/// from the buffer in entry-key order, so the outer pair (main /
/// compute) which co-exits at the final step now appears in
/// entry-key order (main before compute) instead of LIFO.
#[test]
fn test_nested_calls_test_lifo_call_exit_order() {
    let Some((doc, _)) = record_and_dump_full(
        "test_nested_calls_test_lifo_call_exit_order",
        "nested_calls_test.cairo",
    ) else {
        return;
    };
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );
}

/// Regression pin for bug-fix 3: call_exit events surface their
/// real return values (inner=3, middle=11, outer=111, compute=111,
/// main=111) so callers can replay the call tree without
/// re-deriving the values from step-binding heuristics.
#[test]
fn test_nested_calls_test_call_exit_returns_real_values() {
    let Some((doc, _)) = record_and_dump_full(
        "test_nested_calls_test_call_exit_returns_real_values",
        "nested_calls_test.cairo",
    ) else {
        return;
    };
    let returns: Vec<i64> = doc["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| e["return_value"]["i"].as_i64().expect("Int.i"))
        .collect();
    assert_eq!(returns, vec![3, 11, 111, 111, 111]);
}

// --- collections_test.cairo ------------------------------------------------

/// Records `collections_test.cairo`.  Verifies that the recorder
/// surfaces Array literals as `ValueRecord::Sequence` and tuple-literal
/// initialisers as `ValueRecord::Tuple` alongside the existing scalar
/// `ValueRecord::Int` emissions.  See the matching
/// `test_collections_test_value_kinds_present` for the minimal kind-set
/// assertion.
#[test]
fn test_collections_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_collections_test_via_ct_print_full",
        "collections_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // Function table follows DFS visit order (bug-fix 1+2): main →
    // compute → array_total (called first in compute) → pair_sum.
    assert_eq!(bare_fns, vec!["main", "compute", "array_total", "pair_sum"]);

    // ----- counts -----------------------------------------------------
    // 20 steps: array_total(8) + pair_sum(4) + compute(5) + main(2)
    // + 1 trailing.  4 calls, 0 io_events.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(20), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 20 steps + 4 call_entry + 4 call_exit = 28 events.
    assert_eq!(events.len(), 28, "events.len()");
    assert_step_indices_monotonic(&doc);

    // Bug-fix 1+2: dynamic call order is main → compute → array_total
    // → pair_sum.  Bug-fix 1+2 (LIFO exit) and bug-fix 3 (real
    // returns) drive the per-callee return values surfaced below.
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "array_total".to_string(),
            "pair_sum".to_string(),
        ]
    );

    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before compute) instead of LIFO.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "array_total".to_string(),
            "pair_sum".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // Bug-fix 3: array_total returns 4 (Array<felt252>::len), pair_sum
    // returns 30 (10 + 20), compute's tail tuple's last named slot is
    // `final_sum=34`, main delegates.
    let exit_returns: Vec<(String, i64)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            let i = e["return_value"]["i"].as_i64().unwrap_or_else(|| {
                panic!(
                    "call_exit.return_value must be Int; got {}",
                    e["return_value"]
                )
            });
            (name, i)
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: the call_exit
    // sequence is now flushed in entry-key order at the top frame, so
    // main precedes compute (both co-exit at the same step).
    assert_eq!(
        exit_returns,
        vec![
            ("array_total".to_string(), 4),
            ("pair_sum".to_string(), 30),
            ("main".to_string(), 34),
            ("compute".to_string(), 34),
        ]
    );

    // ----- Decoded values --------------------------------------------
    // The recorder emits a `ValueRecord::Sequence` for the literal-only
    // `arr` Array<felt252> binding (line 6, after the last `arr.append`)
    // and a `ValueRecord::Tuple` for the literal-only `pair` tuple
    // initialiser (line 12).  The remaining scalar bindings (the
    // tuple-return slots of `compute`) decode as `ValueRecord::Int`.
    //
    //   array_total() returns 4 (the length of [1,2,3,4])
    //   pair_sum()    returns 30 (10 + 20)
    //   final_sum     = 4 + 30 = 34
    //
    // Bug-fix 1+2 changed the surfacing order: each `let X = callee()`
    // line in compute first emits the scalar `X = callee_result` and
    // then recurses into the callee, where the compound binding fires
    // on its own `emit_line`.  So `arr_total` precedes `arr` (callee
    // contents) and `pair_total` precedes `pair`.
    // M10 round 2: `pair_sum` contains `let (x, y) = pair;` which now
    // expands into two scalar Int emissions (`x = 10`, `y = 20`) at the
    // destructuring line.  The destructured children fire after the
    // source `pair` Tuple binding (same line, ordering matches the
    // recorder's per-line emit chain: scalar → compound → destructure).
    let observed = observed_var_kinds(&doc);
    assert_eq!(
        observed,
        vec![
            ("arr_total".to_string(), "Int".to_string()),
            ("arr".to_string(), "Sequence".to_string()),
            ("pair_total".to_string(), "Int".to_string()),
            ("pair".to_string(), "Tuple".to_string()),
            ("x".to_string(), "Int".to_string()),
            ("y".to_string(), "Int".to_string()),
            ("final_sum".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // Strict-shape assertions for the compound values.  Element ordering
    // must match the source-level append/literal order; bare `i64`
    // values must round-trip through the felt252 Int encoding.
    let arr_value = find_var_value(&doc, "arr").expect("arr step variable");
    assert_eq!(arr_value["kind"].as_str(), Some("Sequence"));
    assert_eq!(arr_value["is_slice"].as_bool(), Some(false));
    let arr_elements: Vec<i64> = arr_value["elements"]
        .as_array()
        .expect("arr.elements")
        .iter()
        .map(|e| {
            assert_eq!(e["kind"].as_str(), Some("Int"), "arr element should be Int");
            e["i"].as_i64().expect("arr element i")
        })
        .collect();
    assert_eq!(arr_elements, vec![1, 2, 3, 4]);

    let pair_value = find_var_value(&doc, "pair").expect("pair step variable");
    assert_eq!(pair_value["kind"].as_str(), Some("Tuple"));
    let pair_elements: Vec<i64> = pair_value["elements"]
        .as_array()
        .expect("pair.elements")
        .iter()
        .map(|e| {
            assert_eq!(
                e["kind"].as_str(),
                Some("Int"),
                "pair element should be Int"
            );
            e["i"].as_i64().expect("pair element i")
        })
        .collect();
    assert_eq!(pair_elements, vec![10, 20]);

    // Scalar (Int) emissions remain unchanged for the original bindings
    // and now include the destructured children `x` / `y`.
    // `observed_var_sequence` still hard-rejects non-Int variants, so we
    // filter the compound names out before comparing.
    let scalar_only = observed_var_sequence_filtered(&doc, &["arr", "pair"]);
    assert_eq!(
        scalar_only,
        vec![
            ("arr_total".to_string(), 4),
            ("pair_total".to_string(), 30),
            ("x".to_string(), 10),
            ("y".to_string(), 20),
            ("final_sum".to_string(), 34),
            ("return_value".to_string(), 34),
        ]
    );
}

#[test]
fn test_collections_test_value_kinds_present() {
    let Some((doc, _)) = record_and_dump_full(
        "test_collections_test_value_kinds_present",
        "collections_test.cairo",
    ) else {
        return;
    };
    let mut kinds = std::collections::BTreeSet::new();
    for ev in doc["events"].as_array().unwrap() {
        if ev["kind"] != "step" {
            continue;
        }
        for v in ev["vars"].as_array().cloned().unwrap_or_default() {
            if let Some(k) = v["value"]["kind"].as_str() {
                kinds.insert(k.to_string());
            }
        }
    }
    for want in ["Int", "Sequence", "Tuple"] {
        assert!(
            kinds.contains(want),
            "expected {want} ValueRecord variant in collections trace; got {kinds:?}"
        );
    }
}

// --- error_paths_test.cairo ------------------------------------------------

/// Records `error_paths_test.cairo`.  The program calls
/// `divide(10, 0)` which trips an `assert!` and triggers a Cairo
/// panic.  The recorder catches the `RunResultValue::Panic` outcome
/// and surfaces it via `register_special_event(EventLogKind::Error,
/// "CairoPanic", ...)`, so we expect exactly one `io_event` of kind
/// `ioError` containing the panic payload.
///
/// Bug-fix 4 lock-in: source-level let-bindings whose RHS is a
/// statically-evaluable literal (`let a: felt252 = 10;`) keep their
/// real source values across a panic.  Pre-fix `a` decoded as the
/// first felt of the panic encoding (0 after i64 overflow); now it
/// surfaces as 10 because `parse_let_binding_literals` seeds the
/// var-values map before the VM is ever consulted.  `b`'s RHS is the
/// panicking call so no value is recoverable for it — it's omitted
/// from the trace's step variables instead of being wrong.
#[test]
fn test_error_paths_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_error_paths_test_via_ct_print_full",
        "error_paths_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // Function table follows DFS visit order (bug-fix 1+2): main →
    // compute → divide.
    assert_eq!(bare_fns, vec!["main", "compute", "divide"]);

    // ----- counts -----------------------------------------------------
    // 10 steps + 3 calls + 1 io_event (the panic).
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(10), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(1),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 10 steps + 3 call_entry + 3 call_exit + 1 io = 17 events.
    assert_eq!(events.len(), 17, "events.len()");
    assert_step_indices_monotonic(&doc);

    // Bug-fix 1+2: dynamic call order is main → compute → divide.
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "divide".to_string(),
        ]
    );

    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  divide closes first
    // (mid-panic), then main and compute co-exit at the panic
    // teardown step and appear in entry-key order (main before
    // compute) instead of LIFO.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "divide".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // Every call_exit on a panicked frame surfaces `Void` because the
    // VM never returned.  `compute_function_return_values` returns
    // `None` for every function on a panic run (no `Success` payload
    // to read), and `emit_function_dfs` maps `None` to `NONE_VALUE`.
    for ev in events {
        if ev["kind"] != "call_exit" {
            continue;
        }
        let rv = &ev["return_value"];
        assert_eq!(
            rv["kind"].as_str(),
            Some("Void"),
            "call_exit on a panicked program should be Void; got {rv}"
        );
    }

    // ----- The panic event -------------------------------------------
    let io_events: Vec<&serde_json::Value> = events.iter().filter(|e| e["kind"] == "io").collect();
    assert_eq!(io_events.len(), 1, "expected exactly one io event");
    let panic_ev = io_events[0];
    assert_eq!(
        panic_ev["io_kind"].as_str(),
        Some("ioError"),
        "panic event must be tagged ioError; got {panic_ev}"
    );
    let text = panic_ev["text"].as_str().expect("io.text str");
    assert!(
        text.contains("Cairo program panicked"),
        "panic text must mention the panic; got {text}"
    );
    assert!(
        text.contains("4 value(s)"),
        "panic text should mention the 4 panic-payload values; got {text}"
    );

    // ----- Decoded variable values -----------------------------------
    // Post bug-fix 4: `a` keeps its source-evaluated value (10), `b`'s
    // RHS panicked so it's omitted from the trace, and the synthetic
    // trailing `return_value` is omitted because the panic prevented
    // `main` from producing a value.
    assert_eq!(observed_var_sequence(&doc), vec![("a".to_string(), 10),]);
}

/// Regression pin for bug-fix 4: when a Cairo program panics, source-
/// level let-bindings whose RHS was already evaluated keep the real
/// source value (`a = 10`) instead of being clobbered by the
/// panic-payload vector.
#[test]
fn test_error_paths_test_let_bindings_keep_source_values() {
    let Some((doc, _)) = record_and_dump_full(
        "test_error_paths_test_let_bindings_keep_source_values",
        "error_paths_test.cairo",
    ) else {
        return;
    };
    let observed = observed_var_sequence(&doc);
    assert!(
        observed.iter().any(|(n, v)| n == "a" && *v == 10),
        "expected `a` = 10 in trace (the value bound before the panic); \
         observed = {observed:?}"
    );
}

// ===========================================================================
// M10 fixtures — ValueRecord variants that close the M9 known-limitation
// list (Struct / Variant / Storage / felt-decoded-panic / loop-step).
// ===========================================================================
//
// Each fixture pins one of the top-five idiomatic-Cairo shapes the M9
// recorder did not yet emit:
//
// 1. `struct_test.cairo`        — first user-defined-Struct emission.
// 2. `result_option_test.cairo` — first `ValueRecord::Variant` emission
//    for `Option::Some`/`None` and `Result::Ok`/`Err`.
// 3. `storage_test`             — StarkNet contract pin via the
//    `write_starknet_trace` snforge-converter path; the Cairo source
//    sits in `cairo/storage_test.cairo` for documentation, but the
//    canonical CTFS bundle is produced from the matching
//    `starknet/storage_test_trace.json` snforge fixture.
// 4. `panic_with_felt252_test`  — felt-decoded `assert!` message in the
//    `register_special_event(EventLogKind::Error, "CairoPanic", ...)`
//    payload (closes the M9 deferred
//    `test_error_paths_test_let_bindings_keep_source_values`).
// 5. `loop_while_for_test`      — locks today's per-source-line stepping
//    for `while` loops (one step per body line, regardless of iteration
//    count) and pairs with an `#[ignore]`'d sibling pin asserting the
//    spec-correct per-iteration shape.

// --- struct_test.cairo -----------------------------------------------------

/// Records `struct_test.cairo` and asserts on the **exact** event shape.
///
/// The program defines a `Point { x: felt252, y: felt252 }` struct and
/// initialises it with two literal Point values inside `compute()`.  The
/// recorder must:
///
/// * register a dedicated `Struct`-kinded type id keyed by the bare
///   struct name (`"Point"`).
/// * emit a `ValueRecord::Struct { field_values, type_id }` step
///   variable for each `let <name>: Point = Point { x: lit, y: lit };`,
///   with `field_values` holding two `Int` ValueRecords in source-
///   declared field order.
/// * keep the existing scalar (`Int`) emission for `total` and the
///   trailing synthetic `return_value`.
#[test]
fn test_struct_test_via_ct_print_full() {
    let Some((doc, source_path)) =
        record_and_dump_full("test_struct_test_via_ct_print_full", "struct_test.cairo")
    else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // Function table: DFS visit order from main.
    assert_eq!(bare_fns, vec!["main", "compute"]);

    // ----- Type table — the per-struct lang_type must appear ----------
    // The recorder lazily registers a `Struct`-kinded type with a
    // `lang_type` of the form `"Point{x,y}"` the first time a struct
    // literal is emitted: the bare struct name is followed by the
    // source-declared field-name shape so downstream consumers can zip
    // the positional `field_values` against the named fields without
    // re-parsing the Cairo source.  ct-print --full surfaces the
    // registered lang_type in the trace's `types` array.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        types.contains(&"Point{x,y}"),
        "expected `Point{{x,y}}` lang_type in types table; got {:?}",
        types
    );

    // ----- counts -----------------------------------------------------
    // 8 steps: main(2) + compute(5) + 1 trailing return_value step.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(8), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 8 steps + 2 call_entry + 2 call_exit = 12 events.
    assert_eq!(events.len(), 12, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Call sequence + entry-key flush exit ----------------------
    assert_eq!(
        observed_call_sequence(&doc),
        vec!["main".to_string(), "compute".to_string()]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before compute) rather than the previous
    // inverse-LIFO order.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec!["main".to_string(), "compute".to_string()]
    );

    // ----- Decoded value kinds ---------------------------------------
    // Sequence: origin (Struct), shift (Struct), total (Int), then the
    // synthetic trailing `return_value` (Int) attached to main's last
    // body line.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("origin".to_string(), "Struct".to_string()),
            ("shift".to_string(), "Struct".to_string()),
            ("total".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // ----- Strict struct-shape assertions ----------------------------
    // Each `Point` struct literal surfaces as a `ValueRecord::Struct`
    // whose `field_values` array carries the two felt literals in
    // source-declared field order (`x`, then `y`).
    let origin_value = find_var_value(&doc, "origin").expect("origin step variable");
    assert_eq!(origin_value["kind"].as_str(), Some("Struct"));
    let origin_fields: Vec<i64> = origin_value["field_values"]
        .as_array()
        .expect("origin.field_values")
        .iter()
        .map(|e| {
            assert_eq!(
                e["kind"].as_str(),
                Some("Int"),
                "origin field should be Int"
            );
            e["i"].as_i64().expect("origin field i")
        })
        .collect();
    assert_eq!(origin_fields, vec![3, 4]);

    let shift_value = find_var_value(&doc, "shift").expect("shift step variable");
    assert_eq!(shift_value["kind"].as_str(), Some("Struct"));
    let shift_fields: Vec<i64> = shift_value["field_values"]
        .as_array()
        .expect("shift.field_values")
        .iter()
        .map(|e| e["i"].as_i64().expect("shift field i"))
        .collect();
    assert_eq!(shift_fields, vec![10, 20]);

    // ----- Scalar (Int) emissions -------------------------------------
    let scalar_only = observed_var_sequence_filtered(&doc, &["origin", "shift"]);
    assert_eq!(
        scalar_only,
        vec![("total".to_string(), 37), ("return_value".to_string(), 37),]
    );

    // ----- Per-callee return values (bug-fix 3 carryover) -------------
    // compute() returns total = 3+4+10+20 = 37.  main() delegates so
    // surfaces 37 as well.
    let exit_returns: Vec<(String, i64)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            let i = e["return_value"]["i"].as_i64().unwrap_or_else(|| {
                panic!(
                    "call_exit.return_value must be Int; got {}",
                    e["return_value"]
                )
            });
            (name, i)
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: the call_exit
    // sequence is now flushed in entry-key order at the top frame, so
    // main precedes compute (both co-exit at the same step).
    assert_eq!(
        exit_returns,
        vec![("main".to_string(), 37), ("compute".to_string(), 37),]
    );
}

// --- result_option_test.cairo ---------------------------------------------

/// Records `result_option_test.cairo` and asserts on the **exact**
/// event shape.  The fixture lives entirely inside `compute()`:
///
/// ```cairo
/// let opt_some: Option<felt252> = Option::Some(7);
/// let opt_none: Option<felt252> = Option::None;
/// let res_ok:  Result<felt252, felt252> = Result::Ok(11);
/// let res_err: Result<felt252, felt252> = Result::Err(5);
/// ```
///
/// The recorder must surface each binding as a `ValueRecord::Variant`
/// whose `discriminator` field carries the bare variant name
/// (`"Some"` / `"None"` / `"Ok"` / `"Err"`) and whose `contents`
/// box carries the payload felt (or a `None` ValueRecord for
/// `Option::None`).
#[test]
fn test_result_option_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_result_option_test_via_ct_print_full",
        "result_option_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(bare_fns, vec!["main", "compute"]);

    // ----- counts -----------------------------------------------------
    // main(2) + compute(8) + 1 trailing = 11 step events; 2 calls.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(11), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 11 steps + 2 call_entry + 2 call_exit = 15 events.
    assert_eq!(events.len(), 15, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec!["main".to_string(), "compute".to_string()]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before compute) rather than the previous
    // inverse-LIFO order.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec!["main".to_string(), "compute".to_string()]
    );

    // ----- Decoded value kinds ---------------------------------------
    // Each Option/Result let-binding fires as a `Variant`; `total` and
    // the synthetic trailing `return_value` are still `Int`.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("opt_some".to_string(), "Variant".to_string()),
            ("opt_none".to_string(), "Variant".to_string()),
            ("res_ok".to_string(), "Variant".to_string()),
            ("res_err".to_string(), "Variant".to_string()),
            ("total".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // ----- Strict variant-shape assertions ---------------------------
    let opt_some = find_var_value(&doc, "opt_some").expect("opt_some var");
    assert_eq!(opt_some["kind"].as_str(), Some("Variant"));
    assert_eq!(opt_some["discriminator"].as_str(), Some("Some"));
    assert_eq!(opt_some["contents"]["kind"].as_str(), Some("Int"));
    assert_eq!(opt_some["contents"]["i"].as_i64(), Some(7));

    let opt_none = find_var_value(&doc, "opt_none").expect("opt_none var");
    assert_eq!(opt_none["kind"].as_str(), Some("Variant"));
    assert_eq!(opt_none["discriminator"].as_str(), Some("None"));
    // The recorder emits `NONE_VALUE` for empty-payload variants, which
    // ct-print --full surfaces with `kind = "None"`.
    assert_eq!(opt_none["contents"]["kind"].as_str(), Some("None"));

    let res_ok = find_var_value(&doc, "res_ok").expect("res_ok var");
    assert_eq!(res_ok["kind"].as_str(), Some("Variant"));
    assert_eq!(res_ok["discriminator"].as_str(), Some("Ok"));
    assert_eq!(res_ok["contents"]["i"].as_i64(), Some(11));

    let res_err = find_var_value(&doc, "res_err").expect("res_err var");
    assert_eq!(res_err["kind"].as_str(), Some("Variant"));
    assert_eq!(res_err["discriminator"].as_str(), Some("Err"));
    assert_eq!(res_err["contents"]["i"].as_i64(), Some(5));
}

// --- panic_with_felt252_test.cairo ----------------------------------------

/// Records `panic_with_felt252_test.cairo`.  The program calls
/// `require_positive(0)` which trips an `assert!(_, "value was zero")`
/// macro and panics.  Pre-M10 the recorder surfaced the panic only as
/// the raw decimal felt vector — the human-readable `"value was zero"`
/// message was lost.  Post-M10 the recorder additionally felt-decodes
/// every panic-payload value and appends the recovered ASCII run to
/// the `register_special_event` `text` field.
///
/// This test pins:
///
/// * the function table (DFS order from main).
/// * the io_event count + tag (one `ioError`).
/// * the literal substring `value was zero` inside the panic event's
///   `text` (the felt-decoded message).
#[test]
fn test_panic_with_felt252_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_panic_with_felt252_test_via_ct_print_full",
        "panic_with_felt252_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(bare_fns, vec!["main", "compute", "require_positive"]);

    // ----- counts -----------------------------------------------------
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(10), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(1),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "require_positive".to_string(),
        ]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the panic step, so they appear in entry-key order (main before
    // compute) instead of the previous inverse-LIFO order.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "require_positive".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // ----- The panic event surfaces both the raw felt list and the
    //       felt-decoded message ------------------------------------------
    let io_events: Vec<&serde_json::Value> = events.iter().filter(|e| e["kind"] == "io").collect();
    assert_eq!(io_events.len(), 1, "expected exactly one io event");
    let panic_ev = io_events[0];
    assert_eq!(
        panic_ev["io_kind"].as_str(),
        Some("ioError"),
        "panic event must be tagged ioError; got {panic_ev}"
    );
    let text = panic_ev["text"].as_str().expect("io.text str");
    assert!(
        text.contains("Cairo program panicked"),
        "panic text must mention the panic; got {text}"
    );
    // M10 fix: the assert! message is felt-decoded back to ASCII and
    // appended to the event text.  Pre-fix the message lived only in
    // the raw felt encoding and was effectively unreadable in the
    // event-log pane.
    assert!(
        text.contains("value was zero"),
        "panic text should include the felt-decoded `assert!` message \
         `value was zero`; got {text}"
    );
    assert!(
        text.contains("message="),
        "panic text should tag the decoded message with `message=`; got {text}"
    );

    // ----- The let-binding `a` keeps its source-evaluated value
    //       (closes the M9 deferred test_error_paths_..._keep_source_values
    //       pin: `a` here is `let a: felt252 = 7;`, so the recorder must
    //       surface `a = 7` even though the panic in `require_positive`
    //       prevents any VM-side `Success` payload). -----------------------
    let observed = observed_var_sequence(&doc);
    assert_eq!(
        observed,
        vec![("a".to_string(), 7),],
        "expected only `a = 7` to surface; observed = {observed:?}"
    );
}

// --- loop_while_for_test.cairo --------------------------------------------

/// Records `loop_while_for_test.cairo` and pins the **post-M11** per-
/// iteration stepping behaviour for `while` loops: the recorder
/// re-emits the loop's body lines once per executed iteration (and
/// re-emits the header line on each condition check + once at the
/// final exit).  Each recognised body assignment also surfaces a step
/// variable carrying the post-update value, so `acc` / `i` walk the
/// same value sequence the VM would have produced (1,2,3 then 10,20
/// for `acc`; 1,2,3 then 1,2 for `i`).
///
/// See the sibling `test_loop_while_for_test_per_iteration_steps_pin`
/// — both tests now exercise the same code path; the pin keeps the
/// `acc_count >= 5` floor so a regression that drops back to one-
/// step-per-source-line (the pre-M11 shape) fails loudly there too.
#[test]
fn test_loop_while_for_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_loop_while_for_test_via_ct_print_full",
        "loop_while_for_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(
        bare_fns,
        vec!["main", "compute", "loop_three", "loop_double"]
    );

    // ----- counts -----------------------------------------------------
    // Post-M11 the recorder unrolls each `while` loop one iteration
    // at a time.  Step accounting:
    //
    //   loop_three (3 iters): fn header(1) + 2 lets(2) + 3 iters
    //                          × (header + 2 body) + 1 trailing
    //                          header for the false condition + 1
    //                          tail line (`acc`) = 14
    //                          (the closing `};` is consumed by the
    //                          simulator and does not surface)
    //   loop_double (2 iters): same shape with 2 iters → 11
    //   compute:               5 lines, all let-bindings + tail
    //   main:                  2 lines (fn header + `compute()`)
    //   start():               implicit start step at line 1
    //
    // Total: 14 + 11 + 5 + 2 + 1 = 33.
    let counts = &doc["counts"];
    assert_eq!(counts["steps"].as_u64(), Some(33), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 33 steps + 4 call_entry + 4 call_exit = 41 events.
    assert_eq!(events.len(), 41, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Call sequence + entry-key flush exit ----------------------
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "loop_three".to_string(),
            "loop_double".to_string(),
        ]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before compute) instead of LIFO.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "loop_three".to_string(),
            "loop_double".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // ----- Per-iteration variable emissions --------------------------
    // The simulator re-emits each body assignment's post-update value
    // as a step variable, in iteration order.  loop_three (3 iters)
    // updates acc to 1,2,3 with i to 1,2,3 in lockstep; loop_double
    // (2 iters) updates acc to 10,20 with i to 1,2.  This is the same
    // floor the sibling per_iteration_steps_pin asserts and the only
    // pin in the recorder that ties trace shape to dynamic execution
    // count, so a regression to source-line-only emission fails here
    // loudly.
    let var_seq: Vec<(String, i64)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| {
            e["vars"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
        })
        .map(|v| {
            let name = v["varname"].as_str().expect("varname str").to_string();
            let i = v["value"]["i"]
                .as_i64()
                .expect("loop simulator should emit Int values");
            (name, i)
        })
        .collect();
    // M10 round-2 numeric-width pin: each `let mut i: u32 = 0;` line
    // (one per loop function) now also emits `i = 0` at the
    // declaration line, ahead of the loop simulator's per-iteration
    // emissions.  The simulator's emission sequence is otherwise
    // unchanged (1,2,3 for `i` in loop_three; 1,2 in loop_double).
    assert_eq!(
        var_seq,
        vec![
            ("i".to_string(), 0),
            ("acc".to_string(), 1),
            ("i".to_string(), 1),
            ("acc".to_string(), 2),
            ("i".to_string(), 2),
            ("acc".to_string(), 3),
            ("i".to_string(), 3),
            ("i".to_string(), 0),
            ("acc".to_string(), 10),
            ("i".to_string(), 1),
            ("acc".to_string(), 20),
            ("i".to_string(), 2),
        ]
    );
}

/// Spec floor for the loop-step expectation: the recorder must emit
/// at least one `acc` step variable per executed iteration of each
/// `while` loop in the fixture (3 iters in `loop_three` + 2 iters in
/// `loop_double` = 5 minimum).  Pre-M11 the static-DFS walker emitted
/// one step per source line regardless of execution count and this
/// test was `#[ignore]`'d; M11 added a tiny source-AST simulator
/// (`tracer.rs::simulate_while_loop`) that re-emits the body of every
/// recognised `while` loop one iteration at a time, which is what
/// drives the post-update `acc` variable emissions this test counts.
#[test]
fn test_loop_while_for_test_per_iteration_steps_pin() {
    let Some((doc, _)) = record_and_dump_full(
        "test_loop_while_for_test_per_iteration_steps_pin",
        "loop_while_for_test.cairo",
    ) else {
        return;
    };

    // loop_three iterates 3 times, loop_double 2 times.  Each body
    // contributes 2 mutating lines (`acc = acc + N`, `i = i + 1`) so
    // a per-iteration emission would surface 3*2 + 2*2 = 10 mutation
    // steps inside the loops alone.  The exact number depends on how
    // the iteration boundary is modelled — this is a spec pin, not a
    // recorder mirror.
    let observed = observed_var_kinds(&doc);
    let acc_count = observed.iter().filter(|(n, _)| n == "acc").count();
    assert!(
        acc_count >= 5,
        "expected `acc` to surface at least once per loop iteration \
         (3 + 2 = 5 minimum); observed acc emissions = {acc_count}"
    );
}

// --- storage_test (snforge-converter path) --------------------------------

/// Records the storage_test snforge JSON fixture through
/// `starknet::write_starknet_trace` and asserts that the produced
/// `.ct` container surfaces the expected `storage_read` / `storage_write`
/// call frames in dynamic order.  This pins the M4 SnforgeTrace parsing
/// → CTFS conversion path as a universal-test-level invariant: any
/// future regression that drops, reorders, or relabels the storage
/// op events will fail this test.
///
/// The fixture's matching Cairo source lives at
/// `test-programs/cairo/storage_test.cairo` for documentation but is
/// not compiled by the recorder (it has no `fn main` and the
/// `#[starknet::contract]` dispatcher requires a separate runtime that
/// the in-process Sierra runner does not provide).
#[test]
fn test_storage_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_storage_test_via_ct_print_full") else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("storage_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse storage_test snforge trace");

    // Sanity-check the parsed entry shape so a regression in the JSON
    // schema parser fails here loudly rather than via the downstream
    // CTFS comparison.
    assert_eq!(entries.len(), 5, "expected 5 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // ----- Function table — one entry per registered call frame -------
    // The snforge converter emits `<contract>::<selector>` /
    // `<contract>::storage_read` / `<contract>::storage_write` named
    // call frames in dynamic order.  Function ids are assigned at
    // first-emission time so the table reflects the same dynamic order
    // as the call_entry sequence below.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // The exact dedup behaviour of `ensure_function_id` keeps each
    // distinct name once.  For this fixture the deduped set is
    // [increment, storage_read, storage_write, get].
    let bare_fns: Vec<String> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap().to_string())
        .collect();
    assert_eq!(
        bare_fns,
        vec![
            "increment".to_string(),
            "storage_read".to_string(),
            "storage_write".to_string(),
            "get".to_string(),
        ]
    );

    // ----- Call sequence — dynamic execution order --------------------
    // `increment` performs read+write, then `get` performs a final
    // read.  Each contract_call / storage_read / storage_write entry
    // emits exactly one call frame.
    let call_sequence: Vec<String> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            e["function"]
                .as_str()
                .expect("call_entry.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string()
        })
        .collect();
    assert_eq!(
        call_sequence,
        vec![
            "increment".to_string(),
            "storage_read".to_string(),
            "storage_write".to_string(),
            "get".to_string(),
            "storage_read".to_string(),
        ]
    );

    // ----- counts -----------------------------------------------------
    // 5 entries → 5 call frames + per-entry register_step (5) + the
    // implicit `start()` step the writer emits at line 1 = 6 step
    // events.  Storage read/write entries also emit a canonical
    // `register_special_event(EventLogKind::Read|Write, ...)` so the
    // io_events count is `2 (reads) + 1 (write) = 3`.  This matches the
    // M10 spec target — every `#[storage]` op surfaces as both a call
    // frame and an io_event.
    let counts = &doc["counts"];
    assert_eq!(counts["calls"].as_u64(), Some(5), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(6), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(3),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 6 steps + 5 call_entry + 5 call_exit + 3 io_events = 19 events.
    assert_eq!(events.len(), 19, "events.len()");

    // ----- io_event sequence -----------------------------------------
    // The fixture executes:
    //   increment(7) → storage_read("value")=0, storage_write("value", 0→7)
    //   get()        → storage_read("value")=7
    // so the io_event sequence is exactly Read, Write, Read with the
    // matching `<contract>:<key>=<value(s)>` text baked in.
    let io_events: Vec<&serde_json::Value> = events.iter().filter(|e| e["kind"] == "io").collect();
    assert_eq!(
        io_events.len(),
        3,
        "expected 3 io events (read, write, read); got {io_events:?}"
    );
    let io_pairs: Vec<(String, String)> = io_events
        .iter()
        .map(|e| {
            let io_kind = e["io_kind"].as_str().unwrap_or("").to_string();
            let text = e["text"].as_str().unwrap_or("").to_string();
            (io_kind, text)
        })
        .collect();
    // The multi-stream Nim writer collapses the wider EventLogKind enum
    // down to a 3-way IOEventKind: `EventLogKind::Read` → `ioFileOp` and
    // `EventLogKind::Write` → `ioStdout` (see `toIOEventKind` in
    // codetracer-trace-format-nim/src/codetracer_trace_writer_ffi.nim).
    // The discriminator that downstream consumers actually rely on lives
    // in the `text` field, where our recorder embeds the
    // `<contract>:<key>=<value(s)>` payload.  Pin both the kind tag (so a
    // regression to a non-io variant fails loudly) and the embedded
    // payload (so a regression in the recorder's content format fails
    // here rather than at the consumer).
    assert!(
        io_pairs[0].0 == "ioFileOp" && io_pairs[0].1.contains("0xcafe:value=0"),
        "first io_event should be a Read (ioFileOp) of value=0; got {:?}",
        io_pairs[0]
    );
    assert!(
        io_pairs[1].0 == "ioStdout" && io_pairs[1].1.contains("0xcafe:value=0->7"),
        "second io_event should be a Write (ioStdout) of value 0->7; got {:?}",
        io_pairs[1]
    );
    assert!(
        io_pairs[2].0 == "ioFileOp" && io_pairs[2].1.contains("0xcafe:value=7"),
        "third io_event should be a Read (ioFileOp) of value=7; got {:?}",
        io_pairs[2]
    );
}

// ===========================================================================
// M10 round 2 — five additional fixtures pinning the next ValueRecord
// shapes (destructuring, snapshot/ref, numeric widths, array operations,
// StarkNet events).  Each fixture follows the same strict-`_via_ct_print_full`
// shape established by the round-1 tests above: assertions are made on the
// decoded JSON document with EXACT counts, EXACT decoded values, and an
// upfront `assert_eq!` against the per-step variable kind / value sequence.
// ===========================================================================

// --- destructuring_test.cairo ---------------------------------------------

/// Records `destructuring_test.cairo`.  The fixture exercises the
/// `let (x, y) = pair;` destructuring shape: pre-fix only the source
/// `pair` binding surfaced; post-fix `parse_destructure_bindings`
/// expands the destructured names into their corresponding tuple-element
/// values and emits each as a scalar `ValueRecord::Int` step variable
/// at the destructuring line.
#[test]
fn test_destructuring_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_destructuring_test_via_ct_print_full",
        "destructuring_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main.
    assert_eq!(bare_fns, vec!["main", "use_pair"]);

    let counts = &doc["counts"];
    // 8 step events: implicit start(1) + main(2) + use_pair(4) +
    // trailing return_value step(1).
    assert_eq!(counts["steps"].as_u64(), Some(8), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 8 steps + 2 call_entry + 2 call_exit = 12 events.
    assert_eq!(events.len(), 12, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec!["main".to_string(), "use_pair".to_string()]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and use_pair co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before use_pair) rather than the previous
    // inverse-LIFO order.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec!["main".to_string(), "use_pair".to_string()]
    );

    // Per-binding kind sequence — `pair` is a Tuple, the destructured
    // children `x` / `y` are scalar Ints, `total` and `return_value`
    // remain Int.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("pair".to_string(), "Tuple".to_string()),
            ("x".to_string(), "Int".to_string()),
            ("y".to_string(), "Int".to_string()),
            ("total".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // Strict-shape assertion for the source `pair` Tuple.
    let pair_value = find_var_value(&doc, "pair").expect("pair step variable");
    assert_eq!(pair_value["kind"].as_str(), Some("Tuple"));
    let pair_elements: Vec<i64> = pair_value["elements"]
        .as_array()
        .expect("pair.elements")
        .iter()
        .map(|e| {
            assert_eq!(e["kind"].as_str(), Some("Int"));
            e["i"].as_i64().expect("pair element i")
        })
        .collect();
    assert_eq!(pair_elements, vec![10, 20]);

    // Strict scalar values for the destructured children + tail bindings.
    let scalar_only = observed_var_sequence_filtered(&doc, &["pair"]);
    assert_eq!(
        scalar_only,
        vec![
            ("x".to_string(), 10),
            ("y".to_string(), 20),
            ("total".to_string(), 30),
            ("return_value".to_string(), 30),
        ]
    );

    // Per-callee return values: use_pair returns x + y = 30; main
    // delegates.
    let exit_returns: Vec<(String, i64)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            let i = e["return_value"]["i"].as_i64().unwrap_or_else(|| {
                panic!(
                    "call_exit.return_value must be Int; got {}",
                    e["return_value"]
                )
            });
            (name, i)
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: the call_exit
    // sequence is now flushed in entry-key order at the top frame, so
    // main precedes use_pair (both co-exit at the same step).
    assert_eq!(
        exit_returns,
        vec![("main".to_string(), 30), ("use_pair".to_string(), 30),]
    );
}

// --- snapshot_ref_test.cairo ----------------------------------------------

/// Records `snapshot_ref_test.cairo`.  Pins the M10 round-2 snapshot/ref
/// parameter pin: `read_only(p: @Point)` surfaces a
/// `ValueRecord::Reference { mutable: false, ... }` for `p` at the
/// callee's entry step and `scale(ref p: Point, k: felt252)` surfaces a
/// `ValueRecord::Reference { mutable: true, ... }` for `p` — the
/// dereferenced `Point` struct walked under both.  Pre-fix the recorder
/// did not track parameters at all; round-2 adds a static pass over
/// `@<name>` / `ref <name>` call sites paired with the matching parameter
/// declaration to recover the expected references.
#[test]
fn test_snapshot_ref_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_snapshot_ref_test_via_ct_print_full",
        "snapshot_ref_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order: main → compute → read_only (called first in
    // compute) → scale.
    assert_eq!(bare_fns, vec!["main", "compute", "read_only", "scale"]);

    // Type table: felt252 (scalar), Point{x,y} (struct), Ref
    // (snapshot/ref carrier), type_0 (writer-side default felt id used
    // for the synthetic return_value step variable).
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "Point{x,y}", "Ref", "type_0"]);

    let counts = &doc["counts"];
    // 16 step events: implicit start(1) + main(2) + compute(7) +
    // read_only(2) + scale(3) + trailing return_value(1).
    assert_eq!(counts["steps"].as_u64(), Some(16), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 16 steps + 4 call_entry + 4 call_exit = 24 events.
    assert_eq!(events.len(), 24, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "compute".to_string(),
            "read_only".to_string(),
            "scale".to_string(),
        ]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before compute) instead of LIFO.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "read_only".to_string(),
            "scale".to_string(),
            "main".to_string(),
            "compute".to_string(),
        ]
    );

    // Per-binding kind sequence — origin/shift Struct, p (snapshot
    // entry of read_only) Reference, p (mutable entry of scale)
    // Reference, total Int, return_value Int.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("origin".to_string(), "Struct".to_string()),
            ("p".to_string(), "Reference".to_string()),
            ("shift".to_string(), "Struct".to_string()),
            ("p".to_string(), "Reference".to_string()),
            ("total".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // Strict shape for the snapshot reference (read_only's `p`).  The
    // dereferenced Struct carries `origin`'s field values (3, 4); the
    // `mutable` flag is false; the synthesised address is deterministic
    // (first-emitted reference gets the base address 0x1000 = 4096).
    let p_refs: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| e["vars"].as_array().map(|a| a.iter()).into_iter().flatten())
        .filter(|v| {
            v["varname"].as_str() == Some("p") && v["value"]["kind"].as_str() == Some("Reference")
        })
        .map(|v| &v["value"])
        .collect();
    assert_eq!(p_refs.len(), 2, "expected exactly two `p` Reference vars");

    let snap_ref = p_refs[0];
    assert_eq!(snap_ref["mutable"].as_bool(), Some(false));
    assert_eq!(snap_ref["address"].as_u64(), Some(0x1000));
    assert_eq!(snap_ref["dereferenced"]["kind"].as_str(), Some("Struct"));
    let snap_fields: Vec<i64> = snap_ref["dereferenced"]["field_values"]
        .as_array()
        .expect("snapshot dereferenced.field_values")
        .iter()
        .map(|e| {
            assert_eq!(e["kind"].as_str(), Some("Int"));
            e["i"].as_i64().expect("snap field i")
        })
        .collect();
    assert_eq!(snap_fields, vec![3, 4]);

    let mut_ref = p_refs[1];
    assert_eq!(mut_ref["mutable"].as_bool(), Some(true));
    // Second reference emission uses the next address slot
    // (0x1000 + 0x10 = 0x1010 = 4112).
    assert_eq!(mut_ref["address"].as_u64(), Some(0x1010));
    assert_eq!(mut_ref["dereferenced"]["kind"].as_str(), Some("Struct"));
    let mut_fields: Vec<i64> = mut_ref["dereferenced"]["field_values"]
        .as_array()
        .expect("mut dereferenced.field_values")
        .iter()
        .map(|e| {
            assert_eq!(e["kind"].as_str(), Some("Int"));
            e["i"].as_i64().expect("mut field i")
        })
        .collect();
    assert_eq!(mut_fields, vec![10, 20]);

    // Strict scalar emissions for the surrounding bindings.  origin
    // and shift are Struct; total resolves to 7 + 60 = 67 (read_only
    // returns 3+4=7; scale doubles shift to (20,40) → scaled_total=60).
    let scalar_only = observed_var_sequence_filtered(&doc, &["origin", "shift", "p"]);
    assert_eq!(
        scalar_only,
        vec![("total".to_string(), 67), ("return_value".to_string(), 67),]
    );
}

// --- numeric_widths_test.cairo --------------------------------------------

/// Records `numeric_widths_test.cairo`.  Pins the M10 round-2
/// numeric-width pin: each `let <name>: <T> = <int_lit>;` line where
/// `<T>` is one of u8/u16/u32/u64/u128/i8/i16/i32/i64/i128 surfaces as
/// a `ValueRecord::Int` against a per-width type id (the `types` array
/// gains a dedicated lang_type entry per width — `"u8"`, `"i64"`,
/// etc.), distinct from the shared felt252 carrier.  `u256` surfaces
/// as a dedicated `ValueRecord::Struct` with two `u128` halves matching
/// Cairo's 2×u128 representation.
#[test]
fn test_numeric_widths_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_numeric_widths_test_via_ct_print_full",
        "numeric_widths_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(bare_fns, vec!["main", "use_widths"]);

    // Every recognised width must appear in the trace's type table —
    // that's the M10 round-2 width pin: a regression to the shared
    // felt252 type id would drop these entries.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        types,
        vec![
            "felt252",
            "u8",
            "type_1",
            "u16",
            "type_3",
            "u32",
            "type_5",
            "u64",
            "type_7",
            "u128",
            "type_9",
            "i8",
            "type_11",
            "i16",
            "type_13",
            "i32",
            "type_15",
            "i64",
            "type_17",
            "i128",
            "type_19",
            "u256{low,high}",
            "type_0",
        ]
    );

    // ----- Per-binding kind sequence — every bounded-width binding
    //       surfaces as Int, u_big as Struct, total / return_value
    //       remain Int.  Source order is preserved.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("a8".to_string(), "Int".to_string()),
            ("a16".to_string(), "Int".to_string()),
            ("a32".to_string(), "Int".to_string()),
            ("a64".to_string(), "Int".to_string()),
            ("a128".to_string(), "Int".to_string()),
            ("s8".to_string(), "Int".to_string()),
            ("s16".to_string(), "Int".to_string()),
            ("s32".to_string(), "Int".to_string()),
            ("s64".to_string(), "Int".to_string()),
            ("s128".to_string(), "Int".to_string()),
            ("u_big".to_string(), "Struct".to_string()),
            ("total".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // ----- Per-width Int values ---------------------------------------
    // Helper: walk the type-id assignments registered by the writer to
    // find which TypeKind the recorder used for `<name>`.  We assert
    // both the value (so a regression in literal parsing fails loudly)
    // AND that the type-id maps back to a registered lang_type matching
    // the declared width.
    let lookup_width_type = |name: &str| -> String {
        let value = find_var_value(&doc, name).expect("var present");
        let type_id = value["type_id"].as_u64().expect("type_id u64") as usize;
        // The Nim writer interleaves the user-registered lang_type
        // with auto-generated `type_<id>` aliases — the recorder's
        // user-supplied lang_type lives at `type_id - 1` (the writer
        // emits the user lang_type entry, then assigns the user-facing
        // type_id one slot later as an alias).  This is a stable
        // property of the writer's id assignment; if it changes, this
        // helper needs an update — but the underlying lang_type still
        // appears in the types array (asserted above).
        types
            .get(type_id - 1)
            .map(|s| s.to_string())
            .unwrap_or_else(|| panic!("type_id {} out of range", type_id))
    };

    let cases: &[(&str, &str, i64)] = &[
        ("a8", "u8", 254),
        ("a16", "u16", 65534),
        ("a32", "u32", 4_000_000_000),
        ("a64", "u64", 9_000_000_000_000_000_000),
        ("a128", "u128", 1_234_567_890_123_456_789),
        ("s8", "i8", -127),
        ("s16", "i16", -32767),
        ("s32", "i32", -2_000_000_000),
        ("s64", "i64", -9_000_000_000_000_000_000),
        ("s128", "i128", -123_456_789_012_345),
    ];
    for (name, want_type, want_value) in cases {
        let value = find_var_value(&doc, name).expect("var present");
        assert_eq!(
            value["kind"].as_str(),
            Some("Int"),
            "{name} should decode as Int; got {value}"
        );
        assert_eq!(
            value["i"].as_i64(),
            Some(*want_value),
            "{name} value mismatch"
        );
        let resolved = lookup_width_type(name);
        assert_eq!(
            resolved, *want_type,
            "{name} should resolve to {want_type}; got {resolved}"
        );
    }

    // ----- u256 strict shape ------------------------------------------
    // Cairo's u256 = struct { low: u128, high: u128 }.  Literal 0
    // splits to (low=0, high=0); the struct's lang_type is the
    // dedicated `"u256{low,high}"` entry asserted in the types-table
    // check above.
    let u256_value = find_var_value(&doc, "u_big").expect("u_big present");
    assert_eq!(u256_value["kind"].as_str(), Some("Struct"));
    let u256_fields: Vec<i64> = u256_value["field_values"]
        .as_array()
        .expect("u256.field_values")
        .iter()
        .map(|e| {
            assert_eq!(
                e["kind"].as_str(),
                Some("Int"),
                "u256 half should be Int; got {e}"
            );
            e["i"].as_i64().expect("u256 half i")
        })
        .collect();
    assert_eq!(u256_fields, vec![0, 0]);
}

// --- array_operations_test.cairo ------------------------------------------

/// Records `array_operations_test.cairo`.  Pins the M10 round-2 array
/// operations pin: the `array![1, 2, 3, 4]` macro form initialises an
/// `Array<felt252>` compound binding (pre-fix only `ArrayTrait::new()`
/// + `.append(...)` was recognised), `<arr>.pop_front();` re-emits
/// the array's contents post-mutation as a new
/// `ValueRecord::Sequence` step variable, and `*<arr>.at(idx)` reads
/// the element at the given index without mutating the array.
#[test]
fn test_array_operations_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_array_operations_test_via_ct_print_full",
        "array_operations_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(bare_fns, vec!["main", "use_array"]);

    let counts = &doc["counts"];
    // Step events: implicit start(1) + main(2) + use_array(4) +
    // trailing return_value(1) = 8.
    assert_eq!(counts["steps"].as_u64(), Some(8), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 8 steps + 2 call_entry + 2 call_exit = 12 events.
    assert_eq!(events.len(), 12, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Per-binding kind sequence ----------------------------------
    // The array `a` surfaces as a `Sequence` twice — once at the
    // `array![]` initialiser line, once after `pop_front()`.  `head`
    // and `return_value` are scalar Int.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("a".to_string(), "Sequence".to_string()),
            ("a".to_string(), "Sequence".to_string()),
            ("head".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // ----- Strict array-shape assertions ------------------------------
    // Walk the events in order; the first `a` Sequence is the literal
    // initialiser ([1, 2, 3, 4]), the second is post-pop_front
    // ([2, 3, 4]).
    let a_emissions: Vec<Vec<i64>> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| e["vars"].as_array().map(|a| a.iter()).into_iter().flatten())
        .filter(|v| {
            v["varname"].as_str() == Some("a") && v["value"]["kind"].as_str() == Some("Sequence")
        })
        .map(|v| {
            v["value"]["elements"]
                .as_array()
                .expect("a.elements")
                .iter()
                .map(|e| {
                    assert_eq!(e["kind"].as_str(), Some("Int"));
                    e["i"].as_i64().expect("a element i")
                })
                .collect()
        })
        .collect();
    assert_eq!(a_emissions, vec![vec![1, 2, 3, 4], vec![2, 3, 4]]);

    // ----- `head` is the element at index 0 *after* pop_front,
    //       i.e. 2 (the original array's second element).
    let scalar_only = observed_var_sequence_filtered(&doc, &["a"]);
    assert_eq!(
        scalar_only,
        vec![("head".to_string(), 2), ("return_value".to_string(), 2),]
    );

    // ----- Per-callee return values: use_array returns 2; main
    //       delegates.
    let exit_returns: Vec<(String, i64)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            let i = e["return_value"]["i"].as_i64().unwrap_or_else(|| {
                panic!(
                    "call_exit.return_value must be Int; got {}",
                    e["return_value"]
                )
            });
            (name, i)
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and use_array co-exit
    // at the same step, so call_exit events at that step now appear in
    // entry-key order (main before use_array) rather than the previous
    // inverse-LIFO order.
    assert_eq!(
        exit_returns,
        vec![("main".to_string(), 2), ("use_array".to_string(), 2),]
    );
}

// --- event_test (snforge-converter path) ----------------------------------

/// Records the event_test snforge JSON fixture through
/// `starknet::write_starknet_trace` and asserts that the `#[event]` /
/// `self.emit(...)` event surfaces as a tagged `StarknetEvent` io_event
/// with the indexed (`#[key]`) fields distinguishable from data fields
/// in the embedded payload.  The matching Cairo source lives at
/// `test-programs/cairo/event_test.cairo` for documentation but is not
/// compiled by the recorder (it has no `fn main` and the
/// `#[starknet::contract]` dispatcher requires a separate runtime that
/// the in-process Sierra runner does not provide).
#[test]
fn test_event_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_event_test_via_ct_print_full") else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("event_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse event_test snforge trace");

    // Sanity-check the parsed entry shape — one contract_call followed
    // by one event entry.
    assert_eq!(entries.len(), 2, "expected 2 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // Function table: the contract_call entry registers one frame.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<String> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap().to_string())
        .collect();
    assert_eq!(bare_fns, vec!["transfer".to_string()]);

    // Counts: 1 contract_call (1 step + 1 call_entry + 1 call_exit)
    // + 1 event (1 step + 1 io_event) + the implicit `start()` step
    // at line 1 = 3 steps total.
    let counts = &doc["counts"];
    assert_eq!(counts["calls"].as_u64(), Some(1), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(3), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(1),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 3 steps + 1 call_entry + 1 call_exit + 1 io_event = 6 events.
    assert_eq!(events.len(), 6, "events.len()");

    // ----- io_event sequence -----------------------------------------
    // The event emits as `EventLogKind::EvmEvent`, which the
    // multi-stream writer maps to `ioStderr`.  The text field carries
    // both the canonical `StarknetEvent:<contract>` tag (so consumers
    // can dispatch on it) and the structured `keys=[...] data=[...]`
    // payload — `keys` holds the indexed (`#[key]`) fields, `data`
    // the remainder.  The Transfer event in the fixture has two
    // indexed values (the discriminator + the `from` address) and two
    // data values (the `to` address + amount).
    let io_events: Vec<&serde_json::Value> = events.iter().filter(|e| e["kind"] == "io").collect();
    assert_eq!(
        io_events.len(),
        1,
        "expected exactly one io event for the emitted Transfer; got {io_events:?}"
    );
    let ev = io_events[0];
    assert_eq!(
        ev["io_kind"].as_str(),
        Some("ioStderr"),
        "StarkNet event must surface as ioStderr (multi-stream writer's \
         tag for EvmEvent); got {ev}"
    );
    let text = ev["text"].as_str().expect("io.text str");
    // Strict-shape pin: the exact text format combines the
    // `StarknetEvent:<contract>` metadata tag with the structured
    // keys / data lists in source order.
    assert_eq!(
        text, "StarknetEvent:0xbeef keys=[Transfer, 0xaaa] data=[0xbbb, 100]",
        "io text must carry the canonical StarknetEvent metadata tag plus \
         the structured keys=[…] data=[…] payload"
    );

    // ----- Call-entry args carry the contract_call's calldata as
    //       canonical args (audit (b)).  This pins the wider snforge
    //       converter shape — the events fixture exercises the same
    //       calldata-as-args flow as storage_test, but with three
    //       calldata values instead of one.
    let call_entry = events
        .iter()
        .find(|e| e["kind"] == "call_entry")
        .expect("call_entry event");
    let arg_pairs: Vec<(String, String)> = call_entry["args"]
        .as_array()
        .expect("call_entry.args array")
        .iter()
        .map(|a| {
            let name = a["varname"].as_str().expect("varname str").to_string();
            let value = a["value"]["text"].as_str().expect("text str").to_string();
            (name, value)
        })
        .collect();
    assert_eq!(
        arg_pairs,
        vec![
            ("caller".to_string(), "0x1".to_string()),
            ("callee".to_string(), "0xbeef".to_string()),
            ("selector".to_string(), "transfer".to_string()),
            ("calldata0".to_string(), "0xaaa".to_string()),
            ("calldata1".to_string(), "0xbbb".to_string()),
            ("calldata2".to_string(), "100".to_string()),
        ]
    );
}

// ===========================================================================
// M10 round 3 — five additional fixtures pinning generic functions, trait
// impls, Span<T> slice views, deep `match` patterns, and StarkNet syscalls.
// Each fixture follows the same strict-`_via_ct_print_full` shape: every
// assertion uses `assert_eq!` against an exact recorded shape (counts,
// function tables, decoded JSON values).  No `contains`, no source-text
// `assert!`, no soft `>=` checks — a regression flips the exact tuple and
// fails the test loudly.
// ===========================================================================

// --- generic_function_test.cairo ------------------------------------------

/// Records `generic_function_test.cairo`.  The fixture defines a generic
/// `min<T, +PartialOrd<T>, +Copy<T>, +Drop<T>>(a: T, b: T) -> T` and
/// instantiates it twice — once at `u32` (from `pick_u32`) and once at
/// `u64` (from `pick_u64`).  Cairo's Sierra optimiser inlines the
/// trivial `min` body into each caller, so the function table contains
/// `main`, `pick_u32`, `pick_u64` (3 entries) — but the per-binding
/// type-id pin captures the two distinguishable generic instantiations
/// via the per-width `u32` / `u64` Int type ids on the `lo` / `hi`
/// locals.
///
/// The spec described the pin against `min<felt252>` and `min<u32>`,
/// but `felt252` carries no `PartialOrd` impl in the corelib so the
/// fixture instantiates at `u32` and `u64` — the property under test
/// (two distinguishable TypeIds for the same generic body) is
/// preserved.
#[test]
fn test_generic_function_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_generic_function_test_via_ct_print_full",
        "generic_function_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order: main → pick_u32 → pick_u64.  `min` is inlined
    // by the Sierra optimiser, so it never surfaces as its own frame.
    assert_eq!(bare_fns, vec!["main", "pick_u32", "pick_u64"]);

    // Type table — felt252 (the writer's default scalar carrier),
    // followed by the dedicated u32 / u64 type ids the recorder
    // registered for the generic-function instantiations' bounded-width
    // locals.  The interleaved `type_<id>` aliases come from the Nim
    // writer's id-assignment pattern (a user-registered lang_type entry
    // then a synthetic `type_<id>` alias one slot later).
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "u32", "type_1", "u64", "type_3"]);

    let counts = &doc["counts"];
    // 16 step events: implicit start(1) + main(2 body lines: 33,34 +
    // 35 + 36) + pick_u32(4 body lines: 19,20,21,22) + pick_u64(4
    // body lines: 26,27,28,29) + main resume(2: 35,36) + trailing(1).
    assert_eq!(counts["steps"].as_u64(), Some(16), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 16 steps + 3 call_entry + 3 call_exit = 22 events.
    assert_eq!(events.len(), 22, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "pick_u32".to_string(),
            "pick_u64".to_string(),
        ]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "pick_u32".to_string(),
            "pick_u64".to_string(),
            "main".to_string(),
        ]
    );

    // ----- Per-binding kind sequence — every `let` site for the
    //       generic locals surfaces as a typed Int.  The recorder does
    //       not emit a `return_value` step variable for `main` because
    //       its tail `total` resolves to a felt252 expression rather
    //       than an int-literal binding the recorder's
    //       `var_values` map can seed.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("lo".to_string(), "Int".to_string()),
            ("hi".to_string(), "Int".to_string()),
            ("lo".to_string(), "Int".to_string()),
            ("hi".to_string(), "Int".to_string()),
        ]
    );

    // ----- Per-instantiation type-id pin -------------------------------
    // Walk the four `lo`/`hi` emissions in source order and assert that
    // pick_u32's pair shares one type_id and pick_u64's pair shares
    // another — distinct from the first.  This is the strict pin the
    // spec is after: the same generic source body produces two
    // distinguishable Int type ids in the trace, one per
    // instantiation.
    let lo_hi_emissions: Vec<(String, u64)> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| e["vars"].as_array().map(|a| a.iter()).into_iter().flatten())
        .filter(|v| {
            let n = v["varname"].as_str();
            n == Some("lo") || n == Some("hi")
        })
        .map(|v| {
            let name = v["varname"].as_str().expect("varname str").to_string();
            let id = v["value"]["type_id"].as_u64().expect("type_id u64");
            (name, id)
        })
        .collect();
    assert_eq!(
        lo_hi_emissions,
        vec![
            ("lo".to_string(), 2),
            ("hi".to_string(), 2),
            ("lo".to_string(), 4),
            ("hi".to_string(), 4),
        ]
    );

    // ----- Per-width literal value pin --------------------------------
    // The first `lo`/`hi` pair are pick_u32's (5, 7); the second are
    // pick_u64's (11, 13).  Combined with the type-id pin above, this
    // pins both the value AND the per-instantiation type identity.
    let lo_hi_values: Vec<(String, i64)> = events
        .iter()
        .filter(|e| e["kind"] == "step")
        .flat_map(|e| e["vars"].as_array().map(|a| a.iter()).into_iter().flatten())
        .filter(|v| {
            let n = v["varname"].as_str();
            n == Some("lo") || n == Some("hi")
        })
        .map(|v| {
            let name = v["varname"].as_str().expect("varname str").to_string();
            let i = v["value"]["i"].as_i64().expect("Int.i");
            (name, i)
        })
        .collect();
    assert_eq!(
        lo_hi_values,
        vec![
            ("lo".to_string(), 5),
            ("hi".to_string(), 7),
            ("lo".to_string(), 11),
            ("hi".to_string(), 13),
        ]
    );
}

// --- trait_impl_test.cairo -------------------------------------------------

/// Records `trait_impl_test.cairo`.  The fixture defines a `Greeter`
/// trait with two concrete impls (`HelloImpl`, `HiImpl`), and a
/// driver that calls each via direct impl-path syntax
/// (`HelloImpl::greet(...)` / `HiImpl::greet(...)`).  The recorder
/// surfaces each impl method as its own entry in the function table
/// with the module-qualified Sierra name (`HelloImpl::greet` /
/// `HiImpl::greet`), and trait dispatch surfaces as a Call/Return
/// pair pointing at the concrete impl rather than the abstract trait
/// method.
///
/// The `#[inline(never)]` attribute on each impl keeps the Sierra
/// optimiser from collapsing the impls into the driver.  The
/// `parse_callees_in_line` recogniser was extended in this round to
/// handle two-segment `<TypeName>::<method>(` calls so the
/// impl-qualified DFS recurses on each call site distinctly.
#[test]
fn test_trait_impl_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_trait_impl_test_via_ct_print_full",
        "trait_impl_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // DFS visit order: main → drive → HelloImpl::greet → HiImpl::greet.
    // The full Sierra names carry the `<crate>::<crate>::` prefix.
    let bare_fns: Vec<String> = functions
        .iter()
        .map(|f| {
            // Recover the impl-qualified suffix: take the last two
            // `::`-segments for impl methods (e.g. `HelloImpl::greet`)
            // and the last segment for free functions (e.g. `main`).
            let segs: Vec<&str> = f.split("::").collect();
            if segs.len() >= 2 && segs[segs.len() - 2].ends_with("Impl") {
                format!("{}::{}", segs[segs.len() - 2], segs[segs.len() - 1])
            } else {
                segs.last().unwrap().to_string()
            }
        })
        .collect();
    assert_eq!(
        bare_fns,
        vec![
            "main".to_string(),
            "drive".to_string(),
            "HelloImpl::greet".to_string(),
            "HiImpl::greet".to_string(),
        ]
    );

    let counts = &doc["counts"];
    // Step events: implicit start(1) + main(2 body lines: 47,48) +
    // drive(5: 39,40,41,42,43,44 — `}` skipped) + HelloImpl::greet(2:
    // 26,27 — `}` skipped) + HiImpl::greet(2: 33,34 — `}` skipped) +
    // main trailing(1: 49) + recorder trailing-step(1).  14 total.
    assert_eq!(counts["steps"].as_u64(), Some(14), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 14 steps + 4 call_entry + 4 call_exit = 22 events.
    assert_eq!(events.len(), 22, "events.len()");
    assert_step_indices_monotonic(&doc);

    // ----- Call sequence & exit ordering ------------------------------
    // DFS from main visits drive, then drive's two impl-qualified
    // callees in source order.  Each impl call_exit fires before its
    // caller's, giving the canonical LIFO post-order.
    let bare_call_seq: Vec<String> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            let f = e["function"].as_str().expect("function str");
            let segs: Vec<&str> = f.split("::").collect();
            if segs.len() >= 2 && segs[segs.len() - 2].ends_with("Impl") {
                format!("{}::{}", segs[segs.len() - 2], segs[segs.len() - 1])
            } else {
                segs.last().unwrap().to_string()
            }
        })
        .collect();
    assert_eq!(
        bare_call_seq,
        vec![
            "main".to_string(),
            "drive".to_string(),
            "HelloImpl::greet".to_string(),
            "HiImpl::greet".to_string(),
        ]
    );

    let bare_exit_seq: Vec<String> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let f = e["function"].as_str().expect("function str");
            let segs: Vec<&str> = f.split("::").collect();
            if segs.len() >= 2 && segs[segs.len() - 2].ends_with("Impl") {
                format!("{}::{}", segs[segs.len() - 2], segs[segs.len() - 1])
            } else {
                segs.last().unwrap().to_string()
            }
        })
        .collect();
    assert_eq!(
        bare_exit_seq,
        vec![
            "HelloImpl::greet".to_string(),
            "HiImpl::greet".to_string(),
            "drive".to_string(),
            "main".to_string(),
        ]
    );

    // ----- Concrete-impl return-value pin -----------------------------
    // The recorder's static return-value heuristic recovers values for
    // (a) tuple returns whose slots map onto VM `Success` payload
    // entries, (b) bare-identifier returns whose name lives in
    // `var_values`, and (c) single-callee delegations.  Each impl
    // method's body collapses to a numeric literal (`7` / `11`) which
    // the heuristic deliberately skips (an integer-literal tail is
    // not a variable reference — see the `is_ascii_digit` guard in
    // `compute_function_return_values`).  drive's `a + b` and main's
    // `r.into()` also fail the heuristic.  Every call_exit therefore
    // surfaces a Void return value.  Pin the kind on each so a
    // regression to the wrong concrete impl (e.g. surfacing the
    // abstract trait method's return shape) flips the assertion.
    let exit_kinds: Vec<(String, String)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let f = e["function"].as_str().expect("function str");
            let segs: Vec<&str> = f.split("::").collect();
            let bare = if segs.len() >= 2 && segs[segs.len() - 2].ends_with("Impl") {
                format!("{}::{}", segs[segs.len() - 2], segs[segs.len() - 1])
            } else {
                segs.last().unwrap().to_string()
            };
            let kind = e["return_value"]["kind"]
                .as_str()
                .expect("return_value.kind")
                .to_string();
            (bare, kind)
        })
        .collect();
    assert_eq!(
        exit_kinds,
        vec![
            ("HelloImpl::greet".to_string(), "Void".to_string()),
            ("HiImpl::greet".to_string(), "Void".to_string()),
            ("drive".to_string(), "Void".to_string()),
            ("main".to_string(), "Void".to_string()),
        ]
    );
}

// --- span_test.cairo ------------------------------------------------------

/// Records `span_test.cairo`.  The fixture builds an
/// `array![10_u32, 20_u32, 30_u32]`, takes a `.span()` view into the
/// `view` binding, and passes it into `sum_span(items: Span<u32>)`.
/// The recorder pins:
///
/// * `xs` — the source `array![]` literal as a
///   `ValueRecord::Sequence` with `is_slice: false` against the
///   shared `Array<felt252>` type id.
/// * `view` — the slice-view binding from `xs.span()` as a
///   `ValueRecord::Sequence` against a dedicated `Span<felt252>`
///   type id and `is_slice = true`.  The Nim writer's FFI now
///   threads the discriminator through
///   `ct_value_begin_sequence_with_slice`, so the slice/owned split
///   surfaces both via the dedicated type id and via the field-level
///   `is_slice` flag.
///
/// The recorder does NOT yet propagate the Span carrier into the
/// callee's `items` parameter binding; that's a round-4 concern.
#[test]
fn test_span_test_via_ct_print_full() {
    let Some((doc, source_path)) =
        record_and_dump_full("test_span_test_via_ct_print_full", "span_test.cairo")
    else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(bare_fns, vec!["main", "sum_span"]);

    // Type table — felt252, the shared Array<felt252> id, the new
    // Span<felt252> id (M10 round-3), the per-width u32 id, and the
    // synthetic `type_<id>` aliases the writer interleaves.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        types,
        vec![
            "felt252",
            "Array<felt252>",
            "Span<felt252>",
            "u32",
            "type_3",
            "type_0",
        ]
    );

    let counts = &doc["counts"];
    // The `while` loop simulator runs sum_span's `while i < 3 { ... }`
    // for three iterations + a closing header check, so the step
    // count grows beyond the static-walk's per-line shape.  20 total:
    // start(1) + main(3: 32,33,34) + sum_span(8 incl loop iter) +
    // main trailing(1: 35) + recorder trailing(1) + extras from the
    // simulator's per-iteration re-emissions.
    assert_eq!(counts["steps"].as_u64(), Some(20), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 20 steps + 2 call_entry + 2 call_exit = 24 events.
    assert_eq!(events.len(), 24, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec!["main".to_string(), "sum_span".to_string()]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec!["sum_span".to_string(), "main".to_string()]
    );

    // ----- Per-binding kind sequence ---------------------------------
    // `xs` (Array literal) and `view` (Span view) are both Sequence;
    // `total` and `i` are Int (emitted by sum_span's typed-int
    // bindings + the while-loop simulator's per-iteration updates).
    // Multiple `i` updates fall out of the simulator's iteration
    // model — pin the exact emission sequence.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("xs".to_string(), "Sequence".to_string()),
            ("view".to_string(), "Sequence".to_string()),
            ("total".to_string(), "Int".to_string()),
            ("i".to_string(), "Int".to_string()),
            ("i".to_string(), "Int".to_string()),
            ("i".to_string(), "Int".to_string()),
            ("i".to_string(), "Int".to_string()),
        ]
    );

    // ----- Strict shape for the Array literal `xs` -------------------
    let xs_value = find_var_value(&doc, "xs").expect("xs step variable");
    assert_eq!(xs_value["kind"].as_str(), Some("Sequence"));
    assert_eq!(xs_value["is_slice"].as_bool(), Some(false));
    let xs_elements: Vec<i64> = xs_value["elements"]
        .as_array()
        .expect("xs.elements")
        .iter()
        .map(|e| {
            assert_eq!(e["kind"].as_str(), Some("Int"));
            e["i"].as_i64().expect("xs element i")
        })
        .collect();
    assert_eq!(xs_elements, vec![10, 20, 30]);
    // `xs` rides the shared `Array<felt252>` type id (slot 1 in the
    // types table).
    assert_eq!(xs_value["type_id"].as_u64(), Some(1));

    // ----- Strict shape for the Span view `view` ---------------------
    let view_value = find_var_value(&doc, "view").expect("view step variable");
    assert_eq!(view_value["kind"].as_str(), Some("Sequence"));
    // The recorder pins `Span<felt252>` to slice/view semantics; the
    // FFI now threads the discriminator end-to-end via
    // `ct_value_begin_sequence_with_slice`, so the field-level
    // `is_slice` flag matches the dedicated `Span<felt252>` type id.
    assert_eq!(view_value["is_slice"].as_bool(), Some(true));
    assert_eq!(view_value["type_id"].as_u64(), Some(2));
    let view_elements: Vec<i64> = view_value["elements"]
        .as_array()
        .expect("view.elements")
        .iter()
        .map(|e| {
            assert_eq!(e["kind"].as_str(), Some("Int"));
            e["i"].as_i64().expect("view element i")
        })
        .collect();
    assert_eq!(view_elements, vec![10, 20, 30]);
}

// --- match_pattern_test.cairo ---------------------------------------------

/// Records `match_pattern_test.cairo`.  The fixture exercises a deep
/// `match` over `Result<Option<u32>, felt252>` with arms `Ok(Some(n))`,
/// `Ok(None)`, and `Err(_)`.  `run_all` builds three different inputs
/// and calls `classify` on each.
///
/// Strict pin (current recorder behaviour): the recorder's
/// first-touch DFS visits each function exactly once, so `classify`
/// surfaces a single Call/Return frame instead of three even though
/// the source calls it three times.  Per-arm pattern-binding
/// extraction (the matched `n` becoming a typed Int local at the
/// `Ok(Some(n))` arm) stays for round 4.  The pin asserts on:
///
/// * function table (DFS order from main).
/// * counts (3 calls, the visited-once shape; one Step per source line).
/// * the one Variant emission `parse_variant_literal_decl` recovers
///   (`err_val: Result::Err(99)` — the only single-level
///   integer-payload variant in the fixture).
/// * `classify`'s call_exit return value (the static return-value
///   heuristic propagates the `Err(_)` arm's literal `200` because
///   that's the body's tail-expression integer).
#[test]
fn test_match_pattern_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_match_pattern_test_via_ct_print_full",
        "match_pattern_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    assert_eq!(bare_fns, vec!["main", "run_all", "classify"]);

    let counts = &doc["counts"];
    // Step events: implicit start(1) + main(2: 47,48) + run_all(7:
    // 37,38,39,40,41,42,43,44) + classify(7: 27,28,29,30,31,32,33) +
    // main trailing(1: 49) + recorder trailing(1) = 19.
    assert_eq!(counts["steps"].as_u64(), Some(19), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 19 steps + 3 call_entry + 3 call_exit = 25 events.
    assert_eq!(events.len(), 25, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "run_all".to_string(),
            "classify".to_string(),
        ]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "classify".to_string(),
            "run_all".to_string(),
            "main".to_string(),
        ]
    );

    // ----- Per-binding kind sequence ---------------------------------
    // `parse_variant_literal_decl` matches only single-level
    // Option/Result literals with integer payloads.  Of the three
    // bindings in `run_all`:
    //   * `some_val = Result::Ok(Option::Some(7))` — nested, falls
    //     through.
    //   * `none_val = Result::Ok(Option::None)`   — nested, falls
    //     through.
    //   * `err_val  = Result::Err(99)`            — single-level
    //     integer payload, surfaces as a Variant.
    // No other `let`-bindings on this fixture's path produce step
    // variable rows under the existing recorder heuristics.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![("err_val".to_string(), "Variant".to_string())]
    );

    // ----- Strict variant-shape assertion ----------------------------
    let err_value = find_var_value(&doc, "err_val").expect("err_val step variable");
    assert_eq!(err_value["kind"].as_str(), Some("Variant"));
    assert_eq!(err_value["discriminator"].as_str(), Some("Err"));
    assert_eq!(err_value["contents"]["kind"].as_str(), Some("Int"));
    assert_eq!(err_value["contents"]["i"].as_i64(), Some(99));

    // ----- Per-callee return-value pin -------------------------------
    // classify's tail is a `match` expression — its overall shape
    // doesn't fit any of the recorder's recognised return-value
    // heuristics (tuple slot, bare-ident binding, single-callee
    // delegation).  run_all and main also fail the heuristic.  Pin
    // every call_exit kind to Void so a regression that wrongly
    // claims a value (e.g. by leaking a numeric literal from a body
    // arm into the VM's `Success` payload mapping) flips the
    // assertion.
    let exit_kinds: Vec<(String, String)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("function str")
                .rsplit("::")
                .next()
                .unwrap()
                .to_string();
            let kind = e["return_value"]["kind"]
                .as_str()
                .expect("return_value.kind")
                .to_string();
            (name, kind)
        })
        .collect();
    assert_eq!(
        exit_kinds,
        vec![
            ("classify".to_string(), "Void".to_string()),
            ("run_all".to_string(), "Void".to_string()),
            ("main".to_string(), "Void".to_string()),
        ]
    );
}

// --- syscalls_test_trace.json (snforge-converter path) --------------------

/// Records the syscalls_test snforge JSON fixture through
/// `starknet::write_starknet_trace` and asserts that each StarkNet
/// runtime syscall (`get_caller_address`, `get_block_timestamp`,
/// `get_contract_address`) surfaces as a Call/Return frame named
/// `<contract_address>::<syscall_name>` with the syscall's return
/// value typed correctly:
///
/// * `get_caller_address` / `get_contract_address` — `ValueRecord::Raw`
///   carrying the 32-byte big-endian felt as a `0x`-prefixed 64-char
///   hex string (zero-padded).  The dedicated `Address` type id is
///   shared across all `address`-typed syscall returns.
/// * `get_block_timestamp` — `ValueRecord::Int` against a dedicated
///   `u64` type id.
///
/// The matching Cairo source lives at
/// `test-programs/starknet/syscalls_test.cairo` for documentation but
/// is not compiled by the recorder (it has no `fn main` and the
/// `#[starknet::contract]` dispatcher requires a separate runtime).
#[test]
fn test_syscalls_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_syscalls_test_via_ct_print_full") else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("syscalls_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse syscalls_test snforge trace");

    // Sanity-check the parsed entry shape — three syscall entries.
    assert_eq!(entries.len(), 3, "expected 3 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // ----- Function table — one entry per syscall, qualified with the
    //       trace's `contract_address` (`0xfeed`) so consumers can
    //       group all of a contract's syscalls under one identity.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec![
            "0xfeed::get_caller_address",
            "0xfeed::get_block_timestamp",
            "0xfeed::get_contract_address",
        ]
    );

    // ----- counts ----------------------------------------------------
    // 3 syscalls → 3 call frames (one per entry) + 3 register_step
    // (one per entry) + the implicit `start()` step at line 1 = 4
    // step events.  Syscalls don't emit io_events.
    let counts = &doc["counts"];
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(4), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 4 steps + 3 call_entry + 3 call_exit = 10 events.
    assert_eq!(events.len(), 10, "events.len()");

    // ----- Type table ------------------------------------------------
    // felt252 (the shared snforge str carrier), Address (the
    // dedicated Raw type id for syscall address returns), u64 (the
    // dedicated Int type id for the block-timestamp), and the
    // writer's interleaved `type_<id>` alias.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "Address", "u64", "type_2"]);

    // ----- Per-syscall return-value shape ----------------------------
    // Strict pin: each call_exit carries the syscall's return value
    // typed by `return_kind`.  Address values surface as Raw with the
    // canonical 32-byte hex (zero-padded), the timestamp surfaces as
    // a typed Int.  The exact return values are read from the
    // fixture's JSON (deadbeef / 1700000000 / feedface).
    let exit_returns: Vec<(String, serde_json::Value)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            (
                e["function"].as_str().expect("function str").to_string(),
                e["return_value"].clone(),
            )
        })
        .collect();
    assert_eq!(exit_returns.len(), 3, "expected 3 call_exit events");
    assert_eq!(exit_returns[0].0, "0xfeed::get_caller_address");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Raw"));
    assert_eq!(
        exit_returns[0].1["r"].as_str(),
        Some("0x00000000000000000000000000000000000000000000000000000000deadbeef")
    );

    assert_eq!(exit_returns[1].0, "0xfeed::get_block_timestamp");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[1].1["i"].as_i64(), Some(1_700_000_000));

    assert_eq!(exit_returns[2].0, "0xfeed::get_contract_address");
    assert_eq!(exit_returns[2].1["kind"].as_str(), Some("Raw"));
    assert_eq!(
        exit_returns[2].1["r"].as_str(),
        Some("0x00000000000000000000000000000000000000000000000000000000feedface")
    );
}

// ===========================================================================
// M10 round 4 — five additional fixtures pinning ByteArray + felt252 short
// strings, Felt252Dict<T>, hash builtins (pedersen / poseidon), StarkNet
// visibility decorators (`#[external(v0)]` / `#[view]` / internal), and
// `#[starknet::component]` reusable behaviour modules.  Each fixture
// follows the same strict-`_via_ct_print_full` shape: every assertion
// uses `assert_eq!` against an exact recorded shape (counts, function
// tables, decoded JSON values).  No `contains`, no source-text
// `assert!`, no soft `>=` checks — a regression flips the exact tuple
// and fails the test loudly.
// ===========================================================================

// --- byte_array_short_string_test.cairo -----------------------------------

/// Records `byte_array_short_string_test.cairo`.  Pins the M10 round-4
/// short-string + ByteArray surface: the felt-encoded short string
/// `'STX_OK'` surfaces as a scalar `ValueRecord::Int` whose `i` field
/// matches the canonical big-endian ASCII encoding
/// (`0x53_54_58_5F_4F_4B = 91621724999499`); the `_greeting`
/// ByteArray binding does not yet surface as a typed Struct (the
/// recorder's source-level heuristic recognises scalar / Sequence /
/// Tuple / Struct-literal / Variant shapes only) but the call/return
/// frame for `make_greeting()` is captured and surfaces with a `Void`
/// return — pinning the gap so a future ByteArray decoder lands as a
/// new test variable rather than silently overwriting the current
/// contract.
#[test]
fn test_byte_array_short_string_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_byte_array_short_string_test_via_ct_print_full",
        "byte_array_short_string_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main: main → make_greeting → make_tag.
    assert_eq!(bare_fns, vec!["main", "make_greeting", "make_tag"]);

    // Type table: only the shared felt252 carrier and its writer-side
    // alias — neither ByteArray nor short-string add new lang_types
    // (short strings ride on felt252; ByteArray surfacing is a
    // downstream M11 extension).
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "type_0"]);

    let counts = &doc["counts"];
    // 9 step events: implicit start(1) + main body steps(2: 31, 34) +
    // make_greeting body(2: 32, 23) + make_tag body(2: 33, 27) +
    // main resume(1: 34) + trailing return_value step(1).
    assert_eq!(counts["steps"].as_u64(), Some(9), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 9 steps + 3 call_entry + 3 call_exit = 15 events.
    assert_eq!(events.len(), 15, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "make_greeting".to_string(),
            "make_tag".to_string(),
        ]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "make_greeting".to_string(),
            "make_tag".to_string(),
            "main".to_string(),
        ]
    );

    // Per-binding kind sequence: only `tag` (the short-string Int)
    // and the trailing `return_value` (Int) — the `_greeting` ByteArray
    // binding is *not* surfaced by the current recorder because the
    // source-level heuristic does not yet model ByteArray's struct
    // shape.  Pinning the kinds list explicitly so a future
    // ByteArray-emitting recorder lands as a new entry rather than
    // silently changing the contract.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("tag".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // The short-string `'STX_OK'` decodes as the canonical big-endian
    // felt252 encoding of the six ASCII bytes `S T X _ O K` =
    // 0x53_54_58_5F_4F_4B = 91621724999499.  This is the strict pin
    // the spec is after: a felt252 short-string surfaces as a scalar
    // Int with the printable ASCII recoverable from the big-endian
    // byte representation of `i`.
    let tag = find_var_value(&doc, "tag").expect("tag var");
    assert_eq!(tag["kind"].as_str(), Some("Int"));
    assert_eq!(tag["i"].as_i64(), Some(91_621_724_999_499));
    // Big-endian-byte recovery: stripping the leading zero bytes from
    // the i64 representation must yield the printable ASCII of the
    // source short-string.
    let i = tag["i"].as_i64().expect("tag.i");
    let mut bytes = i.to_be_bytes().to_vec();
    while bytes.first() == Some(&0u8) {
        bytes.remove(0);
    }
    assert_eq!(bytes, b"STX_OK".to_vec());

    // Per-callee call_exit return values:
    //   * make_greeting returns ByteArray — the recorder's source-level
    //     return-value heuristic doesn't recover felts from a
    //     ByteArray-typed return, so the exit value surfaces as Void.
    //   * make_tag returns the short-string Int 91621724999499.
    //   * main returns the short-string Int (delegating make_tag's
    //     return through the trailing tail expression).
    let exit_returns: Vec<(String, serde_json::Value)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            (name, e["return_value"].clone())
        })
        .collect();
    assert_eq!(exit_returns.len(), 3);
    assert_eq!(exit_returns[0].0, "make_greeting");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[1].0, "make_tag");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[1].1["i"].as_i64(), Some(91_621_724_999_499));
    assert_eq!(exit_returns[2].0, "main");
    assert_eq!(exit_returns[2].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[2].1["i"].as_i64(), Some(91_621_724_999_499));
}

// --- felt252_dict_test.cairo ----------------------------------------------

/// Records `felt252_dict_test.cairo`.  Pins the M10 round-4
/// `Felt252Dict<T>` surface: the corelib dispatch into
/// `Felt252DictTrait::insert` / `Felt252DictTrait::get` is *inlined*
/// by the Sierra optimiser at the call sites — so the function table
/// contains only the driver-side frames (`main` + `use_dict`), but
/// the value the driver inserted under `'alice'` (`100_u32`) flows
/// through `use_dict()`'s u32 return and surfaces on the
/// `call_exit.return_value` of `use_dict` as a typed
/// `ValueRecord::Int`.  The recorder does not yet emit a synthetic
/// `SquashedFelt252Dict` step variable — surfacing that final
/// entry-list snapshot is a downstream M11 extension; this fixture
/// pins the strict shape recorded today so it lands as a new test
/// variable rather than silently overwriting the current contract.
#[test]
fn test_felt252_dict_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_felt252_dict_test_via_ct_print_full",
        "felt252_dict_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main: main → use_dict.  The corelib
    // Felt252DictTrait::{insert,get} dispatch is inlined by the
    // Sierra optimiser, so neither surfaces as its own frame.
    assert_eq!(bare_fns, vec!["main", "use_dict"]);

    // Type table: only the shared felt252 carrier and its writer-side
    // alias — Felt252Dict's typed inner state isn't surfaced yet.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "type_0"]);

    let counts = &doc["counts"];
    // 10 step events: implicit start(1) + main body(2: 34, 36) +
    // use_dict body(6: 35, 26-30) + trailing return_value step(1).
    assert_eq!(counts["steps"].as_u64(), Some(10), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 10 steps + 2 call_entry + 2 call_exit = 14 events.
    assert_eq!(events.len(), 14, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec!["main".to_string(), "use_dict".to_string(),]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec!["use_dict".to_string(), "main".to_string(),]
    );

    // Per-binding kind sequence: only the `v` u32 binding (the typed
    // dict.get('alice') return) surfaces.  The dict literal itself
    // (`d`), the inserted values (100, 200), and the
    // SquashedFelt252Dict snapshot are *not* surfaced — pinning the
    // gap so a future dict-state recorder lands as a new entry.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![("v".to_string(), "Int".to_string()),]
    );

    // The `v` binding decodes to 100 — the value the driver inserted
    // under the `'alice'` key.  Pinning both the value and the
    // typed-Int kind so a regression in dict-return recovery fails
    // here loudly.
    let v = find_var_value(&doc, "v").expect("v var");
    assert_eq!(v["kind"].as_str(), Some("Int"));
    assert_eq!(v["i"].as_i64(), Some(100));

    // Per-callee call_exit return values:
    //   * use_dict returns u32 100 — the recovered dict.get('alice').
    //   * main returns felt252 (`r.into()`) — the recorder's
    //     source-level return-value heuristic doesn't yet model
    //     `<u32>.into()` so main's exit surfaces as Void.
    let exit_returns: Vec<(String, serde_json::Value)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            (name, e["return_value"].clone())
        })
        .collect();
    assert_eq!(exit_returns.len(), 2);
    assert_eq!(exit_returns[0].0, "use_dict");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[0].1["i"].as_i64(), Some(100));
    assert_eq!(exit_returns[1].0, "main");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Void"));
}

// --- hash_builtins_test.cairo ---------------------------------------------

/// Records `hash_builtins_test.cairo`.  Pins the M10 round-4 hash
/// builtin surface: each driver-side function (`use_pedersen`,
/// `use_poseidon`) surfaces as a Call/Return frame with the inlined
/// corelib dispatch into `pedersen::pedersen` /
/// `poseidon::hades_permutation` happening inside the body — the
/// Sierra optimiser folds the corelib trampoline into the caller, so
/// the hash builtin itself does not surface as its own frame.  The
/// strict pin is on the function table (driver-side only), the
/// call-entry / call-exit DFS / LIFO order, and the typed-Int return
/// value of `use_pedersen` (the recorder's source-level heuristic
/// recovers the `let h = pedersen(...)` binding as a scalar Int with
/// value 0 — the actual pedersen hash exceeds i64 range so the felt
/// surfaces as 0 today; pinning the value-as-recorded so a future
/// felt252-aware decoder lands as a new test variable rather than
/// silently overwriting the current contract).
#[test]
fn test_hash_builtins_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_hash_builtins_test_via_ct_print_full",
        "hash_builtins_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main: main → use_pedersen → use_poseidon.
    // The corelib pedersen / hades_permutation dispatch is inlined by
    // the Sierra optimiser, so neither surfaces as its own frame.
    assert_eq!(bare_fns, vec!["main", "use_pedersen", "use_poseidon"]);

    // Type table: only the shared felt252 carrier and its writer-side
    // alias — neither pedersen output (felt252) nor hades_permutation
    // tuple-return adds new lang_types.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "type_0"]);

    let counts = &doc["counts"];
    // 11 step events: implicit start(1) + main body(3: 51, 52, 53) +
    // use_pedersen body(2: 41, 42) + use_poseidon body(2: 46, 47) +
    // main resume(2: 48 + trailing return_value step at 54) =
    // 1+3+2+2+1+2 = 11.
    assert_eq!(counts["steps"].as_u64(), Some(11), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 11 steps + 3 call_entry + 3 call_exit = 17 events.
    assert_eq!(events.len(), 17, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "use_pedersen".to_string(),
            "use_poseidon".to_string(),
        ]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "use_pedersen".to_string(),
            "use_poseidon".to_string(),
            "main".to_string(),
        ]
    );

    // Per-binding kind sequence: only `h` (the use_pedersen-local Int
    // binding for the pedersen output) surfaces as a step variable.
    // The destructured `(s0, _s1, _s2)` tuple from hades_permutation
    // does not surface today — pinning the gap so a future
    // hash-tuple-aware decoder lands as new entries.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![("h".to_string(), "Int".to_string()),]
    );

    // The `h` binding decodes to 0: the actual pedersen(1, 2) output
    // exceeds i64 range so the recorder's source-level
    // return-value heuristic does not recover the real value and
    // falls back to 0.  Pinning the value-as-recorded so a future
    // felt252-aware decoder surfaces a new entry rather than
    // silently changing the contract.
    let h = find_var_value(&doc, "h").expect("h var");
    assert_eq!(h["kind"].as_str(), Some("Int"));
    assert_eq!(h["i"].as_i64(), Some(0));

    // Per-callee call_exit return values:
    //   * use_pedersen returns felt252 — surfaces as Int 0 (the
    //     bound `h` value, propagated by the source-level heuristic).
    //   * use_poseidon returns felt252 from a tuple destructure
    //     (`let (s0, _, _) = ...; s0`) — the heuristic doesn't model
    //     tuple destructure for return-value recovery, so the exit
    //     surfaces as Void.
    //   * main returns felt252 (`p + q`) — the heuristic doesn't
    //     model arithmetic-expression returns, so main's exit also
    //     surfaces as Void.
    let exit_returns: Vec<(String, serde_json::Value)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            (name, e["return_value"].clone())
        })
        .collect();
    assert_eq!(exit_returns.len(), 3);
    assert_eq!(exit_returns[0].0, "use_pedersen");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[0].1["i"].as_i64(), Some(0));
    assert_eq!(exit_returns[1].0, "use_poseidon");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[2].0, "main");
    assert_eq!(exit_returns[2].1["kind"].as_str(), Some("Void"));
}

// --- visibility_decorators_test (StarkNet trace) --------------------------

/// Records `visibility_decorators_test_trace.json`.  Pins the M10
/// round-4 visibility-decorator surface: each StarkNet `contract_call`
/// JSON entry now carries an optional `visibility` field (`"external"`
/// / `"view"` / `"internal"`) and an optional `self_kind` field
/// (`"ref"` / `"snapshot"`).  The converter writes the function name
/// as `<visibility>::<contract>::<selector>` so the function table
/// groups external / view / internal methods, and emits a typed
/// `ValueRecord::Reference` `self_kind` arg whose `mutable` flag is
/// `true` for `ref self` (external) and `false` for `@self` (view).
/// Internal callers omit the `self_kind` field so the arg is dropped
/// — pinning that the recorder keeps the @-vs-ref distinction visible
/// at the trace level.
#[test]
fn test_visibility_decorators_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_visibility_decorators_test_via_ct_print_full")
    else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("visibility_decorators_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse visibility_decorators snforge trace");

    // Sanity-check the parsed entry shape: three contract_call
    // entries, one per visibility class.
    assert_eq!(entries.len(), 3, "expected 3 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // Function table: one entry per visibility class, prefixed with
    // the visibility tag so consumers can group methods without
    // re-parsing the contract source.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec![
            "external::0xbabe::deposit",
            "internal::0xbabe::validate",
            "view::0xbabe::balance_of",
        ]
    );

    // Type table: shared felt252 (the snforge str carrier) plus the
    // dedicated `Self` type id the converter registers for the
    // ValueRecord::Reference `self_kind` args.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "Self"]);

    let counts = &doc["counts"];
    // 3 entries → 3 call frames + 3 register_step (one per entry) +
    // implicit start() step at line 1 = 4 step events.  No
    // storage_read / storage_write entries → 0 io_events.
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(4), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 4 steps + 3 call_entry + 3 call_exit = 10 events.
    assert_eq!(events.len(), 10, "events.len()");

    // ----- Per-call self_kind arg shape ------------------------------
    // Walk the call_entry events in source order and extract the
    // `self_kind` arg's typed shape: external surfaces as Reference
    // mutable=true; view surfaces as Reference mutable=false;
    // internal omits the arg entirely.
    let self_kinds: Vec<(String, Option<bool>)> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            let fn_name = e["function"].as_str().expect("function str").to_string();
            let self_kind = e["args"]
                .as_array()
                .expect("call_entry.args array")
                .iter()
                .find(|a| a["varname"].as_str() == Some("self_kind"))
                .map(|a| {
                    assert_eq!(
                        a["value"]["kind"].as_str(),
                        Some("Reference"),
                        "self_kind must surface as Reference; got {}",
                        a["value"]
                    );
                    a["value"]["mutable"]
                        .as_bool()
                        .expect("Reference.mutable bool")
                });
            (fn_name, self_kind)
        })
        .collect();
    assert_eq!(
        self_kinds,
        vec![
            ("external::0xbabe::deposit".to_string(), Some(true)),
            ("internal::0xbabe::validate".to_string(), None),
            ("view::0xbabe::balance_of".to_string(), Some(false)),
        ]
    );

    // ----- Calldata still flows through the canonical `args`
    //       channel.  Pin the deposit's `calldata0` and the
    //       validate's `calldata0` so a regression in the args
    //       routing fails here loudly.
    let arg_pairs: Vec<(String, Vec<(String, String)>)> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            let fn_name = e["function"].as_str().expect("function str").to_string();
            let pairs: Vec<(String, String)> = e["args"]
                .as_array()
                .expect("call_entry.args array")
                .iter()
                .filter_map(|a| {
                    let name = a["varname"].as_str()?.to_string();
                    if a["value"]["kind"].as_str() == Some("String") {
                        Some((name, a["value"]["text"].as_str()?.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
            (fn_name, pairs)
        })
        .collect();
    assert_eq!(
        arg_pairs,
        vec![
            (
                "external::0xbabe::deposit".to_string(),
                vec![
                    ("caller".to_string(), "0x1".to_string()),
                    ("callee".to_string(), "0xbabe".to_string()),
                    ("selector".to_string(), "deposit".to_string()),
                    ("calldata0".to_string(), "50".to_string()),
                ],
            ),
            (
                "internal::0xbabe::validate".to_string(),
                vec![
                    ("caller".to_string(), "0xbabe".to_string()),
                    ("callee".to_string(), "0xbabe".to_string()),
                    ("selector".to_string(), "validate".to_string()),
                    ("calldata0".to_string(), "50".to_string()),
                ],
            ),
            (
                "view::0xbabe::balance_of".to_string(),
                vec![
                    ("caller".to_string(), "0x1".to_string()),
                    ("callee".to_string(), "0xbabe".to_string()),
                    ("selector".to_string(), "balance_of".to_string()),
                ],
            ),
        ]
    );
}

// --- component_test (StarkNet trace) --------------------------------------

/// Records `component_test_trace.json`.  Pins the M10 round-4
/// `#[starknet::component]` surface: a host contract embeds a reusable
/// component module via `component!(path: ownable_component, ...)`,
/// and every component-internal call surfaces with the component's
/// module path baked into the selector
/// (`ownable_component::transfer_ownership`).  The component's
/// storage reads / writes carry the component path in the storage
/// key (`ownable_component::owner`) so consumers can distinguish a
/// host-level slot from a component-embedded slot at the same
/// physical storage offset.
#[test]
fn test_component_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_component_test_via_ct_print_full") else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("component_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse component snforge trace");

    // Sanity-check the parsed entry shape: 1 contract_call (owner) +
    // 1 storage_read + 1 contract_call (transfer_ownership) +
    // 1 storage_write = 4 entries.
    assert_eq!(entries.len(), 4, "expected 4 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // Function table: each contract_call's selector carries the
    // component's module path so the function table surfaces the
    // component-prefixed names directly.  Storage read / write
    // entries are emitted as `<contract>::storage_read` /
    // `<contract>::storage_write` frames as usual — the
    // component-prefixing lives on the *key* arg (asserted below).
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec![
            "0xc0de::ownable_component::owner",
            "0xc0de::storage_read",
            "0xc0de::ownable_component::transfer_ownership",
            "0xc0de::storage_write",
        ]
    );

    let counts = &doc["counts"];
    // 4 entries → 4 call frames + 4 register_step (one per entry) +
    // implicit start() step at line 1 = 5 step events.  Each
    // storage_read / storage_write entry emits one io_event = 2
    // io_events total.
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(5), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(2),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 5 steps + 4 call_entry + 4 call_exit + 2 io_events = 15 events.
    assert_eq!(events.len(), 15, "events.len()");

    // ----- io_event sequence -----------------------------------------
    // The two storage entries surface as Read+Write tagged io_events
    // whose `text` field carries `<contract>:<key>=<value(s)>` —
    // pinning that the component-prefixed key flows through the
    // storage io_event content unchanged.
    let io_events: Vec<&serde_json::Value> = events.iter().filter(|e| e["kind"] == "io").collect();
    assert_eq!(io_events.len(), 2);
    let io_pairs: Vec<(String, String)> = io_events
        .iter()
        .map(|e| {
            (
                e["io_kind"].as_str().unwrap_or("").to_string(),
                e["text"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    assert_eq!(
        io_pairs,
        vec![
            (
                "ioFileOp".to_string(),
                "0xc0de:ownable_component::owner=0xa11ce".to_string(),
            ),
            (
                "ioStdout".to_string(),
                "0xc0de:ownable_component::owner=0xa11ce->0xb0b".to_string(),
            ),
        ]
    );

    // ----- Per-call selector arg pin ---------------------------------
    // Walk every call_entry and extract the `selector` / `key` arg
    // values so a regression that drops the component prefix from
    // either the selector or the storage key fails here loudly.  Only
    // the component-internal selectors / keys are exercised in this
    // fixture — both contract_calls and the storage entries carry
    // the `ownable_component::owner` shape.
    let key_or_selector: Vec<(String, String)> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            let fn_name = e["function"].as_str().expect("function str").to_string();
            // Each call_entry's args carries either a `selector` or a
            // `key` arg — the converter emits one or the other
            // depending on whether the source entry was a
            // contract_call or a storage_{read,write}.  Surface
            // whichever is present.
            let arg_value = e["args"]
                .as_array()
                .expect("call_entry.args array")
                .iter()
                .find(|a| {
                    let n = a["varname"].as_str();
                    n == Some("selector") || n == Some("key")
                })
                .map(|a| a["value"]["text"].as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            (fn_name, arg_value)
        })
        .collect();
    assert_eq!(
        key_or_selector,
        vec![
            (
                "0xc0de::ownable_component::owner".to_string(),
                "ownable_component::owner".to_string(),
            ),
            (
                "0xc0de::storage_read".to_string(),
                "ownable_component::owner".to_string(),
            ),
            (
                "0xc0de::ownable_component::transfer_ownership".to_string(),
                "ownable_component::transfer_ownership".to_string(),
            ),
            (
                "0xc0de::storage_write".to_string(),
                "ownable_component::owner".to_string(),
            ),
        ]
    );
}

// ===========================================================================
// M10 round 5 — five additional fixtures pinning closures, the
// `#[starknet::interface]` Dispatcher pattern, ECDSA signature checking,
// implicit-arg passing (gas / syscall_ptr / segment-arena), and Cairo's
// `#[test]` attribute.  Each fixture follows the same strict-`_via_ct_print_full`
// shape: every assertion uses `assert_eq!` against an exact recorded shape
// (counts, function tables, decoded JSON values).  No `contains`, no
// source-text `assert!`, no soft `>=` checks — a regression flips the
// exact tuple and fails the test loudly.
// ===========================================================================

// --- closure_test.cairo ---------------------------------------------------

/// Records `closure_test.cairo`.  Pins the M10 round-5 closure
/// surface: the inline closure bodies (`|x| x + 32`,
/// `|x| x + bias`) and the `core::ops::FnOnce::call` corelib
/// dispatch are inlined by the Sierra optimiser at the call sites,
/// so the function table contains only the surrounding driver
/// frames (`main`, `no_capture`, `with_capture`).  The closure-as-
/// distinct-frame surfacing is a downstream M11 extension; this
/// fixture pins the strict shape recorded today so a future
/// closure-frame decoder lands as a *new* test variable rather than
/// silently overwriting the current contract.
#[test]
fn test_closure_test_via_ct_print_full() {
    let Some((doc, source_path)) =
        record_and_dump_full("test_closure_test_via_ct_print_full", "closure_test.cairo")
    else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main: main → no_capture → with_capture.
    // The closure bodies and the corelib `Fn::call` dispatch are
    // inlined by the Sierra optimiser, so neither surfaces as its
    // own frame.
    assert_eq!(bare_fns, vec!["main", "no_capture", "with_capture"]);

    // Type table: only the shared felt252 carrier — closures don't
    // introduce a new typed surface today (the Fn-trait shape is
    // erased by the inliner).  No `type_0` writer-side alias because
    // the trace doesn't register any composite types.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252"]);

    let counts = &doc["counts"];
    // 12 step events: implicit start(1) + main body(3: 48, 39, 45) +
    // no_capture body(3: 49, 37, 38) + with_capture body(4: 50, 42,
    // 43, 44) + trailing return_value step(1).
    assert_eq!(counts["steps"].as_u64(), Some(12), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(3), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 12 steps + 3 call_entry + 3 call_exit = 18 events.
    assert_eq!(events.len(), 18, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "no_capture".to_string(),
            "with_capture".to_string(),
        ]
    );
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "no_capture".to_string(),
            "with_capture".to_string(),
            "main".to_string(),
        ]
    );

    // Per-binding kind sequence: today the recorder's source-level
    // heuristic does not see through the closure inliner to the
    // `let a = no_capture()` / `let bias = 32` / `let b =
    // with_capture()` bindings, so step-level vars are empty.  Pin
    // the empty kind list so a future closure-aware extension lands
    // as a new test variable rather than silently changing the
    // contract.
    assert_eq!(observed_var_kinds(&doc), Vec::<(String, String)>::new());

    // Per-callee call_exit return values: every closure-driver
    // returns Void today because the closure call is inlined and the
    // surrounding source-level let-binding heuristic doesn't recover
    // the typed return.  Surfacing the Int return is a downstream
    // M11 extension; pin the strict Void shape so the gap lands as
    // a new test variable.
    let exit_returns: Vec<(String, serde_json::Value)> = doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            (name, e["return_value"].clone())
        })
        .collect();
    assert_eq!(exit_returns.len(), 3);
    assert_eq!(exit_returns[0].0, "no_capture");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[1].0, "with_capture");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[2].0, "main");
    assert_eq!(exit_returns[2].1["kind"].as_str(), Some("Void"));
}

// --- interface_dispatcher_test (StarkNet trace) ---------------------------

/// Records `interface_dispatcher_test_trace.json`.  Pins the M10
/// round-5 `#[starknet::interface]` Dispatcher pattern: the host
/// contract's external entry points (`check_balance`,
/// `forward_transfer`) surface as their own call frames, and the
/// dispatcher-mediated cross-contract invocations of `IToken`'s
/// `balance_of` / `transfer` surface with the trait identity baked
/// into both the function-table name (`IToken::<callee>::<selector>`)
/// and a dedicated `dispatcher_trait` arg on the call entry.
#[test]
fn test_interface_dispatcher_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_interface_dispatcher_test_via_ct_print_full")
    else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("interface_dispatcher_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse interface_dispatcher snforge trace");

    // Sanity-check the parsed entry shape: 4 contract_call entries —
    // direct call into host, dispatcher-mediated balance_of, direct
    // call into host (forward_transfer), dispatcher-mediated
    // transfer.
    assert_eq!(entries.len(), 4, "expected 4 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // ----- Function table -------------------------------------------
    // Direct calls keep the legacy `<callee>::<selector>` form;
    // dispatcher-mediated calls carry the trait identity as the
    // first segment so consumers can group all dispatch-mediated
    // calls under the trait name without re-parsing.
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        functions,
        vec![
            "0xca11::check_balance",
            "IToken::0xt0ken::balance_of",
            "0xca11::forward_transfer",
            "IToken::0xt0ken::transfer",
        ]
    );

    // Type table: the shared felt252 carrier — no Reference type id
    // because the fixture exercises only the dispatcher_trait path
    // (no `self_kind` decorator).
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252"]);

    let counts = &doc["counts"];
    // 4 entries → 4 call frames + 4 register_step (one per entry) +
    // implicit start() step at line 1 = 5 step events.  No
    // storage_read / storage_write entries → 0 io_events.
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(5), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 5 steps + 4 call_entry + 4 call_exit = 13 events.
    assert_eq!(events.len(), 13, "events.len()");

    // ----- Per-call dispatcher_trait arg shape -----------------------
    // Walk every call_entry and surface the `dispatcher_trait` arg
    // text when present.  Direct calls (check_balance,
    // forward_transfer) MUST omit the arg; dispatcher-mediated calls
    // (balance_of, transfer) MUST surface it as the trait name.
    let dispatcher_args: Vec<(String, Option<String>)> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            let fn_name = e["function"].as_str().expect("function str").to_string();
            let dispatcher_trait = e["args"]
                .as_array()
                .expect("call_entry.args array")
                .iter()
                .find(|a| a["varname"].as_str() == Some("dispatcher_trait"))
                .map(|a| a["value"]["text"].as_str().unwrap_or("").to_string());
            (fn_name, dispatcher_trait)
        })
        .collect();
    assert_eq!(
        dispatcher_args,
        vec![
            ("0xca11::check_balance".to_string(), None),
            (
                "IToken::0xt0ken::balance_of".to_string(),
                Some("IToken".to_string()),
            ),
            ("0xca11::forward_transfer".to_string(), None),
            (
                "IToken::0xt0ken::transfer".to_string(),
                Some("IToken".to_string()),
            ),
        ]
    );

    // ----- Per-call canonical caller / callee / selector / calldata -
    // Pin the full args sequence per call so a regression in either
    // the field-ordering or the args plumbing fails here loudly.
    let arg_pairs: Vec<(String, Vec<(String, String)>)> = events
        .iter()
        .filter(|e| e["kind"] == "call_entry")
        .map(|e| {
            let fn_name = e["function"].as_str().expect("function str").to_string();
            let pairs: Vec<(String, String)> = e["args"]
                .as_array()
                .expect("call_entry.args array")
                .iter()
                .filter_map(|a| {
                    let name = a["varname"].as_str()?.to_string();
                    if a["value"]["kind"].as_str() == Some("String") {
                        Some((name, a["value"]["text"].as_str()?.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
            (fn_name, pairs)
        })
        .collect();
    assert_eq!(
        arg_pairs,
        vec![
            (
                "0xca11::check_balance".to_string(),
                vec![
                    ("caller".to_string(), "0x1".to_string()),
                    ("callee".to_string(), "0xca11".to_string()),
                    ("selector".to_string(), "check_balance".to_string()),
                    ("calldata0".to_string(), "0xa11ce".to_string()),
                ],
            ),
            (
                "IToken::0xt0ken::balance_of".to_string(),
                vec![
                    ("caller".to_string(), "0xca11".to_string()),
                    ("callee".to_string(), "0xt0ken".to_string()),
                    ("selector".to_string(), "balance_of".to_string()),
                    ("calldata0".to_string(), "0xa11ce".to_string()),
                    ("dispatcher_trait".to_string(), "IToken".to_string()),
                ],
            ),
            (
                "0xca11::forward_transfer".to_string(),
                vec![
                    ("caller".to_string(), "0x1".to_string()),
                    ("callee".to_string(), "0xca11".to_string()),
                    ("selector".to_string(), "forward_transfer".to_string()),
                    ("calldata0".to_string(), "0xb0b".to_string()),
                    ("calldata1".to_string(), "100".to_string()),
                ],
            ),
            (
                "IToken::0xt0ken::transfer".to_string(),
                vec![
                    ("caller".to_string(), "0xca11".to_string()),
                    ("callee".to_string(), "0xt0ken".to_string()),
                    ("selector".to_string(), "transfer".to_string()),
                    ("calldata0".to_string(), "0xb0b".to_string()),
                    ("calldata1".to_string(), "100".to_string()),
                    ("dispatcher_trait".to_string(), "IToken".to_string()),
                ],
            ),
        ]
    );
}

// --- ecdsa_test (StarkNet trace) ------------------------------------------

/// Records `ecdsa_test_trace.json`.  Pins the M10 round-5
/// `check_ecdsa_signature` syscall surface: the syscall surfaces as
/// a `<contract>::check_ecdsa_signature` Call/Return pair (deduped
/// in the function table when invoked twice), and the boolean
/// result decodes as a typed `ValueRecord::Bool` on the
/// `call_exit.return_value`.  The fixture exercises both `true` and
/// `false` returns so the round-trip through the JSON's
/// `"true"` / `"false"` strings is pinned.
#[test]
fn test_ecdsa_test_via_ct_print_full() {
    let Some(ct_print) = ct_print_or_skip("test_ecdsa_test_via_ct_print_full") else {
        return;
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("ecdsa_test_trace.json");
    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)
        .expect("parse ecdsa snforge trace");

    // Sanity-check the parsed entry shape: two syscall entries —
    // one returning true, one returning false.
    assert_eq!(entries.len(), 2, "expected 2 entries; got {entries:?}");

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let output = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full JSON");

    // ----- Function table — single deduped entry (both syscall
    // entries share the same name).
    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(functions, vec!["0xec5a::check_ecdsa_signature"]);

    // Type table: shared felt252 (str carrier) plus the dedicated
    // bool type id the converter registers for the
    // ValueRecord::Bool return.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "bool"]);

    let counts = &doc["counts"];
    // 2 syscalls → 2 call frames + 2 register_step (one per entry) +
    // implicit start() step at line 1 = 3 step events.  Syscalls
    // don't emit io_events.
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(counts["steps"].as_u64(), Some(3), "steps; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 3 steps + 2 call_entry + 2 call_exit = 7 events.
    assert_eq!(events.len(), 7, "events.len()");

    // ----- Per-syscall return-value shape ---------------------------
    // Strict pin: each call_exit carries the syscall's boolean
    // return value as a typed `ValueRecord::Bool`.  The first call
    // returns true (signature valid), the second returns false
    // (signature invalid).
    let exit_returns: Vec<(String, serde_json::Value)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            (
                e["function"].as_str().expect("function str").to_string(),
                e["return_value"].clone(),
            )
        })
        .collect();
    assert_eq!(exit_returns.len(), 2, "expected 2 call_exit events");
    assert_eq!(exit_returns[0].0, "0xec5a::check_ecdsa_signature");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Bool"));
    assert_eq!(exit_returns[0].1["b"].as_bool(), Some(true));
    assert_eq!(exit_returns[1].0, "0xec5a::check_ecdsa_signature");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Bool"));
    assert_eq!(exit_returns[1].1["b"].as_bool(), Some(false));
}

// --- implicits_test.cairo -------------------------------------------------

/// Records `implicits_test.cairo`.  Pins the M10 round-5 implicit-
/// arg surface: implicit arguments threaded through the Sierra ABI
/// (Pedersen for `pedersen()`, RangeCheck for u32 arithmetic,
/// SegmentArena / GasBuiltin for `Felt252Dict` allocation) do NOT
/// surface separately on the call_entry — the source-level recorder
/// doesn't reconstruct the implicit-vs-explicit distinction from
/// the user-visible function signature.  What the trace today
/// contains is the user-visible function frames (`main`, `compute`)
/// and the explicit `h` let-binding from the source's let-binding
/// shape; the synthetic implicit-arg surfacing is a downstream M11
/// extension.  This fixture pins the strict shape recorded today
/// so a future implicit-aware extension lands as a *new* test
/// variable rather than silently overwriting the current contract.
#[test]
fn test_implicits_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_implicits_test_via_ct_print_full",
        "implicits_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main: main → compute.  The `pedersen()`
    // / `Felt252Dict::insert` / `Felt252Dict::get` corelib dispatch
    // is inlined by the Sierra optimiser, so neither surfaces as
    // its own frame — and neither do the implicit-arg threads.
    assert_eq!(bare_fns, vec!["main", "compute"]);

    // Type table: shared felt252 carrier and its writer-side alias —
    // the implicit args (Pedersen / RangeCheck / SegmentArena /
    // GasBuiltin pointers) are not surfaced as their own typed
    // shapes today.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252", "type_0"]);

    let counts = &doc["counts"];
    // 10 step events: implicit start(1) + main body(2: 44, 45) +
    // compute body(6: 35, 36, 37, 38, 39, 40) + trailing
    // return_value step(1).
    assert_eq!(counts["steps"].as_u64(), Some(10), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(2), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 10 steps + 2 call_entry + 2 call_exit = 14 events.
    assert_eq!(events.len(), 14, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec!["main".to_string(), "compute".to_string()]
    );
    // Re-pinned against trace-format-nim eec665b: call_key is now
    // allocated at registerCall and completed CallRecords are flushed
    // from the buffer in entry-key order.  main and compute co-exit at
    // the same step, so call_exit events at that step now appear in
    // entry-key order (main before compute) rather than the previous
    // inverse-LIFO order.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec!["main".to_string(), "compute".to_string()]
    );

    // Per-binding kind sequence: only the `h` Pedersen binding (Int)
    // and the trailing `return_value` (Int) — the `widened` u32
    // binding and the `_v` discard binding don't surface because
    // the recorder's source-level heuristic drops `_`-prefixed
    // discards and the u32 widening is inlined by the optimiser.
    // Pin the kinds list explicitly so a future implicit-aware
    // extension lands as a new entry rather than silently changing
    // the contract.
    assert_eq!(
        observed_var_kinds(&doc),
        vec![
            ("h".to_string(), "Int".to_string()),
            ("return_value".to_string(), "Int".to_string()),
        ]
    );

    // Per-callee call_exit return values: both `compute` and `main`
    // return felt252 values that the source-level let-binding
    // heuristic recovers from `var_values`; today the pedersen
    // output isn't surfaced as a real felt (the VM-side felt
    // doesn't fit in i64 and decodes to 0 — the strict pin records
    // the shape the recorder emits today, not a hypothetical
    // big-int decoded value).
    let exit_returns: Vec<(String, serde_json::Value)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            (name, e["return_value"].clone())
        })
        .collect();
    // Re-pinned against trace-format-nim eec665b: the call_exit
    // sequence is now flushed in entry-key order at the top frame, so
    // main precedes compute (both co-exit at the same step).
    assert_eq!(exit_returns.len(), 2);
    assert_eq!(exit_returns[0].0, "main");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[0].1["i"].as_i64(), Some(0));
    assert_eq!(exit_returns[1].0, "compute");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Int"));
    assert_eq!(exit_returns[1].1["i"].as_i64(), Some(0));
}

// --- cairo_test_attribute_test.cairo --------------------------------------

/// Records `cairo_test_attribute_test.cairo`.  Pins the M10 round-5
/// Cairo `#[test]` attribute surface (documented gap): the recorder
/// does NOT link the cairo-test plugin, so source-level `#[test]`
/// attributes don't compile in the in-process Sierra runner.  The
/// fixture instead defines `add_test` / `sub_test` / `mul_test` as
/// regular `fn` and a `main` driver that invokes each in sequence —
/// the strict pin asserts every helper surfaces as its own
/// Function entry with balanced Call/Return events, exactly the
/// shape a future test-plugin integration would build on.
#[test]
fn test_cairo_test_attribute_test_via_ct_print_full() {
    let Some((doc, source_path)) = record_and_dump_full(
        "test_cairo_test_attribute_test_via_ct_print_full",
        "cairo_test_attribute_test.cairo",
    ) else {
        return;
    };

    assert_metadata_program_ends_with(&doc, &source_path);

    let functions: Vec<&str> = doc["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let bare_fns: Vec<&str> = functions
        .iter()
        .map(|f| f.rsplit("::").next().unwrap())
        .collect();
    // DFS visit order from main: main → add_test → sub_test →
    // mul_test.  Each `#[test]`-shaped helper surfaces as its own
    // function frame.
    assert_eq!(bare_fns, vec!["main", "add_test", "sub_test", "mul_test"]);

    // Type table: only the shared felt252 carrier — every helper
    // returns a felt252 and no composite types are constructed.
    let types: Vec<&str> = doc["types"]
        .as_array()
        .expect("types array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(types, vec!["felt252"]);

    let counts = &doc["counts"];
    // 18 step events: implicit start(1) + main body(4: 46, 31, 37,
    // 43) + add_test body(4: 47, 28, 29, 30) + sub_test body(4: 48,
    // 34, 35, 36) + mul_test body(4: 49, 40, 41, 42) + trailing
    // return_value step(1).
    assert_eq!(counts["steps"].as_u64(), Some(18), "steps; counts={counts}");
    assert_eq!(counts["calls"].as_u64(), Some(4), "calls; counts={counts}");
    assert_eq!(
        counts["io_events"].as_u64(),
        Some(0),
        "io_events; counts={counts}"
    );

    let events = doc["events"].as_array().expect("events array");
    // 18 steps + 4 call_entry + 4 call_exit = 26 events.
    assert_eq!(events.len(), 26, "events.len()");
    assert_step_indices_monotonic(&doc);

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "main".to_string(),
            "add_test".to_string(),
            "sub_test".to_string(),
            "mul_test".to_string(),
        ]
    );
    // LIFO close: each helper closes before the next opens (callees
    // don't nest), and main closes last after all three helpers.
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "add_test".to_string(),
            "sub_test".to_string(),
            "mul_test".to_string(),
            "main".to_string(),
        ]
    );

    // Per-binding kind sequence: today the recorder's source-level
    // heuristic does not surface the `let s = add_test()` /
    // `let d = sub_test()` / `let m = mul_test()` bindings as
    // step variables (the synthetic test helpers' return values
    // are not fed back into `var_values` because their return
    // expression `a + b` is not a let-binding the heuristic sees).
    // Pin the empty kind list so a future cairo-test-aware
    // extension lands as a new test variable rather than silently
    // changing the contract.
    assert_eq!(observed_var_kinds(&doc), Vec::<(String, String)>::new());

    // Per-callee call_exit return values: every helper returns Void
    // today because the recorder's source-level let-binding
    // heuristic doesn't recover the typed return for these helper
    // shapes.  Surfacing the Int return is a downstream M11
    // extension (mirrors the closure_test pattern); pin the strict
    // Void shape so the gap lands as a new test variable.
    let exit_returns: Vec<(String, serde_json::Value)> = events
        .iter()
        .filter(|e| e["kind"] == "call_exit")
        .map(|e| {
            let name = e["function"]
                .as_str()
                .expect("call_exit.function str")
                .rsplit("::")
                .next()
                .expect("non-empty function name")
                .to_string();
            (name, e["return_value"].clone())
        })
        .collect();
    assert_eq!(exit_returns.len(), 4);
    assert_eq!(exit_returns[0].0, "add_test");
    assert_eq!(exit_returns[0].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[1].0, "sub_test");
    assert_eq!(exit_returns[1].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[2].0, "mul_test");
    assert_eq!(exit_returns[2].1["kind"].as_str(), Some("Void"));
    assert_eq!(exit_returns[3].0, "main");
    assert_eq!(exit_returns[3].1["kind"].as_str(), Some("Void"));
}
