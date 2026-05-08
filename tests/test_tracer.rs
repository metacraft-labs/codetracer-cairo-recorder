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
