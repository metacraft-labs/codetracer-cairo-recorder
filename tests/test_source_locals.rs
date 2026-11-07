//! Source-level local-variable fidelity tests for the Cairo recorder.
//!
//! Each test records a real Cairo program through the full
//! Cairo -> Sierra -> CASM -> VM pipeline (no mocks), decodes the
//! resulting CTFS container with `ct-print --full`, and asserts on the
//! facts a source-level debugger needs:
//!
//! * steps are attributed to the real source file and to the source
//!   lines of the executed statements;
//! * every named `let` binding surfaces as a local on its declaration
//!   line, carrying the value the program actually computed;
//! * the steps run inside a `main` function frame.
//!
//! The programs are chosen so that their locals are not recoverable
//! from the program's return value alone: in `simple_trivial_chain_test`
//! the compiler collapses the copy chain `a -> b -> c` into a single
//! runtime value, and in `computed_locals_test` every local is the
//! result of a runtime computation that is never returned on its own.

use std::path::{Path, PathBuf};
use std::process::Command;

fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX))
}

fn cairo_test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-programs/cairo")
}

/// Record `program` and return the `ct-print --full` JSON document
/// (paths are kept so the step paths can be checked against the real
/// source file) and the absolute source path.
fn record_full(program: &str) -> (serde_json::Value, PathBuf) {
    let ct_print = ct_print_path();
    assert!(
        ct_print.exists(),
        "ct-print binary required at {} — build it via the \
         codetracer-trace-format-nim sibling recipe",
        ct_print.display()
    );

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = cairo_test_dir()
        .join(program)
        .canonicalize()
        .expect("fixture exists");
    codetracer_cairo_recorder::recorder::record(&source_path, &out_dir)
        .expect("recorder::record should succeed");

    let ct_file = std::fs::read_dir(&out_dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .unwrap_or_else(|| panic!("expected a .ct container in {}", out_dir.display()));

    let output = Command::new(&ct_print)
        .arg("--full")
        .arg(&ct_file)
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        output.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ct-print --full should emit valid JSON");
    (doc, source_path)
}

fn steps(doc: &serde_json::Value) -> Vec<&serde_json::Value> {
    doc["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["kind"] == "step")
        .collect()
}

/// `(name, line, value)` for every scalar local attached to a step.
fn locals(doc: &serde_json::Value) -> Vec<(String, i64, i64)> {
    let mut out = Vec::new();
    for step in steps(doc) {
        let line = step["line"].as_i64().expect("step line");
        for v in step["vars"].as_array().into_iter().flatten() {
            let name = v["varname"].as_str().expect("varname").to_string();
            let value = &v["value"];
            assert_eq!(
                value["kind"].as_str(),
                Some("Int"),
                "local `{name}` should be an Int, got {value}"
            );
            out.push((name, line, value["i"].as_i64().expect("Int.i")));
        }
    }
    out
}

fn assert_steps_in_source(doc: &serde_json::Value, source_path: &Path, lines: &[i64]) {
    let want_path = source_path.to_string_lossy();
    let steps = steps(doc);
    for step in &steps {
        let path = step["path"].as_str().expect("step path");
        assert_eq!(
            path, want_path,
            "every step must be attributed to the recorded source file"
        );
    }
    let step_lines: Vec<i64> = steps.iter().map(|s| s["line"].as_i64().unwrap()).collect();
    for line in lines {
        assert!(
            step_lines.contains(line),
            "expected a step on source line {line}; step lines were {step_lines:?}"
        );
    }
}

fn assert_main_frame(doc: &serde_json::Value, body_lines: &[i64]) {
    let events = doc["events"].as_array().expect("events array");
    let main_call = events
        .iter()
        .find(|e| {
            e["kind"] == "call_entry"
                && e["function"]
                    .as_str()
                    .is_some_and(|f| f == "main" || f.ends_with("::main"))
        })
        .unwrap_or_else(|| panic!("expected a `main` call frame; events: {events:#?}"));
    let main_fn_id = &main_call["function_id"];
    for step in steps(doc) {
        let line = step["line"].as_i64().unwrap();
        if body_lines.contains(&line) {
            assert_eq!(
                &step["function_id"], main_fn_id,
                "step on body line {line} must run inside the `main` frame"
            );
        }
    }
}

fn assert_local(locals: &[(String, i64, i64)], name: &str, line: i64, value: i64) {
    assert!(
        locals
            .iter()
            .any(|(n, l, v)| n == name && *l == line && *v == value),
        "expected local `{name}` = {value} on line {line}; recorded locals: {locals:?}"
    );
}

#[test]
fn trivial_copy_chain_records_every_local() {
    let (doc, source_path) = record_full("simple_trivial_chain_test.cairo");

    assert_steps_in_source(&doc, &source_path, &[11, 12, 13, 14]);
    assert_main_frame(&doc, &[11, 12, 13, 14]);

    let locals = locals(&doc);
    assert_local(&locals, "a", 11, 10);
    assert_local(&locals, "b", 12, 10);
    assert_local(&locals, "c", 13, 10);
}

#[test]
fn computed_locals_carry_their_runtime_values() {
    let (doc, source_path) = record_full("computed_locals_test.cairo");

    assert_steps_in_source(&doc, &source_path, &[6, 7, 8, 9, 10]);
    assert_main_frame(&doc, &[6, 7, 8, 9, 10]);

    let locals = locals(&doc);
    assert_local(&locals, "a", 6, 6);
    assert_local(&locals, "b", 7, 42);
    assert_local(&locals, "c", 8, 43);
    assert_local(&locals, "d", 9, 43);
}
