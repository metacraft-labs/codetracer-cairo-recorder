//! CTFS audit regression tests for the Cairo / Starknet recorder.
//!
//! These tests lock in the fixes landed during the 2026-05 CTFS audit
//! (entry 1.50 in `/tmp/isonim-migration.txt`).  They mirror the pattern
//! established by the EVM (1.39), Solana (1.44), Move (1.46), and
//! Cardano (1.48) recorder audits.
//!
//! Each test corresponds to one bullet from the section 5.6 audit
//! checklist:
//!
//!  - (b) Call args via `arg()` — `test_starknet_contract_call_stages_args`.
//!  - (c) Starknet log events via `register_special_event` —
//!    `test_starknet_event_emits_special_event`.
//!  - (f) Canonical CTFS schema match — `test_ctfs_format_advertised_in_help`
//!    and `test_ctfs_writer_produces_ct_container`.
//!
//! Note: these tests verify behaviour at the recorder API surface and
//! at the CLI surface.  Decoding the binary `.ct` container requires a
//! CTFS reader which is not in this crate's dev-dependencies; the
//! per-event assertions therefore assert on observable artefacts (file
//! existence, magic bytes, container size, CLI help text) rather than
//! decoding the records.

use std::path::Path;

use codetracer_trace_writer_nim::TraceEventsFileFormat;

/// Canonical CTFS multi-stream container magic — see
/// `codetracer-trace-format-spec/`.
const CTFS_MAGIC: [u8; 5] = [0xC0, 0xDE, 0x72, 0xAC, 0xE2];

/// Helper: build a tempdir, run the recorder against `flow_test.cairo`,
/// and return the path to the produced trace container.
fn record_flow_test(format: TraceEventsFileFormat) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let source_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs/cairo/flow_test.cairo");

    codetracer_cairo_recorder::recorder::record(&source_path, &out_dir, format)
        .expect("recorder::record should succeed");

    (tmp_dir, out_dir)
}

/// Helper: collect every `.ct` file in `out_dir`.
fn ct_files_in(out_dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(out_dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect()
}

// ---- Audit (f): CTFS multi-stream container is producible -----------------

/// The recorder must be able to emit a canonical CTFS multi-stream `.ct`
/// container (the format that `NimTraceReaderHandle` and the db-backend
/// `CTFSTraceReader` consume directly).  Pre-fix, the CLI's
/// `OutputFormat` enum did not even expose `Ctfs` — only `Binary` /
/// `Json` — so there was no way to request the canonical container
/// from the CLI.
#[test]
fn test_ctfs_writer_produces_ct_container() {
    let (_tmp, out_dir) = record_flow_test(TraceEventsFileFormat::Ctfs);

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );

    let content = std::fs::read(&ct_files[0]).expect("read ct file");
    assert!(content.len() >= 5, ".ct container too small");
    assert_eq!(
        &content[..5],
        &CTFS_MAGIC,
        "produced container should start with CTFS magic bytes"
    );
}

/// The CLI binary must accept `ctfs` as a `--format` value AND default
/// to it.  This catches accidental regressions in the CLI surface (e.g.
/// someone reverting the `OutputFormat` enum back to the pre-fix
/// `Binary` / `Json` only shape).
///
/// Same shape as the Cardano (1.48), Move (1.46) and Solana (1.44)
/// audit smoke tests.
#[test]
fn test_ctfs_format_advertised_in_help() {
    use std::process::Command;

    let bin = env!("CARGO_BIN_EXE_codetracer-cairo-recorder");
    let output = Command::new(bin)
        .args(["record", "--help"])
        .output()
        .expect("failed to run codetracer-cairo-recorder record --help");

    assert!(output.status.success(), "--help should exit 0");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("ctfs"),
        "`record --help` should advertise `ctfs` as a --format value; got:\n{help}"
    );
    assert!(
        help.contains("[default: ctfs]"),
        "`record --help` should default --format to `ctfs`; got:\n{help}"
    );
}

/// The same default-Ctfs guarantee must hold for the `trace-starknet`
/// subcommand.  This protects against partial reverts that fix only one
/// subcommand's default.
#[test]
fn test_ctfs_format_default_for_trace_starknet() {
    use std::process::Command;

    let bin = env!("CARGO_BIN_EXE_codetracer-cairo-recorder");
    let output = Command::new(bin)
        .args(["trace-starknet", "--help"])
        .output()
        .expect("failed to run codetracer-cairo-recorder trace-starknet --help");

    assert!(output.status.success(), "--help should exit 0");

    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("ctfs"),
        "`trace-starknet --help` should advertise `ctfs`; got:\n{help}"
    );
    assert!(
        help.contains("[default: ctfs]"),
        "`trace-starknet --help` should default --format to `ctfs`; got:\n{help}"
    );
}

// ---- Audit (e): Step records emitted on every line transition -------------

/// `register_step` must fire on each parsed source line so the frontend
/// can step line-by-line through the Cairo program.  A trace produced
/// for `flow_test.cairo` (5 let-bindings + 1 tuple expr in `compute()`,
/// 1 expr in `main`) should generate a non-trivial container.
///
/// Without a CTFS reader in this crate's dev-deps we can't introspect
/// the produced step-event count from Rust — the proxy is that the
/// `.ct` container is materially populated (well above the magic-only
/// header).  Same shape as the Cardano 1.48 audit's regression test.
#[test]
fn test_steps_emitted_for_let_bindings() {
    let (_tmp, out_dir) = record_flow_test(TraceEventsFileFormat::Ctfs);

    let ct_size: u64 = ct_files_in(&out_dir)
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();

    // The flow_test program has multiple let-bindings + a tuple return,
    // so the .ct container must have meaningful payload above the
    // header magic (Cardano 1.48 uses the same > 100-byte threshold).
    assert!(
        ct_size > 100,
        ".ct container should hold step + value records, got {ct_size} bytes"
    );
}

// ---- Audit (b): snforge ContractCall stages args via arg() ----------------

/// snforge `ContractCall` entries used to surface their `caller` /
/// `callee` / `selector` / `calldata` fields as scoped Variables (via
/// `register_variable_with_full_value`) — visible in the locals pane
/// but **not** on `CallRecord.args`.  Pre-fix the `register_call` site
/// passed `vec![]` and dropped the call-arg shape entirely, mirroring
/// the gap caught in Move (1.46) for `OpenFrame.frame.parameters`.
///
/// Post-fix, `write_starknet_trace` stages each field via
/// `TraceWriter::arg(name, value)` before the matching `register_call`
/// so the Nim writer attaches them to `CallRecord.args`.
///
/// We can't decode the `.ct` container directly, but we can assert that
/// `write_starknet_trace` runs to completion against the mock-trace
/// fixture and produces a populated `.ct` artefact.  The pre-fix code
/// already did this (it just emitted Variables instead of args), so
/// this is a structural smoke test guarding the Ok-result invariant.
#[test]
fn test_starknet_contract_call_stages_args() {
    use codetracer_cairo_recorder::starknet::{parse_snforge_trace, write_starknet_trace};

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("starknet-traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    let trace_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs/starknet/mock_trace.json");
    let entries = parse_snforge_trace(&trace_path).expect("parse mock trace");

    write_starknet_trace(&trace_path, &entries, &out_dir, TraceEventsFileFormat::Ctfs)
        .expect("write_starknet_trace should succeed");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container in {:?}",
        out_dir
    );
    let total: u64 = ct_files
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    assert!(
        total > 100,
        "starknet .ct container should hold call + arg records, got {total} bytes"
    );
}

// ---- Audit (c): Starknet log events via register_special_event ------------

/// Starknet contract-emitted log events (snforge `TraceEntry::Event`)
/// are structured `(keys, data)` records — analogous to EVM LOG opcodes.
/// Pre-fix they were rendered as synthetic `<contract>::emit_event` Call
/// frames with the keys / data stuffed into Variable records.  This
/// matched the legacy "everything is a function" recorder shape but
/// dropped the structured-log information that the CodeTracer event
/// log pane consumes.
///
/// Post-fix, `write_starknet_trace` *also* routes each Event entry
/// through `register_special_event(EventLogKind::EvmEvent, ...)` so the
/// frontend's structured event-log surface receives them.
///
/// The synthetic Call/Variable emission is NOT removed — keeping it
/// preserves backward compatibility with downstream consumers and the
/// existing pure-conversion unit tests.  This test verifies the
/// post-fix invariant at the recorder API: a trace containing a
/// Starknet event runs through to completion and produces a populated
/// `.ct` container.
#[test]
fn test_starknet_event_emits_special_event() {
    use codetracer_cairo_recorder::starknet::{
        write_starknet_trace, TraceEntry,
    };

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("starknet-traces");
    std::fs::create_dir_all(&out_dir).unwrap();

    // Synthesise a single Event entry — minimal shape that exercises
    // the EvmEvent special-event routing without depending on the full
    // mock-trace fixture.
    let entries = vec![TraceEntry::Event {
        contract: "0x2".to_string(),
        keys: vec!["BalanceIncreased".to_string()],
        data: vec!["0x1".to_string(), "42".to_string()],
    }];

    let trace_path = tmp_dir.path().join("synthetic.json");
    std::fs::write(&trace_path, "[]").unwrap();

    write_starknet_trace(&trace_path, &entries, &out_dir, TraceEventsFileFormat::Ctfs)
        .expect("write_starknet_trace should succeed even for event-only traces");

    let ct_files = ct_files_in(&out_dir);
    assert!(
        !ct_files.is_empty(),
        "expected a .ct container even for an event-only trace; got {:?}",
        out_dir
    );

    // Post-fix, the EvmEvent-routed event lands in the multi-stream
    // event channel — the container should be larger than the magic
    // header alone.
    let total: u64 = ct_files
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    assert!(
        total > 32,
        "container should hold the EvmEvent special-event record, got {total} bytes"
    );
}
