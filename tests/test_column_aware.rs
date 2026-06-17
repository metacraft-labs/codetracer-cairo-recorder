//! Column-aware replay-navigation regression test for the Cairo recorder
//! (FU-Column-Aware-Nav-Cairo).
//!
//! Mirrors the JS recorder's
//! `tests/integration/column-aware.test.ts` "multiple statements on
//! one line each record distinct columns" fixture, and matches the
//! EVM (`tests/test_column_aware.rs`) and Solana
//! (`tests/test_column_aware_steps.rs`) sibling tests.  The Cairo
//! fixture packs three `let` declarations onto a single source line
//! so each statement starts at a distinct column.  The recorder must:
//!
//!   * Set `meta.dat` bit 4 (`FLAG_HAS_COLUMN_AWARE_STEPS`).  Verified
//!     through `ct-print --full`'s `metadata.flags.has_column_aware_steps`.
//!   * Surface a step event for each of the three statements with a
//!     strictly distinct column value.
//!
//! See `codetracer-specs/Planned-Features/
//! Column-Aware-Navigation-Other-Languages.plan.md` for the acceptance
//! criteria.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the `ct-print` binary shipped with `codetracer-trace-format-nim`.
fn ct_print_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("codetracer-trace-format-nim")
        .join(format!("ct-print{}", std::env::consts::EXE_SUFFIX))
}

fn ct_files_in(out_dir: &Path) -> Vec<PathBuf> {
    if !out_dir.exists() {
        return Vec::new();
    }
    std::fs::read_dir(out_dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect()
}

/// Returns the path to ct-print or logs a `SKIP:` diagnostic and
/// returns `None`.  Mirrors the convention enforced by
/// `verify-cli-convention-no-silent-skip.sh`.
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

#[test]
fn test_column_aware_distinct_columns_on_one_line() {
    let test_name = "test_column_aware_distinct_columns_on_one_line";
    let Some(ct_print) = ct_print_or_skip(test_name) else {
        return;
    };

    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs")
        .join("cairo")
        .join("column_aware_test.cairo");
    assert!(source.exists(), "fixture missing: {}", source.display());

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    codetracer_cairo_recorder::recorder::record(&source, &out_dir)
        .expect("recorder::record should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {out_dir:?}"
    );

    let dump = Command::new(&ct_print)
        .args(["--full", "--strip-paths"])
        .arg(&ct_files[0])
        .output()
        .expect("failed to run ct-print --full");
    assert!(
        dump.status.success(),
        "ct-print --full should succeed; stderr: {}",
        String::from_utf8_lossy(&dump.stderr),
    );

    let doc: serde_json::Value =
        serde_json::from_slice(&dump.stdout).expect("ct-print --full should emit valid JSON");

    // --- meta.dat bit 4: FLAG_HAS_COLUMN_AWARE_STEPS ---
    // The trace metadata must advertise column-aware support so
    // downstream tooling knows to surface columns to the user.  Mirrors
    // the JS reference assertion at
    // `codetracer-js-recorder/tests/integration/column-aware.test.ts`.
    let has_column_aware = doc["metadata"]["flags"]["has_column_aware_steps"].as_bool();
    assert_eq!(
        has_column_aware,
        Some(true),
        "trace metadata must advertise has_column_aware_steps=true; got {:?}",
        doc["metadata"]
    );

    // --- gather step events per line ---
    //
    // The fixture's body line (line 15) is:
    //
    //   "    let a: felt252 = 1; let b: felt252 = 2; let c: felt252 = 3;"
    //
    // Three top-level statements, each starting at a distinct 1-based
    // byte column inside the line (5, 25, 45).  The recorder's
    // statement-column splitter (`statement_columns_on_line` in
    // `src/tracer.rs`) drives the emission.
    let events = doc["events"].as_array().expect("events array");
    let mut cols_by_line: std::collections::BTreeMap<i64, std::collections::BTreeSet<i64>> =
        std::collections::BTreeMap::new();
    for ev in events {
        if ev["kind"] != "step" {
            continue;
        }
        let Some(line) = ev["line"].as_i64() else {
            continue;
        };
        let Some(col) = ev["column"].as_i64() else {
            continue;
        };
        cols_by_line.entry(line).or_default().insert(col);
    }

    // Three statements on a single line MUST surface as three (or more)
    // distinct columns.  We pick the maximum-cardinality line so the
    // assertion is robust to comment-header drift above the function
    // definition — the multi-statement line is the *only* line in the
    // fixture that yields three or more distinct columns.
    let (line, distinct_cols) = cols_by_line
        .iter()
        .max_by_key(|(_, cols)| cols.len())
        .map(|(line, cols)| (*line, cols.clone()))
        .expect("trace should contain at least one step event with a column field");
    assert!(
        distinct_cols.len() >= 3,
        "expected the three-statement line to surface >= 3 distinct step columns; \
         got line {line} -> {distinct_cols:?}; full line->cols map: {cols_by_line:?}",
    );

    // Every surfaced column is >= 1 (1-based on the wire).
    for col in &distinct_cols {
        assert!(
            *col >= 1,
            "step column must be >= 1 (1-based on the wire); got {col} on line {line}",
        );
    }

    eprintln!(
        "PASS: column-aware step emission — line {line} surfaces distinct columns {distinct_cols:?}"
    );
}
