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
        .join("ct-print")
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

    // ----- Call sequence: compute first, then main --------------------
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
        call_sequence[0].ends_with("::compute"),
        "expected first call to be `compute`; got {:?}",
        call_sequence
    );
    assert!(
        call_sequence[1].ends_with("::main"),
        "expected second call to be `main`; got {:?}",
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
            observed_vars
                .iter()
                .any(|(n, v)| n == name && v == value),
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
// Where the recorder's current behaviour deviates from what the
// language semantics dictate (e.g. `call_entry` events are emitted in
// **lexical** source order rather than dynamic execution order, each
// callee is registered exactly once even when invoked multiple times,
// `call_exit.return_value` is always `Void`, and collection literals
// always decode as `Int`), the deviation is documented inline as
// `RECORDER BUG: ...` and a parallel `#[ignore]`d sibling test
// captures the spec-correct expectation so it surfaces the moment the
// recorder catches up.

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
fn record_and_dump_full(
    test_name: &str,
    program: &str,
) -> Option<(serde_json::Value, PathBuf)> {
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

    let doc: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("ct-print --full should emit valid JSON");

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
            let i = value["i"].as_i64().unwrap_or_else(|| {
                panic!("Int.i must be i64 for `{name}`; got {value}")
            });
            out.push((name, i));
        }
    }
    out
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
fn assert_metadata_program_ends_with(
    doc: &serde_json::Value,
    source_path: &std::path::Path,
) {
    let prog = doc["metadata"]["program"]
        .as_str()
        .expect("metadata.program str");
    let want = source_path.file_name().unwrap().to_string_lossy();
    assert!(
        prog.ends_with(&*want),
        "metadata.program {prog} must end with {want}"
    );
}

/// Assert that every `call_exit` event carries a `Void` return_value.
///
/// RECORDER BUG: the recorder always passes `NONE_VALUE` to
/// `register_return`, so genuine return values never reach the trace.
/// A spec-compliant recorder would surface the actual function result
/// (Int / Sequence / Struct / etc.) and the `#[ignore]`d sibling tests
/// below capture that expectation.
fn assert_all_call_exits_return_void(doc: &serde_json::Value) {
    for ev in doc["events"].as_array().expect("events array") {
        if ev["kind"] != "call_exit" {
            continue;
        }
        let rv = &ev["return_value"];
        assert_eq!(
            rv["kind"].as_str(),
            Some("Void"),
            "call_exit.return_value must be Void today; got {rv} \
             — if real return values have landed, update the per-program \
             test to assert on them explicitly rather than weakening this \
             check"
        );
    }
}

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

    // ----- Function table — order is writer-assignment (lexical) order
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
        vec!["classify", "loop_sum", "match_pick", "compute", "main"]
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

    // ----- Call sequence ---------------------------------------------
    // RECORDER BUG: the spec wants dynamic call order
    // (main → compute → classify → loop_sum → match_pick).  Today the
    // recorder walks the source linearly and emits one call_entry per
    // function definition in lexical order.  See
    // test_control_flow_test_call_sequence_dynamic_order below.
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "classify".to_string(),
            "loop_sum".to_string(),
            "match_pick".to_string(),
            "compute".to_string(),
            "main".to_string(),
        ]
    );

    // ----- Call-exit order: today identical to entry order (LIFO would
    // be the spec, but each function is "exited" the moment the parser
    // sees the next `fn` keyword) -----------------------------------
    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "classify".to_string(),
            "loop_sum".to_string(),
            "match_pick".to_string(),
            "compute".to_string(),
            "main".to_string(),
        ]
    );

    assert_all_call_exits_return_void(&doc);

    // ----- Decoded variable values -----------------------------------
    // Only `compute()`'s let-bindings + the synthetic trailing
    // `return_value` carry values today (they're the only names that
    // appear in the longest tuple-return slot).
    assert_eq!(
        observed_var_sequence(&doc),
        vec![
            ("raw".to_string(), 2),
            ("sign".to_string(), 20),
            ("loop_total".to_string(), 3),
            ("picked".to_string(), 100),
            ("combined".to_string(), 123),
            ("return_value".to_string(), 123),
        ]
    );
}

#[test]
#[ignore = "RECORDER BUG: call_entry events are emitted in lexical \
            source order, not dynamic execution order.  Spec-compliant \
            output for this program should be \
            [main, compute, classify, loop_sum, match_pick]."]
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
    assert_eq!(
        bare_fns,
        vec!["inner", "middle", "outer", "compute", "main"]
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

    // ----- Call sequence: lexical order (RECORDER BUG — see sibling
    // #[ignore] test below).
    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
            "compute".to_string(),
            "main".to_string(),
        ],
        "call_entry events appear in lexical source order today \
         (RECORDER BUG: should be dynamic call order)"
    );

    assert_eq!(
        observed_exit_sequence(&doc),
        vec![
            "inner".to_string(),
            "middle".to_string(),
            "outer".to_string(),
            "compute".to_string(),
            "main".to_string(),
        ]
    );

    assert_all_call_exits_return_void(&doc);

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

#[test]
#[ignore = "RECORDER BUG: nested calls should produce LIFO call_exit \
            ordering (inner before middle before outer).  Today every \
            user function is closed in lexical order, so depth-aware \
            consumers cannot reconstruct the call tree."]
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
            "compute".to_string(),
            "main".to_string(),
        ]
    );
}

#[test]
#[ignore = "RECORDER BUG: call_exit.return_value is always Void.  A \
            spec-compliant recorder would surface inner=3, middle=11, \
            outer=111, compute=111, main=111 on the call_exit events \
            so callers can replay the call tree without re-deriving \
            the values from step-binding heuristics."]
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

/// Records `collections_test.cairo`.  RECORDER BUG: collection
/// values (Array / Tuple) are not surfaced as ValueRecord::Sequence
/// or ValueRecord::Tuple — only the **scalar** let-bindings inside
/// `compute()`'s tuple-return slot end up with decoded `Int` values.
/// The arrays and tuples themselves are completely opaque.
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
    assert_eq!(
        bare_fns,
        vec!["array_total", "pair_sum", "compute", "main"]
    );

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

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "array_total".to_string(),
            "pair_sum".to_string(),
            "compute".to_string(),
            "main".to_string(),
        ]
    );

    assert_all_call_exits_return_void(&doc);

    // ----- Decoded values --------------------------------------------
    // RECORDER BUG: a spec-compliant trace would expose the Array
    // `arr` as a ValueRecord::Sequence (or List) with elements
    // [1,2,3,4], and the tuple `(10, 20)` as ValueRecord::Tuple.
    // Today the recorder only surfaces the scalar let-bindings that
    // live inside `compute()`'s tuple-return slot.
    //
    //   array_total() returns 4 (the length of [1,2,3,4])
    //   pair_sum()    returns 30 (10 + 20)
    //   final_sum     = 4 + 30 = 34
    assert_eq!(
        observed_var_sequence(&doc),
        vec![
            ("arr_total".to_string(), 4),
            ("pair_total".to_string(), 30),
            ("final_sum".to_string(), 34),
            ("return_value".to_string(), 34),
        ]
    );
}

#[test]
#[ignore = "RECORDER BUG: arrays, tuples and structs are not encoded \
            as ValueRecord::Sequence / Tuple / Struct.  Spec-compliant \
            output should expose at minimum the Sequence variant for \
            `arr` (= [1,2,3,4]) and the Tuple variant for `pair` \
            (= (10, 20)) — the recorder currently only writes Int."]
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
/// RECORDER BUG (documented inline): the panic still leaves the
/// `compute()` let-bindings populated from the **panic vector** (not
/// the source-evaluated values), so `a` and `b` decode as 0 — the
/// first two entries of the panic payload.  A spec-compliant recorder
/// would either leave the let-bindings absent on a panic or carry the
/// real source-level values.
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
    assert_eq!(bare_fns, vec!["divide", "compute", "main"]);

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

    assert_eq!(
        observed_call_sequence(&doc),
        vec![
            "divide".to_string(),
            "compute".to_string(),
            "main".to_string(),
        ]
    );

    assert_all_call_exits_return_void(&doc);

    // ----- The panic event -------------------------------------------
    let io_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["kind"] == "io")
        .collect();
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
    // RECORDER BUG: `a` and `b` are populated from the panic-payload
    // vector (the first two felts of the Cairo panic encoding), not
    // from the source-level values (`a = 10`, `b = divide(a, 0)`).
    // The first panic felt is too large to fit in i64 so it falls
    // back to 0, and the second is the divisor (0).  See sibling
    // `#[ignore]` test below.
    assert_eq!(
        observed_var_sequence(&doc),
        vec![
            ("a".to_string(), 0),
            ("b".to_string(), 0),
            ("return_value".to_string(), 16),
        ]
    );
}

#[test]
#[ignore = "RECORDER BUG: when a Cairo program panics, source-level \
            let-bindings should keep the values they actually held at \
            the panic point (`a = 10`).  The recorder currently maps \
            the panic-payload vector onto the let-binding names by \
            index, so `a` decodes as the first felt of the panic \
            encoding (0 after i64 overflow)."]
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
