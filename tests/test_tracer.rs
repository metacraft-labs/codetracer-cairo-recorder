//! Integration tests for the Cairo tracer.
//!
//! These tests compile and run real Cairo programs through the full
//! compilation pipeline (Cairo -> Sierra -> CASM) and verify the
//! resulting CodeTracer trace output.
//!
//! Tests verify actual trace content with specific computed values,
//! not just file existence or non-emptiness.
//!
//! Legacy-format tests
//! -------------------
//!
//! Many tests in this file (`test_cairo_compile_and_run` through
//! `test_cairo_cli_record`, plus `test_starknet_codetracer_output` and
//! `test_cli_trace_starknet`) assert against the **legacy** 3-file
//! output shape (`trace.json` + `trace_metadata.json` + `trace_paths.json`).
//! After the M33 switch (commit b31d8d7) the recorder emits a single
//! multi-stream `<program>.ct` container — the legacy 3-file shape no
//! longer exists, so those tests are marked `#[ignore]` until they are
//! rewritten to use a CTFS reader.  This is a pre-existing breakage that
//! predates the 2026-05 CTFS audit.  See `AUDIT-CTFS-2026-05.md` ("Open
//! gaps") for the rewrite plan.  Pure-data conversion tests for the
//! snforge parser/converter (`test_parse_mock_snforge_trace`,
//! `test_starknet_*_captured`) and the new CTFS audit tests in
//! `test_ctfs_audit.rs` continue to run.

use std::path::{Path, PathBuf};

use codetracer_trace_writer_nim::TraceEventsFileFormat;

/// Helper: path to the test-programs directory.
fn test_programs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs/cairo")
}

/// Helper: run the tracer on a Cairo source file and return the output directory.
fn run_tracer_on_file(source_path: &Path, out_dir: &Path) {
    codetracer_cairo_recorder::recorder::record(source_path, out_dir, TraceEventsFileFormat::Json)
        .expect("trace_program should succeed");
}

/// Helper: parse the trace events JSON from the output directory.
fn load_trace_events(out_dir: &Path) -> Vec<serde_json::Value> {
    let events_path = out_dir.join("trace.json");
    let content = std::fs::read_to_string(&events_path).expect("failed to read trace events");
    let events: serde_json::Value =
        serde_json::from_str(&content).expect("trace events should be valid JSON");
    events
        .as_array()
        .expect("events should be an array")
        .clone()
}

/// Helper: parse trace_metadata.json from the output directory.
fn load_trace_metadata(out_dir: &Path) -> serde_json::Value {
    let metadata_path = out_dir.join("trace_metadata.json");
    let content =
        std::fs::read_to_string(&metadata_path).expect("failed to read trace_metadata.json");
    serde_json::from_str(&content).expect("trace_metadata.json should be valid JSON")
}

/// Helper: collect all Int values from Value events in the trace.
/// Returns a vec of (variable_id, i64_value) pairs.
fn collect_int_values(events: &[serde_json::Value]) -> Vec<(i64, i64)> {
    events
        .iter()
        .filter_map(|e| {
            let val = e.get("Value")?;
            let variable_id = val.get("variable_id")?.as_i64()?;
            let value = val.get("value")?;
            if value.get("kind").and_then(|k| k.as_str()) == Some("Int") {
                let i = value.get("i").and_then(|v| v.as_i64())?;
                Some((variable_id, i))
            } else {
                None
            }
        })
        .collect()
}

/// Helper: collect all VariableName events and return the names in order.
fn collect_variable_names(events: &[serde_json::Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| {
            e.get("VariableName")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Helper: find all Int values for a given variable name across the trace.
fn find_variable_values(events: &[serde_json::Value], var_name: &str) -> Vec<i64> {
    let var_names = collect_variable_names(events);
    let var_id = var_names.iter().position(|name| name == var_name);

    match var_id {
        Some(id) => {
            let int_values = collect_int_values(events);
            int_values
                .iter()
                .filter(|(vid, _)| *vid == id as i64)
                .map(|(_, v)| *v)
                .collect()
        }
        None => vec![],
    }
}

// ---------------------------------------------------------------------------
// Test 1: Compile and run flow_test.cairo, verify 3-file output
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_compile_and_run() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    // Verify the three output files exist and are non-empty.
    for filename in &["trace.json", "trace_metadata.json", "trace_paths.json"] {
        let path = out_dir.join(filename);
        assert!(path.exists(), "{} should exist", filename);
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 0, "{} should be non-empty", filename);
    }

    // trace.json should be valid JSON containing an array of events.
    let events = load_trace_events(&out_dir);
    assert!(!events.is_empty(), "trace should have at least one event");

    // There should be Step events (actual execution was recorded).
    let step_count = events.iter().filter(|e| e.get("Step").is_some()).count();
    assert!(
        step_count > 0,
        "trace should contain at least one Step event, got none"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Verify the trace contains felt252 value 94 (final_result)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_compute_value() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let events = load_trace_events(&out_dir);

    // Collect all integer values from the trace.
    let int_values = collect_int_values(&events);
    let all_values: Vec<i64> = int_values.iter().map(|(_, v)| *v).collect();

    // The compute function calculates: (10 + 32) * 2 + 10 = 94
    // This value should appear in the trace (as return_value or final_result).
    assert!(
        all_values.contains(&94),
        "trace should contain value 94 (the final result of (10+32)*2+10), got values: {:?}",
        {
            let mut unique: Vec<i64> = all_values.clone();
            unique.sort();
            unique.dedup();
            unique
        }
    );
}

// ---------------------------------------------------------------------------
// Test 3: Verify Step events are emitted at source lines
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_step_events() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let events = load_trace_events(&out_dir);

    // Count Step events.
    let step_events: Vec<&serde_json::Value> =
        events.iter().filter(|e| e.get("Step").is_some()).collect();

    assert!(
        step_events.len() >= 3,
        "should have at least 3 step events for flow_test.cairo, got {}",
        step_events.len()
    );

    // Verify step events have valid structure.
    for event in &step_events {
        let step = event.get("Step").unwrap();
        assert!(
            step.get("path_id").is_some(),
            "Step event should have path_id field"
        );
        let line = step["line"]
            .as_i64()
            .expect("Step line should be an integer");
        assert!(line > 0, "Step line should be positive, got {}", line);
        // Lines should be within the source file range (12 lines).
        assert!(
            line <= 20,
            "Step line should be within source file range, got {}",
            line
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: Verify variables a=10, b=32, sum_val=42 appear in trace
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_variable_values() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let events = load_trace_events(&out_dir);

    // Check that VariableName events exist.
    let var_names = collect_variable_names(&events);
    assert!(
        !var_names.is_empty(),
        "trace should contain VariableName events"
    );

    // Verify specific variable names appear.
    assert!(
        var_names.contains(&"a".to_string()),
        "variable 'a' should appear in trace, got names: {:?}",
        var_names
    );
    assert!(
        var_names.contains(&"b".to_string()),
        "variable 'b' should appear in trace, got names: {:?}",
        var_names
    );
    assert!(
        var_names.contains(&"sum_val".to_string()),
        "variable 'sum_val' should appear in trace, got names: {:?}",
        var_names
    );

    // Verify variable values.
    let a_values = find_variable_values(&events, "a");
    assert!(
        a_values.contains(&10),
        "variable 'a' should have value 10, got: {:?}",
        a_values
    );

    let b_values = find_variable_values(&events, "b");
    assert!(
        b_values.contains(&32),
        "variable 'b' should have value 32, got: {:?}",
        b_values
    );

    let sum_values = find_variable_values(&events, "sum_val");
    assert!(
        sum_values.contains(&42),
        "variable 'sum_val' should have value 42, got: {:?}",
        sum_values
    );

    // Also check doubled=84 and final_result=94.
    let doubled_values = find_variable_values(&events, "doubled");
    assert!(
        doubled_values.contains(&84),
        "variable 'doubled' should have value 84, got: {:?}",
        doubled_values
    );

    let final_values = find_variable_values(&events, "final_result");
    assert!(
        final_values.contains(&94),
        "variable 'final_result' should have value 94, got: {:?}",
        final_values
    );
}

// ---------------------------------------------------------------------------
// Test 5: Verify metadata JSON has required fields
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_metadata_structure() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let metadata = load_trace_metadata(&out_dir);

    // TraceMetadata must contain "program", "args", and "workdir" fields.
    assert!(
        metadata.get("program").is_some(),
        "metadata should have 'program' field, got: {}",
        metadata
    );
    assert!(
        metadata["program"].is_string(),
        "metadata 'program' should be a string"
    );
    let program_str = metadata["program"].as_str().unwrap();
    assert!(
        program_str.contains("flow_test.cairo"),
        "metadata 'program' should reference the cairo source file, got: {}",
        program_str
    );

    assert!(
        metadata.get("args").is_some(),
        "metadata should have 'args' field, got: {}",
        metadata
    );
    assert!(
        metadata["args"].is_array(),
        "metadata 'args' should be an array"
    );

    assert!(
        metadata.get("workdir").is_some(),
        "metadata should have 'workdir' field, got: {}",
        metadata
    );
    assert!(
        metadata["workdir"].is_string(),
        "metadata 'workdir' should be a string"
    );
}

// ---------------------------------------------------------------------------
// Test 6: trace_paths.json content validation
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_tracer_paths_valid() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let paths_content =
        std::fs::read_to_string(out_dir.join("trace_paths.json")).expect("failed to read paths");
    let paths: serde_json::Value =
        serde_json::from_str(&paths_content).expect("trace_paths.json should be valid JSON");
    assert!(paths.is_array(), "trace_paths.json should be a JSON array");
    let paths_arr = paths.as_array().unwrap();
    assert!(
        !paths_arr.is_empty(),
        "trace_paths.json should have at least one path entry"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Function call/return events
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_function_entry_exit() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let events = load_trace_events(&out_dir);

    // There should be Call events (function entries were recorded).
    let call_count = events.iter().filter(|e| e.get("Call").is_some()).count();
    assert!(
        call_count > 0,
        "trace should contain at least one Call event"
    );

    // There should be Return events.
    let return_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e.get("Return").is_some())
        .collect();
    assert!(
        !return_events.is_empty(),
        "trace should contain at least one Return event"
    );

    // The last Return event should be after the last Step.
    let last_return_idx = events
        .iter()
        .rposition(|e| e.get("Return").is_some())
        .expect("should have a Return event");

    let steps_after_return = events[last_return_idx + 1..]
        .iter()
        .filter(|e| e.get("Step").is_some())
        .count();
    assert_eq!(
        steps_after_return, 0,
        "no Step events should appear after the final Return"
    );
}

// ---------------------------------------------------------------------------
// Test 8: All intermediate values appear in trace
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_all_intermediate_values() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = test_programs_dir().join("flow_test.cairo");
    run_tracer_on_file(&source_path, &out_dir);

    let events = load_trace_events(&out_dir);

    // Collect all Int values across all Value events.
    let int_values = collect_int_values(&events);
    let all_values: Vec<i64> = int_values.iter().map(|(_, v)| *v).collect();

    // The compute function produces these key values:
    // a = 10, b = 32, sum_val = 42, doubled = 84, final_result = 94
    for expected in &[10i64, 32, 42, 84, 94] {
        assert!(
            all_values.contains(expected),
            "trace should contain value {} from compute function, got values: {:?}",
            expected,
            {
                let mut unique: Vec<i64> = all_values.clone();
                unique.sort();
                unique.dedup();
                unique
            }
        );
    }
}

// ---------------------------------------------------------------------------
// Test 9: CLI record with flow_test.cairo
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cairo_cli_record() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("cli-traces");
    let source_path = test_programs_dir().join("flow_test.cairo");

    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--",
            "record",
            source_path.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run");

    assert!(
        output.status.success(),
        "record should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify output files exist.
    assert!(out_dir.join("trace.json").exists());
    assert!(out_dir.join("trace_metadata.json").exists());
    assert!(out_dir.join("trace_paths.json").exists());

    // Verify the CLI-produced trace has actual content.
    let events = load_trace_events(&out_dir);
    assert!(!events.is_empty(), "CLI trace should have events");

    let step_count = events.iter().filter(|e| e.get("Step").is_some()).count();
    assert!(step_count > 0, "CLI trace should contain Step events");
}

// ===========================================================================
// StarkNet trace tests
// ===========================================================================

/// Helper: path to the starknet test-programs directory.
fn starknet_test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs/starknet")
}

// ---------------------------------------------------------------------------
// Test 10: Parse mock snforge trace file
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test 11: Verify contract calls are captured in conversion
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test 12: Verify storage reads/writes are captured
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test 13: Verify events are captured
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test 14: Verify conversion to CodeTracer format produces valid output
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_starknet_codetracer_output() {
    use codetracer_cairo_recorder::starknet::{parse_snforge_trace, write_starknet_trace};

    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("starknet-traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = starknet_test_dir().join("mock_trace.json");
    let entries = parse_snforge_trace(&trace_path).unwrap();

    write_starknet_trace(
        &trace_path,
        &entries,
        &out_dir,
        TraceEventsFileFormat::Json,
    )
    .expect("write_starknet_trace should succeed");

    // Verify the three output files exist and are non-empty.
    for filename in &["trace.json", "trace_metadata.json", "trace_paths.json"] {
        let path = out_dir.join(filename);
        assert!(path.exists(), "{} should exist", filename);
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 0, "{} should be non-empty", filename);
    }

    // Verify trace.json is valid JSON with events.
    let events = load_trace_events(&out_dir);
    assert!(!events.is_empty(), "starknet trace should have events");

    let step_count = events.iter().filter(|e| e.get("Step").is_some()).count();
    assert!(
        step_count >= 6,
        "starknet trace should have at least 6 Step events (one per entry), got {}",
        step_count
    );

    let call_count = events.iter().filter(|e| e.get("Call").is_some()).count();
    assert!(
        call_count >= 6,
        "starknet trace should have at least 6 Call events, got {}",
        call_count
    );
}

// ---------------------------------------------------------------------------
// Test 15: CLI trace-starknet subcommand
// ---------------------------------------------------------------------------

#[test]
#[ignore = "legacy 3-file output (pre-M33); rewrite for .ct container — see module doc-comment"]
fn test_cli_trace_starknet() {
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let out_dir = tmp_dir.path().join("cli-starknet-traces");
    let trace_path = starknet_test_dir().join("mock_trace.json");

    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--",
            "trace-starknet",
            trace_path.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run");

    assert!(
        output.status.success(),
        "trace-starknet should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(out_dir.join("trace.json").exists());
    assert!(out_dir.join("trace_metadata.json").exists());
    assert!(out_dir.join("trace_paths.json").exists());

    let events = load_trace_events(&out_dir);
    assert!(!events.is_empty(), "CLI starknet trace should have events");
}
