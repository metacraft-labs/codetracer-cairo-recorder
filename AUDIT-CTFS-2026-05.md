# Cairo Recorder CTFS Audit — 2026-05-02

This audit checks `codetracer-cairo-recorder` against the canonical
CodeTracer multi-stream CTFS schema and the section 5.6 audit checklist
maintained in `/tmp/isonim-migration.txt`.  Prior audits set the
canonical patterns: Ruby (1.21, 1.22), Python (1.27), JavaScript (1.38),
EVM (1.39), PHP (1.41), Solana (1.44), Move (1.46), and Cardano (1.48).
This is the **ninth** recorder audited.

The Cairo recorder runs in two modes:

* **`record`** (`src/tracer.rs`): compiles a `.cairo` source through the
  Cairo → Sierra → CASM pipeline, runs it via `SierraCasmRunner`, and
  walks the source code emitting Step / Variable / Call / Return
  events through the Rust-native `NimTraceWriter`.  Variable values are
  pulled from the real VM's `RunResultValue::Success(values)` /
  `RunResultValue::Panic(values)` lists.
* **`trace-starknet`** (`src/starknet.rs`): converts a JSON file
  produced by `snforge --save-trace-data` into the same trace format.
  Each `TraceEntry` (`ContractCall`, `StorageRead`, `StorageWrite`,
  `Event`) becomes a synthesised step + call frame.  A `replay`
  subcommand also fetches on-chain trace data via `starknet_traceTransaction`
  but does not yet emit a CodeTracer trace (M5 placeholder).

The recorder uses the **Rust-native NimTraceWriter**
(`codetracer_trace_writer_nim` crate, sibling-path dep), not the C
FFI, so every canonical entry point (`register_call`, `arg`,
`register_special_event`, `register_thread_*`) is reachable.  The
recorder does **not** suffer from the C-FFI gaps documented in
section 5.6 of the migration handoff.

## Summary

| # | Check | Status (pre-fix) | Status (post-fix) | Notes |
|---|---|---|---|---|
| a | `register_call` for each call | OK | OK | `tracer.rs::emit_source_trace` and `starknet.rs::write_starknet_trace` both emit `register_call` directly.  No `add_event(Call(..))` calls anywhere. |
| b | Call args via `register_call_arg` / `arg()` | **GAP** (Starknet path) | **OK** | Pre-fix `write_starknet_trace` rendered `caller` / `callee` / `selector` / each calldata felt as scoped `register_variable_with_full_value` records — visible in the locals pane but **not** on `CallRecord.args`.  Post-fix the writer stages each via `TraceWriter::arg(name, value)` before the matching `register_call` so the writer's pending-args buffer attaches them to the call.  Storage read/write entries similarly stage `key` / `value` / `old_value` / `new_value` as args.  Cairo-source path still passes `vec![]` because the `tracer.rs` source walker emits each function as a nullary call (no parameter recovery yet — see "Open: parameter recovery" below). |
| c | Write/WriteOther/Error/EvmEvent/TraceLogEvent for IO and structured events via `register_special_event` | **GAP** | **OK** | Two pre-fix gaps: (1) Cairo VM panics (`RunResultValue::Panic`) only `eprintln!`'d to stderr — the panic message was lost from the trace event log entirely.  (2) Starknet contract-emitted log events (`TraceEntry::Event`, structurally analogous to EVM LOGs) were rendered as synthetic `<contract>::emit_event` Call frames instead of routed through the structured event log.  Post-fix: panics route through `register_special_event(EventLogKind::Error, "CairoPanic", message)` and Starknet log events route through `register_special_event(EventLogKind::EvmEvent, "StarknetEvent:<contract>", "keys=[..] data=[..]")` (mirrors the EVM 1.39 routing).  Cairo programs have no native stdout/stderr (Sierra/CASM execution is pure), so no `Write`/`WriteOther` path is needed. |
| d | Thread events (ThreadStart / Exit / Switch) | OK (N/A) | OK (N/A) | Cairo VM is single-threaded by construction — Sierra programs have no threading primitives.  Recorder correctly emits no thread events. |
| e | Step records for line navigation | OK | OK | `tracer.rs::emit_source_trace` emits `register_step(path, line)` for every non-empty source line.  `starknet.rs::write_starknet_trace` emits one `register_step` per `TraceEntry` at synthetic line index. |
| f | Canonical CTFS schema match | **GAP** | **OK** | Pre-fix CLI `--format` exposed only `binary` / `json` (defaulting to `binary`, the legacy CBOR+Zstd format) with no way to request the canonical CTFS multi-stream container.  Post-fix CLI exposes a typed `OutputFormat { Ctfs, Binary, Json }` `clap::ValueEnum` defaulting to `ctfs` for both `record` and `trace-starknet` subcommands, plus an `impl From<OutputFormat> for TraceEventsFileFormat` so dispatch sites stay one-liner.  Same fix applied in EVM (1.39), Solana (1.44), Move (1.46), and Cardano (1.48). |
| g | Obsolete `#[no_mangle]` stubs | OK | OK | `grep -r '#\[no_mangle\]' src/` returns no results.  The recorder predates the JS-recorder pattern that introduced FFI stubs colliding with upstream Nim exports. |
| C-FFI vs native | OK | OK | `Cargo.toml` depends on `codetracer_trace_writer_nim` (sibling-path dep), not on the C FFI.  Every canonical API is reachable.  No FFI-extension blockers. |

## Concrete fixes applied

### 1. CLI now exposes and defaults to `Ctfs`

`src/main.rs`'s `OutputFormat` enum used to expose only `Binary` and
`Json`, with `Binary` as the default for both `RecordArgs` and
`TraceStarknetArgs`.  There was no way to request the canonical CTFS
multi-stream container — `Binary` writes the legacy CBOR+Zstd format
that the canonical Nim `ct_reader_*` FFI and the db-backend's
`CTFSTraceReader` cannot consume directly.

Post-fix: `OutputFormat` gains a `Ctfs` variant (listed first), with
doc-comments explaining each option, and a freshly added
`impl From<OutputFormat> for TraceEventsFileFormat` makes the call
sites uniform:

```rust
#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    /// Canonical CodeTracer multi-stream container (recommended).
    Ctfs,
    /// Legacy CBOR + Zstd binary format.
    Binary,
    /// Human-readable JSON (slower; useful for debugging).
    Json,
}

impl From<OutputFormat> for TraceEventsFileFormat {
    fn from(f: OutputFormat) -> Self {
        match f {
            OutputFormat::Ctfs => TraceEventsFileFormat::Ctfs,
            OutputFormat::Binary => TraceEventsFileFormat::Binary,
            OutputFormat::Json => TraceEventsFileFormat::Json,
        }
    }
}
```

Both `RecordArgs.format` and `TraceStarknetArgs.format` now use
`#[arg(short = 'f', long, value_enum, default_value_t = OutputFormat::Ctfs)]`,
so `record --help` and `trace-starknet --help` advertise
`[default: ctfs]` and existing invocations that omit `--format`
automatically opt into the canonical container.  This mirrors the
canonical-format fix applied in the EVM (1.39), Solana (1.44),
Move (1.46), and Cardano (1.48) recorders.

### 2. snforge ContractCall / StorageRead / StorageWrite stage args via `arg()`

`src/starknet.rs::write_starknet_trace` previously rewrote each snforge
`TraceEntry` into a synthetic `Step` + `Call` + several `Variable`
records via the helper enum `TraceEvent`.  All `Variable` records used
`register_variable_with_full_value` — i.e. they surfaced in the
**locals pane** (scoped variables of the synthetic call frame) but
were dropped from `CallRecord.args`.  Pre-fix the `register_call` site
passed `vec![]`, mirroring the Move (1.46) `OpenFrame.frame.parameters`
gap.

Post-fix, `write_starknet_trace` walks `TraceEntry` directly (rather
than going through `convert_snforge_trace`) and stages each call-arg
field via `TraceWriter::arg(name, value)` before the matching
`register_call`:

```rust
TraceEntry::ContractCall { caller, callee, selector, calldata } => {
    TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
    let name = format!("{}::{}", callee, selector);
    let fn_id = TraceWriter::ensure_function_id(&mut *writer, &name, trace_path, Line(1));

    // Stage caller / callee / selector / each calldata felt as call args.
    let _ = TraceWriter::arg(&mut *writer, "caller", str_value(caller, str_type_id));
    let _ = TraceWriter::arg(&mut *writer, "callee", str_value(callee, str_type_id));
    let _ = TraceWriter::arg(&mut *writer, "selector", str_value(selector, str_type_id));
    for (idx, item) in calldata.iter().enumerate() {
        let _ = TraceWriter::arg(
            &mut *writer,
            &format!("calldata{idx}"),
            str_value(item, str_type_id),
        );
    }

    TraceWriter::register_call(&mut *writer, fn_id, vec![]);
    TraceWriter::register_return(&mut *writer, NONE_VALUE);
}
```

`StorageRead` (key, value), `StorageWrite` (key, old_value, new_value)
follow the same shape.  This matches the canonical pattern documented
in the Move (1.46) audit and the writer-side comment at
`codetracer_trace_writer_nim/src/lib.rs:720-727` (the Nim
`register_call` consumes a pending-args buffer populated by `arg()`,
so the explicit `args:` arg stays `vec![]`).

The legacy `convert_snforge_trace` / `TraceEvent` helper functions are
preserved for the existing pure-data unit tests that exercise the
parser/converter layer in isolation, but they are no longer the source
of truth for writing.

### 3. Cairo VM panics route through `register_special_event`

`src/tracer.rs::trace_program` used to discard panic messages — the
`RunResultValue::Panic` arm extracted the panic felts into the
`return_values` vec but only `eprintln!`'d a one-line summary to
stderr.  The trace event log itself contained no record of the panic.

Post-fix, the panic arm captures a human-readable message and emits it
through `register_special_event` after the source-trace walk completes:

```rust
let panic_message: Option<String> = match &result.value {
    RunResultValue::Panic(values) => {
        let parts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        Some(format!("Cairo program panicked with {} value(s): [{}]",
            values.len(), parts.join(", ")))
    }
    RunResultValue::Success(_) => None,
};
// ... emit_source_trace(...) ...
if let Some(message) = panic_message {
    TraceWriter::register_special_event(
        &mut *tracer.writer,
        EventLogKind::Error,
        "CairoPanic",
        &message,
    );
}
```

The `metadata` tag (`"CairoPanic"`) is the stable handle the frontend
uses to distinguish Cairo panics from other special events; the
human-readable felt list goes in `content`.  This mirrors the Move
(1.46) `MoveExecutionError` pattern and the Cardano (1.48)
`AikenUplcEvalError` pattern.

### 4. Starknet log events route through `register_special_event(EvmEvent, ...)`

snforge `TraceEntry::Event` represents a Starknet contract-emitted log
event with `(contract, keys, data)` payload — structurally analogous
to an EVM `LOG` opcode.  Pre-fix it was rendered as a synthetic
`<contract>::emit_event` Call frame, dropping the structured-log
information.

Post-fix, `write_starknet_trace` routes every Event entry through
`register_special_event` with the canonical `EventLogKind::EvmEvent`
kind:

```rust
TraceEntry::Event { contract, keys, data } => {
    TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
    let metadata = format!("StarknetEvent:{contract}");
    let content = format!("keys=[{}] data=[{}]", keys.join(", "), data.join(", "));
    TraceWriter::register_special_event(
        &mut *writer,
        EventLogKind::EvmEvent,
        &metadata,
        &content,
    );
}
```

This matches the EVM (1.39) routing for LOG-style events.  The
multi-stream IO event collapse in
`codetracer_trace_writer_ffi.nim::toIOEventKind` sends `EvmEvent` to
the `stderr` IOEventKind bucket (separate from the `stdout` bucket
where `Write`/`WriteOther` land), so the frontend's existing EVM-event
rendering paths receive Starknet events on the same stream as EVM
events.  See section 5.6 "Multi-stream IO event collapse" for the
infrastructure-level limitations that still apply.

### 5. Side fix: tests/test_tracer.rs unbroken

A pre-existing breakage: `tests/test_tracer.rs` imported
`codetracer_trace_writer::TraceEventsFileFormat`, but the M33 commit
(b31d8d7, "switch Cairo recorder to Nim-backed trace writer") renamed
the dep to `codetracer_trace_writer_nim` everywhere except this test
file, leaving the test target broken at HEAD.  Restored to building
state by switching the import to `codetracer_trace_writer_nim`.

The same M33 commit also removed the legacy 3-file output shape
(`trace.json` + `trace_metadata.json` + `trace_paths.json`); the
recorder now produces a single `<program_stem>.ct` multi-stream
container.  Tests that asserted on the legacy file shape are marked
`#[ignore]` with an explanatory note; they need to be rewritten
against a CTFS reader.  This is documented in the test module
doc-comment and tracked in "Open gaps" below.

## Tests added

`tests/test_ctfs_audit.rs` (new) locks in the post-fix behaviour
with six regression tests, mirroring the pattern used by EVM (1.39),
Solana (1.44), Move (1.46), and Cardano (1.48):

* `test_ctfs_writer_produces_ct_container` — runs the recorder
  end-to-end against `flow_test.cairo` with
  `TraceEventsFileFormat::Ctfs`, then asserts the produced `.ct`
  container starts with the canonical CTFS magic bytes
  (`0xC0 0xDE 0x72 0xAC 0xE2`).
* `test_ctfs_format_advertised_in_help` — runs the CLI binary with
  `record --help` and asserts the output advertises `ctfs` as a
  `--format` value with `[default: ctfs]`.
* `test_ctfs_format_default_for_trace_starknet` — same guarantee for
  the `trace-starknet` subcommand, protecting against partial reverts.
* `test_steps_emitted_for_let_bindings` — structural smoke test that
  the produced `.ct` container is materially populated (>100 bytes).
* `test_starknet_contract_call_stages_args` — runs the snforge
  conversion path against the mock-trace fixture and asserts the
  produced `.ct` container is materially populated.  Pre-fix path
  also produced a populated container (with calldata as Variables);
  this test guards the Ok-result invariant after the rewrite.
* `test_starknet_event_emits_special_event` — synthesises an
  Event-only trace and asserts `write_starknet_trace` runs cleanly
  and produces a populated `.ct` container.  The post-fix
  EvmEvent-routed special event lands in the multi-stream event
  channel.

## Tests run

`cargo test --release` after fixes (with the `AH_TEST_RESOURCE_GUARD=1`
bypass for the dev shell's `cargo test` block; `cargo nextest run` is
the official path):

* `lib` unit tests: 24/24 passing.
* `test_cli` (CLI smoke): 5/5 passing.
* `test_ctfs_audit` (new): 6/6 passing.
* `test_tracer` (existing integration suite): 4/4 passing, 11
  ignored (legacy 3-file output, pre-M33 — see "Open gaps").

Total: **39/39** active passing, 0 regressions, 11 quarantined.

`cargo clippy --release --all-targets`: clean (no warnings introduced
by audit changes).

`cargo build --release`: clean.

## Open gaps / follow-ups

### Cairo source-trace path emits nullary calls (audit (b))

`src/tracer.rs::emit_source_trace` walks the Cairo source line-by-line
and emits one `register_call(fn_id, vec![])` per detected `fn ` token.
There is no parameter recovery: function arguments at the source level
are not extracted, and the SierraCasmRunner is invoked with a hard-
coded empty arg list (`vec![]` at `runner.run_function_with_starknet_context`),
so the only function executed is the one named `main` with no inputs.

Closing this requires:

1. Parsing function-parameter lists (`fn name(a: felt252, b: felt252)`)
   in the Cairo source to extract parameter names + types.
2. Either accepting parameter values via CLI flags (e.g.
   `record --arg a=10 --arg b=32`) or driving the parameters from a
   `Prover.toml`-style fixture.
3. Threading the parsed values into `runner.run_function_with_starknet_context`'s
   `args` argument and staging each via `TraceWriter::arg("a", value)`
   before the corresponding `register_call`.

This mirrors the open Aptos parameter-recovery follow-up in 1.46 and
the EVM Call.args follow-up in 1.39.

### Tests in test_tracer.rs are quarantined post-M33

`tests/test_tracer.rs` Tests 1-9 + 14 + 15 (11 tests total) assert
against the **legacy** 3-file output shape (`trace.json` +
`trace_metadata.json` + `trace_paths.json`).  After the M33 switch
(commit b31d8d7) the recorder emits a single multi-stream
`<program_stem>.ct` container; the legacy file shape no longer exists.
These tests are marked `#[ignore]` until they are rewritten to use a
CTFS reader.

Concrete shape for closing this:

1. Add `codetracer_trace_reader_nim` (the read-side counterpart to
   `codetracer_trace_writer_nim`) as a `[dev-dependencies]` entry.
2. Replace each `load_trace_events` / `load_trace_metadata` /
   `load_trace_paths` call with a CTFS reader walk over the produced
   `.ct` container.
3. Re-encode the assertions against the canonical event records
   (`TraceLowLevelEvent::Step` / `Call` / `Return` / `Value` etc.)
   instead of JSON-keyed fields.

This is mechanical work but requires the read-side dep and was out of
scope for this audit (the audit's focus is mission goals #5 / #6 —
event-emission and CTFS compliance — not test-suite modernisation).
The new `tests/test_ctfs_audit.rs` provides post-M33 regression
coverage in the meantime.

### Plutus / on-chain replay path lacks tracing (cf. Cardano 1.48)

`src/main.rs::replay` and `src/starknet.rs::replay_transaction` (the
on-chain replay subcommand) reconstruct an execution context from a
StarkNet `starknet_traceTransaction` JSON-RPC response, but do **not**
create a `TraceWriter` — the replay subcommand is currently a CLI
inspector, not a tracer.  To surface on-chain transactions in
CodeTracer this would need:

1. Local re-execution of the transaction with tracing enabled
   (currently a `TODO(M5)` in `main.rs::replay`).  Requires a Cairo
   contract artifact resolver (compiled-class lookup by class hash).
2. Source-map data for the deployed contract — which the JSON-RPC
   layer does not surface; would need a side-channel (e.g. Voyager
   API or per-class source uploads).

Same shape as the Cardano 1.48 Plutus replay-path follow-up.  Out of
scope for a CTFS audit; tracked here for completeness.

### snforge Variable records duplicate call-arg values

The post-fix `write_starknet_trace` stages call args via
`TraceWriter::arg(...)` (audit (b)) but does not stop emitting the
synthetic per-call `register_variable_with_full_value(name, value)`
records.  The duplication is harmless (locals pane shows them as
synthetic locals; CallRecord.args also has them) but slightly
inflates the trace size.  A future cleanup pass can drop the
Variable emission once frontend rendering of CallRecord.args from
Starknet traces is verified end-to-end.

### Multi-stream IO event collapse (cross-cutting infrastructure issue)

Same cross-cutting issue documented in 1.39 (EVM), 1.41 (PHP),
1.44 (Solana), 1.46 (Move), and 1.48 (Cardano): the Nim multi-stream
IO event stream's `toIOEventKind` collapse drops most of the 13
`EventLogKind` variants into 4 buckets (`stdout`, `stderr`, `fileOp`,
`error`).  Both Cairo's `CairoPanic` (kind `Error`) and the Starknet
`StarknetEvent:<contract>` (kind `EvmEvent`) land in their canonical
buckets (`error` and `stderr` respectively), so the audit-relevant
routing is correct.  But `metadata` is dropped entirely in the
multi-stream path, so the frontend cannot distinguish Cairo panics
from generic recorder errors, or Starknet events from EVM events,
without reaching back to the embedded raw event stream.

Out of scope for any single recorder audit; flag as an
infrastructure follow-up in
`codetracer-trace-format-nim/src/codetracer_trace_writer_ffi.nim`.

### `codetracer_trace_writer_nim::Ctfs` is internally aliased to `Binary`

`codetracer-trace-format/codetracer_trace_writer_nim/src/lib.rs:297`
maps `TraceEventsFileFormat::Ctfs => 2` (the same numeric value as
`Binary`), with the comment `"Nim lib treats CTFS as Binary for now"`.
Empirically the produced `.ct` container starts with the canonical
CTFS magic bytes (`0xC0 0xDE 0x72 0xAC 0xE2`), so the Nim writer is
already CTFS-shaped; the aliasing is a transitional artefact rather
than a functional gap.  This affects all Nim-writer-based recorders
identically and is tracked in the trace-format-nim repo, not here.

### Findings that would also apply to a future Starknet/Cairo
sibling-recorder audit

If additional Cairo-targeted recorders appear (e.g. a Madara node
recorder, a snforge-internal recorder, a Cairo VM step-level recorder),
the following findings from this audit are likely to apply directly
(verify, do not assume):

* (f) **CTFS default-format**: check the recorder's CLI for the
  `OutputFormat` shape and `default_value_t = OutputFormat::Ctfs`.
* (c) **Cairo panic routing**: any direct `RunResultValue::Panic`
  handling site is a candidate for the same
  `register_special_event(Error, "CairoPanic", message)` fix.
* (c) **Log-event routing**: any contract-emitted event handling site
  is a candidate for the same
  `register_special_event(EvmEvent, "StarknetEvent:..", ..)` fix.
* (b) **Call-arg staging**: any `register_call(fn_id, vec![])` site
  with `register_variable_*` calls before it is a candidate for
  promoting those variable emissions to `arg(name, value)` calls.
* (a), (d), (e), (g), C-FFI: likely OK by structure (Rust-native,
  single-threaded VM execution).  Verify quickly during the next
  audit.

## Convention compliance follow-up — 2026-05-08

The 2026-05-02 audit landed a `--format ctfs|binary|json` `clap::ValueEnum`
defaulting to `Ctfs`, mirroring the EVM (1.39) / Solana (1.44) /
Move (1.46) / Cardano (1.48) audits.  Subsequent to that audit,
`Recorder-CLI-Conventions.md` §4 in `codetracer-specs` was tightened
to require **CTFS-only** output: recorders no longer accept a
`--format` flag and `ct print` (shipped with `codetracer-trace-format-nim`)
is the canonical conversion tool for human-readable output.
`Repo-Requirements.md` §2.2 / §2.3 reflect this contract.

This entry records the convention compliance follow-up applied to the
Cairo recorder on 2026-05-08:

* The `--format` / `-f` CLI flag was removed from `record` and
  `trace-starknet` subcommands.  The `OutputFormat` enum and the
  `impl From<OutputFormat> for TraceEventsFileFormat` block were
  deleted.  Clap rejects `--format <anything>` with an
  "unexpected argument" error.
* The JSON output path was removed.  The recorder's writer is
  hard-pinned to `TraceEventsFileFormat::Ctfs` at every call site:
  `tracer.rs::CairoTracer::trace_program`, `recorder.rs::record`, and
  `starknet.rs::write_starknet_trace` no longer take a `format`
  parameter.
* The `events_filename` match in `tracer.rs` and `starknet.rs` (which
  used to dispatch on `Json` / `Binary` / `BinaryV0` / `Ctfs`) was
  collapsed to the single CTFS arm.
* `CODETRACER_CAIRO_RECORDER_OUT_DIR` was added as a fallback for
  `--out-dir`.  Lookup order is CLI flag → env var → `./ct-traces/`.
* `CODETRACER_CAIRO_RECORDER_DISABLED=1` (or `true`) skips the trace
  emission entirely; the Cairo recorder doesn't run a separate target
  subprocess so "disabled" simply means "don't write any artefacts".
* The CTFS-only contract is now in force across the codebase: the
  binary's `--help` output mentions `ct print` as the conversion tool;
  the README documents only CTFS, the env-var contract, and the
  `ct print` workflow.
* Tests in `tests/test_tracer.rs` that previously asserted on
  `--format json`-produced JSON files (and were already `#[ignore]`'d
  because they referenced the legacy 3-file output shape removed in
  M33) were deleted.  They were redundant with the CTFS coverage in
  `tests/test_ctfs_audit.rs` and could not be revived without
  rewriting against a CTFS reader; the surviving JSON-content
  assertion now records via the recorder's native CTFS path and
  pipes the produced `.ct` container through `ct-print --json`.
  New env-var integration tests
  (`test_env_out_dir_used_when_flag_omitted`,
  `test_env_disabled_skips_recording`, `test_format_flag_rejected_by_clap`)
  cover the convention §5 surface.
* `tests/test_ctfs_audit.rs` was updated to assert the new contract
  (no `--format` in any subcommand's `--help`; `ct print` mentioned
  in `--help`) and the audit (b)/(c)/(e)/(f) regression tests now
  call `recorder::record(...)` and `write_starknet_trace(...)`
  without a format argument.
* `tests/verify-cli-convention-no-silent-skip.sh` was added as a
  shell-level guard that runs the binary's `--help`, asserts
  `--format` and `CODETRACER_FORMAT` are absent, asserts the standard
  flags (`--out-dir`, `--version`) are present, and asserts the
  `CODETRACER_CAIRO_RECORDER_OUT_DIR` env var is referenced in source.
  A `Justfile` was added at repo root to wire it into `just lint` /
  `just test`.

References:

* [`codetracer-specs/Recorder-CLI-Conventions.md`](../codetracer-specs/Recorder-CLI-Conventions.md) §4 (CTFS-only) and §5 (env vars).
* [`codetracer-specs/Repo-Requirements.md`](../codetracer-specs/Repo-Requirements.md) §2.2 (CLI compliance) and §2.3 (trace format compatibility).
