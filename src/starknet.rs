//! StarkNet contract trace parsing and conversion.
//!
//! Parses trace output from `snforge --save-trace-data` and converts it
//! into CodeTracer trace events. This provides an alternative path for
//! recording StarkNet contract execution — instead of compiling and
//! running Cairo directly, users can run `snforge` separately and feed
//! the resulting trace files into this converter.

use std::path::{Path, PathBuf};

use eyre::{eyre, Context, Result};
use serde::{Deserialize, Serialize};

use codetracer_trace_types::{EventLogKind, Line, TypeKind, ValueRecord, NONE_VALUE};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{create_trace_writer, TraceEventsFileFormat};

// ---------------------------------------------------------------------------
// Trace entry types
// ---------------------------------------------------------------------------

/// A single entry in an snforge trace file.
///
/// Represents one observable event during StarkNet contract execution:
/// a contract call, storage access, or emitted event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TraceEntry {
    /// A call from one contract to another (or to itself).
    ///
    /// M10 round-4: optional `visibility` / `self_kind` fields encode
    /// the StarkNet ABI decorator class for the called method:
    ///
    /// * `visibility` ∈ {"external", "view", "internal"}.  When
    ///   present, the function table entry the converter writes is
    ///   `<visibility>::<callee>::<selector>` so consumers can group
    ///   functions by visibility class without re-parsing the
    ///   contract source.  Absent → the legacy `<callee>::<selector>`
    ///   form is preserved (no behaviour change for existing
    ///   fixtures).
    /// * `self_kind` ∈ {"ref", "snapshot"}.  When present, the
    ///   converter emits a typed `ValueRecord::Reference` arg named
    ///   `self_kind` whose `mutable` flag is `true` for `"ref"` and
    ///   `false` for `"snapshot"` — pinning the @-vs-ref split that
    ///   distinguishes a state-mutating external from a read-only
    ///   view at the trace level.
    ///
    /// M10 round-5: optional `dispatcher_trait` field encodes the
    /// `#[starknet::interface]` Dispatcher pattern for cross-contract
    /// calls.  When present, the function table entry the converter
    /// writes is `<dispatcher_trait>::<callee>::<selector>` so
    /// consumers can group dispatcher-mediated calls under the
    /// trait identity, and a typed `ValueRecord::String` arg named
    /// `dispatcher_trait` is staged on the call so the trait
    /// identity surfaces alongside the canonical caller / callee /
    /// selector args.  Absent → the legacy `<callee>::<selector>`
    /// (or `<visibility>::<callee>::<selector>`) form is preserved.
    /// Mutual exclusivity: `visibility` and `dispatcher_trait` are
    /// independent — fixtures supply one or the other depending on
    /// whether the call is direct (visibility known) or mediated by
    /// a dispatcher (trait known).  When both are absent the
    /// pre-round-5 contract is preserved.
    #[serde(rename = "contract_call")]
    ContractCall {
        caller: String,
        callee: String,
        selector: String,
        calldata: Vec<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        visibility: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        self_kind: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        dispatcher_trait: String,
    },

    /// A storage read operation.
    #[serde(rename = "storage_read")]
    StorageRead {
        contract: String,
        key: String,
        value: String,
    },

    /// A storage write operation.
    #[serde(rename = "storage_write")]
    StorageWrite {
        contract: String,
        key: String,
        old_value: String,
        new_value: String,
    },

    /// An event emitted by a contract.
    #[serde(rename = "event")]
    Event {
        contract: String,
        keys: Vec<String>,
        data: Vec<String>,
    },

    /// A StarkNet runtime syscall (e.g. `get_caller_address`,
    /// `get_block_timestamp`, `get_contract_address`).  M10 round-3:
    /// each surfaces as a Call/Return frame named after the syscall,
    /// with the return value typed by `return_kind`:
    ///
    /// * `"address"` — `ValueRecord::Raw` carrying the 32-byte
    ///   big-endian felt of a contract / wallet address.  Encoded as a
    ///   `0x`-prefixed hex string of up to 32 bytes (64 hex chars) in
    ///   the JSON; the recorder pads to 32 bytes.
    /// * `"u64"` — `ValueRecord::Int` decoded from the decimal /
    ///   `0x`-prefixed string.  Used for the block-timestamp /
    ///   block-number / nonce-style syscalls.
    ///
    /// Unrecognised `return_kind` values fall back to a string-typed
    /// `ValueRecord::String` so the trace stays self-describing without
    /// pretending to know the value's shape.
    #[serde(rename = "syscall")]
    Syscall {
        name: String,
        return_kind: String,
        return_value: String,
    },
}

/// Top-level structure of an snforge trace file produced by
/// `snforge --save-trace-data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnforgeTrace {
    /// The test or entry-point name that produced this trace.
    #[serde(default)]
    pub test_name: String,

    /// Contract address of the test runner / entry point.
    #[serde(default)]
    pub contract_address: String,

    /// Ordered list of trace entries.
    pub entries: Vec<TraceEntry>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse an snforge trace file (JSON) into a vector of [`TraceEntry`] values.
pub fn parse_snforge_trace(path: &Path) -> Result<Vec<TraceEntry>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read snforge trace file: {}", path.display()))?;

    let trace: SnforgeTrace = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse snforge trace JSON: {}", path.display()))?;

    Ok(trace.entries)
}

/// Wrap a string snippet as a `ValueRecord::String` typed as the
/// already-registered Cairo `felt252` string type.
///
/// All snforge trace fields are JSON strings (felt252 hex / decimal
/// representations).  Wrapping them in `ValueRecord::String` keeps the
/// downstream `arg(name, value)` / `register_variable_*` plumbing
/// uniform with the Cairo source-trace path.
fn str_value(text: &str, str_type_id: codetracer_trace_types::TypeId) -> ValueRecord {
    ValueRecord::String {
        text: text.to_string(),
        type_id: str_type_id,
    }
}

// ---------------------------------------------------------------------------
// Conversion to CodeTracer events
// ---------------------------------------------------------------------------

/// A CodeTracer-compatible trace event produced from an snforge trace entry.
///
/// These mirror the events that `TraceWriter` emits (Step, Call, Return,
/// VariableName, Value) but are kept as plain data so that they can be
/// inspected and tested without a live `TraceWriter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    /// A step at a synthetic line.
    Step { line: u32 },

    /// A function/contract call.
    Call { name: String },

    /// A return from a call.
    Return,

    /// A variable binding (name + string value).
    Variable { name: String, value: String },
}

/// Convert a slice of [`TraceEntry`] values into a sequence of
/// [`TraceEvent`] values suitable for writing via `TraceWriter`.
///
/// The conversion assigns a synthetic line number to each entry
/// (starting at 1) so that the CodeTracer UI can display them in order.
pub fn convert_snforge_trace(entries: &[TraceEntry]) -> Vec<TraceEvent> {
    let mut events = Vec::new();
    let mut line: u32 = 1;

    for entry in entries {
        match entry {
            TraceEntry::ContractCall {
                caller,
                callee,
                selector,
                calldata,
                ..
            } => {
                let name = format!("{}::{}", callee, selector);
                events.push(TraceEvent::Step { line });
                events.push(TraceEvent::Call { name });
                events.push(TraceEvent::Variable {
                    name: "caller".to_string(),
                    value: caller.clone(),
                });
                events.push(TraceEvent::Variable {
                    name: "callee".to_string(),
                    value: callee.clone(),
                });
                events.push(TraceEvent::Variable {
                    name: "selector".to_string(),
                    value: selector.clone(),
                });
                if !calldata.is_empty() {
                    events.push(TraceEvent::Variable {
                        name: "calldata".to_string(),
                        value: format!("[{}]", calldata.join(", ")),
                    });
                }
                events.push(TraceEvent::Return);
            }
            TraceEntry::StorageRead {
                contract,
                key,
                value,
            } => {
                events.push(TraceEvent::Step { line });
                events.push(TraceEvent::Call {
                    name: format!("{}::storage_read", contract),
                });
                events.push(TraceEvent::Variable {
                    name: "key".to_string(),
                    value: key.clone(),
                });
                events.push(TraceEvent::Variable {
                    name: "value".to_string(),
                    value: value.clone(),
                });
                events.push(TraceEvent::Return);
            }
            TraceEntry::StorageWrite {
                contract,
                key,
                old_value,
                new_value,
            } => {
                events.push(TraceEvent::Step { line });
                events.push(TraceEvent::Call {
                    name: format!("{}::storage_write", contract),
                });
                events.push(TraceEvent::Variable {
                    name: "key".to_string(),
                    value: key.clone(),
                });
                events.push(TraceEvent::Variable {
                    name: "old_value".to_string(),
                    value: old_value.clone(),
                });
                events.push(TraceEvent::Variable {
                    name: "new_value".to_string(),
                    value: new_value.clone(),
                });
                events.push(TraceEvent::Return);
            }
            TraceEntry::Event {
                contract,
                keys,
                data,
            } => {
                events.push(TraceEvent::Step { line });
                events.push(TraceEvent::Call {
                    name: format!("{}::emit_event", contract),
                });
                if !keys.is_empty() {
                    events.push(TraceEvent::Variable {
                        name: "event_keys".to_string(),
                        value: format!("[{}]", keys.join(", ")),
                    });
                }
                if !data.is_empty() {
                    events.push(TraceEvent::Variable {
                        name: "event_data".to_string(),
                        value: format!("[{}]", data.join(", ")),
                    });
                }
                events.push(TraceEvent::Return);
            }
            TraceEntry::Syscall {
                name,
                return_kind,
                return_value,
            } => {
                // Mirror the contract_call / storage_* shape: emit a
                // Step + Call + Variable(return) + Return triple.  The
                // syscall name is unprefixed because syscalls are global
                // to the StarkNet runtime — the dedicated Cairo
                // converter writer arm pairs each syscall with the
                // contract address it was invoked from.
                events.push(TraceEvent::Step { line });
                events.push(TraceEvent::Call { name: name.clone() });
                events.push(TraceEvent::Variable {
                    name: "return_kind".to_string(),
                    value: return_kind.clone(),
                });
                events.push(TraceEvent::Variable {
                    name: "return_value".to_string(),
                    value: return_value.clone(),
                });
                events.push(TraceEvent::Return);
            }
        }
        line += 1;
    }

    events
}

// ---------------------------------------------------------------------------
// Writing converted trace to CodeTracer files
// ---------------------------------------------------------------------------

/// Write converted snforge trace entries to CodeTracer output files.
///
/// Creates the canonical CTFS multi-stream `.ct` container plus
/// `trace_metadata.json` / `trace_paths.json` sidecars in `out_dir`,
/// mirroring the output of the `record` subcommand.
///
/// The recorder is CTFS-only — see `Recorder-CLI-Conventions.md` §4 in
/// `codetracer-specs`.  Use `ct print` from `codetracer-trace-format-nim`
/// to convert the produced bundle to JSON or other text forms.
pub fn write_starknet_trace(
    trace_path: &Path,
    entries: &[TraceEntry],
    out_dir: &Path,
) -> Result<()> {
    let program_str = trace_path.to_string_lossy();
    // CTFS-only.  Pre-2026-05-08 this function took a format parameter
    // and the CLI exposed `--format ctfs|binary|json`.  The convention
    // now mandates CTFS exclusively for all recorders.
    let format = TraceEventsFileFormat::Ctfs;
    let mut writer = create_trace_writer(&program_str, &[], format);

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // CTFS multi-stream container.
    let events_filename = "trace.ctfs";
    let events_path = out_dir.join(events_filename);
    let metadata_path = out_dir.join("trace_metadata.json");
    let paths_path = out_dir.join("trace_paths.json");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_metadata(&mut *writer, &metadata_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_paths(&mut *writer, &paths_path).map_err(|e| eyre!("{e}"))?;

    TraceWriter::start(&mut *writer, trace_path, Line(1));

    let str_type_id = TraceWriter::ensure_type_id(&mut *writer, TypeKind::String, "felt252");

    // Recover the trace's `contract_address` field — used as the
    // qualifying prefix for syscall function names so consumers can
    // group all of a contract's syscalls under one identity.  Failure
    // to re-parse falls back to an empty prefix (the syscall function
    // name then surfaces unprefixed) — the test pin still holds, just
    // without the contract-address grouping.
    let entry_contract: String = std::fs::read_to_string(trace_path)
        .ok()
        .and_then(|s| serde_json::from_str::<SnforgeTrace>(&s).ok())
        .map(|t| t.contract_address)
        .unwrap_or_default();

    // Walk entries directly so we can:
    //   - stage call args via TraceWriter::arg(name, value) before
    //     register_call (audit (b) — same pattern as Move 1.46 / Cardano 1.48).
    //   - route `Event` entries through register_special_event with
    //     EventLogKind::EvmEvent (audit (c) — same pattern as EVM 1.39).
    //
    // The legacy convert_snforge_trace() path is preserved for unit-test
    // backward compatibility but is no longer the source of truth for
    // writing; it remains exposed for data-conversion fixtures.
    let mut line: u32 = 1;
    for entry in entries {
        match entry {
            TraceEntry::ContractCall {
                caller,
                callee,
                selector,
                calldata,
                visibility,
                self_kind,
                dispatcher_trait,
            } => {
                TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
                // M10 round-4 / round-5: choose the function table
                // name based on which optional decorator field is
                // present.  `dispatcher_trait` (round-5) wins over
                // `visibility` (round-4) when both are supplied —
                // dispatcher-mediated calls cross trait boundaries
                // and the trait identity is the more specific
                // grouping.  Absent → preserve the legacy
                // `<callee>::<selector>` form so existing fixtures
                // keep their pinned function-table strings.
                let name = if !dispatcher_trait.is_empty() {
                    format!("{}::{}::{}", dispatcher_trait, callee, selector)
                } else if !visibility.is_empty() {
                    format!("{}::{}::{}", visibility, callee, selector)
                } else {
                    format!("{}::{}", callee, selector)
                };
                let fn_id =
                    TraceWriter::ensure_function_id(&mut *writer, &name, trace_path, Line(1));

                // Stage caller / callee / selector / each calldata felt as
                // canonical call args.  These were previously emitted as
                // scoped Variables, which surfaces them in the locals pane
                // but not on CallRecord.args (audit (b)).
                let _ = TraceWriter::arg(&mut *writer, "caller", str_value(caller, str_type_id));
                let _ = TraceWriter::arg(&mut *writer, "callee", str_value(callee, str_type_id));
                let _ =
                    TraceWriter::arg(&mut *writer, "selector", str_value(selector, str_type_id));
                for (idx, item) in calldata.iter().enumerate() {
                    let _ = TraceWriter::arg(
                        &mut *writer,
                        &format!("calldata{idx}"),
                        str_value(item, str_type_id),
                    );
                }
                // M10 round-4: when the JSON entry carries a
                // `self_kind`, emit a typed `ValueRecord::Reference`
                // arg distinguishing `ref self` (mutable=true) from
                // `@self` (mutable=false).  Absent → no `self_kind`
                // arg is emitted, preserving the legacy contract for
                // fixtures that don't care about the @-vs-ref split.
                if !self_kind.is_empty() {
                    let mutable = self_kind == "ref";
                    let ref_type_id =
                        TraceWriter::ensure_type_id(&mut *writer, TypeKind::Ref, "Self");
                    let value = ValueRecord::Reference {
                        dereferenced: Box::new(NONE_VALUE),
                        address: 0,
                        mutable,
                        type_id: ref_type_id,
                    };
                    let _ = TraceWriter::arg(&mut *writer, "self_kind", value);
                }
                // M10 round-5: when the JSON entry carries a
                // `dispatcher_trait`, surface the trait identity as
                // a typed `ValueRecord::String` arg so consumers can
                // group all dispatcher-mediated calls under the
                // trait name without re-parsing the function-table
                // string.  Absent → no `dispatcher_trait` arg is
                // emitted.
                if !dispatcher_trait.is_empty() {
                    let _ = TraceWriter::arg(
                        &mut *writer,
                        "dispatcher_trait",
                        str_value(dispatcher_trait, str_type_id),
                    );
                }

                TraceWriter::register_call(&mut *writer, fn_id, vec![]);
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
            }
            TraceEntry::StorageRead {
                contract,
                key,
                value,
            } => {
                TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
                let name = format!("{}::storage_read", contract);
                let fn_id =
                    TraceWriter::ensure_function_id(&mut *writer, &name, trace_path, Line(1));
                let _ = TraceWriter::arg(&mut *writer, "key", str_value(key, str_type_id));
                let _ = TraceWriter::arg(&mut *writer, "value", str_value(value, str_type_id));
                TraceWriter::register_call(&mut *writer, fn_id, vec![]);
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
                // Surface the storage op as a canonical io_event so the
                // event log reflects the on-chain state read.  The
                // metadata is the bare op tag ("StorageRead") and the
                // content carries `<contract>:<key>=<value>`.  This pairs
                // with the corresponding StorageWrite arm below — both
                // route through register_special_event with the
                // canonical EventLogKind::Read / Write tags so frontend
                // consumers can render storage I/O alongside contract
                // events without re-parsing the call frame.
                let metadata = "StorageRead";
                let content = format!("{contract}:{key}={value}");
                TraceWriter::register_special_event(
                    &mut *writer,
                    EventLogKind::Read,
                    metadata,
                    &content,
                );
            }
            TraceEntry::StorageWrite {
                contract,
                key,
                old_value,
                new_value,
            } => {
                TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
                let name = format!("{}::storage_write", contract);
                let fn_id =
                    TraceWriter::ensure_function_id(&mut *writer, &name, trace_path, Line(1));
                let _ = TraceWriter::arg(&mut *writer, "key", str_value(key, str_type_id));
                let _ =
                    TraceWriter::arg(&mut *writer, "old_value", str_value(old_value, str_type_id));
                let _ =
                    TraceWriter::arg(&mut *writer, "new_value", str_value(new_value, str_type_id));
                TraceWriter::register_call(&mut *writer, fn_id, vec![]);
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
                // Mirror the StorageRead arm: emit a Write-kinded
                // special event whose content captures both the previous
                // and new felt values so a downstream consumer can
                // diff the storage slot without rebuilding state.
                let metadata = "StorageWrite";
                let content = format!("{contract}:{key}={old_value}->{new_value}");
                TraceWriter::register_special_event(
                    &mut *writer,
                    EventLogKind::Write,
                    metadata,
                    &content,
                );
            }
            TraceEntry::Syscall {
                name,
                return_kind,
                return_value,
            } => {
                // M10 round-3: each syscall surfaces as a Call/Return
                // pair named `<contract>::<syscall_name>` (the
                // contract-address prefix matches the storage_read /
                // storage_write naming so consumers can group all of
                // a call's effects under one contract id).  The
                // syscall return value rides on the call_exit event
                // as a typed `ValueRecord` whose shape depends on
                // `return_kind`:
                //
                //   * `"address"` — `ValueRecord::Raw` carrying the
                //     `0x`-prefixed hex string (zero-padded to 64
                //     chars / 32 bytes) so consumers can recover the
                //     raw 256-bit address without parsing.
                //   * `"u64"`     — `ValueRecord::Int` against a
                //     dedicated `u64` type id; the value is parsed
                //     from the JSON's decimal / `0x`-prefixed string.
                //   * `"bool"`    — `ValueRecord::Bool` against a
                //     dedicated `bool` type id; accepted JSON values
                //     are `"true"` / `"false"` (case-insensitive) and
                //     `"1"` / `"0"`.  Used by signature-verification
                //     syscalls (e.g. `check_ecdsa_signature`) whose
                //     return is a pure boolean.
                //
                // Anything else falls back to a `ValueRecord::String`
                // so the trace stays self-describing.
                TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
                let qualified = if entry_contract.is_empty() {
                    name.clone()
                } else {
                    format!("{entry_contract}::{name}")
                };
                let fn_id =
                    TraceWriter::ensure_function_id(&mut *writer, &qualified, trace_path, Line(1));
                TraceWriter::register_call(&mut *writer, fn_id, vec![]);
                let return_record = match return_kind.as_str() {
                    "address" => {
                        // Pad the hex string to 64 chars (32 bytes) so
                        // consumers always see the canonical
                        // 256-bit-wide raw form regardless of leading
                        // zeros in the input JSON.
                        let bare = return_value.strip_prefix("0x").unwrap_or(return_value);
                        let padded = format!("0x{:0>64}", bare);
                        let raw_id =
                            TraceWriter::ensure_type_id(&mut *writer, TypeKind::Raw, "Address");
                        ValueRecord::Raw {
                            r: padded,
                            type_id: raw_id,
                        }
                    }
                    "u64" => {
                        let int_id =
                            TraceWriter::ensure_type_id(&mut *writer, TypeKind::Int, "u64");
                        let parsed = if let Some(hex) = return_value.strip_prefix("0x") {
                            i64::from_str_radix(hex, 16).unwrap_or(0)
                        } else {
                            return_value.parse::<i64>().unwrap_or(0)
                        };
                        ValueRecord::Int {
                            i: parsed,
                            type_id: int_id,
                        }
                    }
                    "bool" => {
                        let bool_id =
                            TraceWriter::ensure_type_id(&mut *writer, TypeKind::Bool, "bool");
                        // Accept `"true"` / `"false"` (case-insensitive)
                        // and `"1"` / `"0"`.  Anything else parses as
                        // false — the JSON producer is expected to use
                        // one of the canonical forms.
                        let b = matches!(return_value.to_ascii_lowercase().as_str(), "true" | "1");
                        ValueRecord::Bool {
                            b,
                            type_id: bool_id,
                        }
                    }
                    _ => str_value(return_value, str_type_id),
                };
                TraceWriter::register_return(&mut *writer, return_record);
            }
            TraceEntry::Event {
                contract,
                keys,
                data,
            } => {
                // Starknet contract-emitted log events are structured
                // (keys, data) records — analogous to EVM LOG opcodes.
                // Route them through register_special_event with the
                // canonical EventLogKind::EvmEvent kind so the frontend
                // event-log pane surfaces them as structured log records
                // rather than synthetic stdout text.  This mirrors the
                // EVM (1.39) routing for LOG-style events.
                //
                // M10 round-2: the multi-stream writer collapses the
                // wider EventLogKind enum down to a 3-way IOEventKind
                // and drops the `metadata` argument on the floor.  To
                // make the canonical `StarknetEvent:<contract>` tag
                // and the (`#[key]`-)indexed-vs-data distinction
                // recoverable from the io_event payload alone, we
                // embed the metadata in the content string itself —
                // every consumer reads `text`, so this keeps the tag
                // accessible without re-introducing the dropped
                // metadata argument plumbing.
                TraceWriter::register_step(&mut *writer, trace_path, Line(line as i64));
                let metadata = format!("StarknetEvent:{contract}");
                let content = format!(
                    "{metadata} keys=[{}] data=[{}]",
                    keys.join(", "),
                    data.join(", "),
                );
                TraceWriter::register_special_event(
                    &mut *writer,
                    EventLogKind::EvmEvent,
                    &metadata,
                    &content,
                );
            }
        }
        line += 1;
    }

    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_metadata(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_paths(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// On-chain transaction replay infrastructure (M5)
// ---------------------------------------------------------------------------

/// Client for communicating with a StarkNet JSON-RPC node.
///
/// Used to fetch transaction traces via `starknet_traceTransaction`.
#[derive(Debug, Clone)]
pub struct StarknetRpcClient {
    /// The URL of the StarkNet JSON-RPC endpoint (e.g. `https://free-rpc.nethermind.io/mainnet-juno`).
    pub rpc_url: String,
}

impl StarknetRpcClient {
    /// Create a new RPC client pointing at the given endpoint.
    pub fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
        }
    }

    /// Fetch a transaction trace from the StarkNet node.
    ///
    /// Calls `starknet_traceTransaction` via JSON-RPC over HTTP using
    /// [`ureq`] (blocking, no async runtime).  The response is parsed
    /// into a [`TransactionTrace`] via [`TransactionTrace::from_json`]
    /// so the live and offline (`--trace-file`) paths share the same
    /// JSON schema contract.
    ///
    /// `tx_hash` is forwarded as the single positional parameter of the
    /// JSON-RPC `params` array — matching the
    /// [StarkNet JSON-RPC trace API](https://github.com/starkware-libs/starknet-specs).
    pub fn trace_transaction(&self, tx_hash: &str) -> Result<TransactionTrace> {
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "starknet_traceTransaction",
            "params": [tx_hash],
        });

        let response = ureq::post(&self.rpc_url)
            .set("Content-Type", "application/json")
            .send_json(&request_body)
            .map_err(|e| {
                eyre!(
                    "starknet_traceTransaction request to {} failed: {e}",
                    self.rpc_url
                )
            })?;

        let body = response
            .into_string()
            .with_context(|| format!("failed to read RPC response body from {}", self.rpc_url))?;

        // Surface JSON-RPC errors directly so callers see the upstream
        // node's error code / message rather than a generic parse
        // failure.
        if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(err) = envelope.get("error") {
                return Err(eyre!(
                    "starknet_traceTransaction returned JSON-RPC error: {err}"
                ));
            }
        }

        TransactionTrace::from_json(&body)
    }

    /// Fetch the [`ContractClass`] currently deployed at `contract_address`
    /// at the given `block_id` (e.g. `"latest"`, `"pending"`, or a
    /// `{ "block_number": N }` / `{ "block_hash": "0x..." }` JSON
    /// object) via `starknet_getClassAt`.
    ///
    /// Only Cairo 1+ Sierra classes are supported — the JSON-RPC
    /// response for a Cairo 0 deprecated class lacks the
    /// `sierra_program` field and will fail to deserialise into
    /// [`SierraContractClass`].  This matches the recorder's stance
    /// that the local re-execution path runs against Sierra (the only
    /// dialect [`SierraCasmRunner`] handles).
    pub fn get_class_at(
        &self,
        block_id: &serde_json::Value,
        contract_address: &str,
    ) -> Result<SierraContractClass> {
        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "starknet_getClassAt",
            "params": [block_id, contract_address],
        });

        let response = ureq::post(&self.rpc_url)
            .set("Content-Type", "application/json")
            .send_json(&request_body)
            .map_err(|e| {
                eyre!(
                    "starknet_getClassAt request to {} failed: {e}",
                    self.rpc_url
                )
            })?;

        let body = response
            .into_string()
            .with_context(|| format!("failed to read RPC response body from {}", self.rpc_url))?;

        let envelope: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| "failed to parse starknet_getClassAt response as JSON")?;

        if let Some(err) = envelope.get("error") {
            return Err(eyre!("starknet_getClassAt returned JSON-RPC error: {err}"));
        }

        let result = envelope
            .get("result")
            .ok_or_else(|| eyre!("starknet_getClassAt response missing 'result' field"))?;

        // Reject Cairo 0 deprecated classes early: they carry a
        // `program` field instead of `sierra_program` and the Sierra
        // re-execution path cannot consume them.
        if result.get("program").is_some() && result.get("sierra_program").is_none() {
            return Err(eyre!(
                "contract at {contract_address} is a Cairo 0 deprecated class — \
                 the recorder's local re-execution path supports Sierra (Cairo 1+) only"
            ));
        }

        serde_json::from_value::<SierraContractClass>(result.clone())
            .with_context(|| "failed to parse starknet_getClassAt result as Sierra ContractClass")
    }
}

/// Re-export of the upstream Cairo Sierra `ContractClass` type so
/// downstream callers don't have to depend on
/// `cairo-lang-starknet-classes` directly.
pub type SierraContractClass = cairo_lang_starknet_classes::contract_class::ContractClass;

/// Represents the execution invocation within a StarkNet transaction trace,
/// corresponding to the `execute_invocation` field from `starknet_traceTransaction`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationTrace {
    pub contract_address: String,
    pub entry_point_selector: String,
    #[serde(default)]
    pub calldata: Vec<String>,
    #[serde(default)]
    pub caller_address: String,
    #[serde(default)]
    pub result: Vec<String>,
    #[serde(default)]
    pub calls: Vec<InvocationTrace>,
    #[serde(default)]
    pub events: Vec<InvocationEvent>,
    #[serde(default)]
    pub messages: Vec<serde_json::Value>,
}

/// An event emitted during an invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationEvent {
    pub keys: Vec<String>,
    pub data: Vec<String>,
}

/// A single storage entry within a state diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEntry {
    pub key: String,
    pub value: String,
}

/// Storage diffs for a single contract address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDiff {
    pub address: String,
    pub storage_entries: Vec<StorageEntry>,
}

/// Nonce update for a contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NonceUpdate {
    pub contract_address: String,
    pub nonce: String,
}

/// The state diff produced by executing a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDiff {
    #[serde(default)]
    pub storage_diffs: Vec<StorageDiff>,
    #[serde(default)]
    pub nonces: Vec<NonceUpdate>,
    #[serde(default)]
    pub deployed_contracts: Vec<serde_json::Value>,
    #[serde(default)]
    pub deprecated_declared_classes: Vec<serde_json::Value>,
    #[serde(default)]
    pub declared_classes: Vec<serde_json::Value>,
    #[serde(default)]
    pub replaced_classes: Vec<serde_json::Value>,
}

/// A StarkNet transaction trace as returned by `starknet_traceTransaction`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionTrace {
    /// The transaction type (e.g. "INVOKE", "DEPLOY_ACCOUNT", "DECLARE").
    #[serde(rename = "type")]
    pub tx_type: String,

    /// The top-level execution invocation.
    pub execute_invocation: InvocationTrace,

    /// State changes produced by the transaction.
    #[serde(default)]
    pub state_diff: Option<StateDiff>,
}

impl TransactionTrace {
    /// Parse a [`TransactionTrace`] from a JSON string.
    ///
    /// Accepts either the bare result object or a full JSON-RPC response
    /// envelope (with `{ "result": ... }`).
    pub fn from_json(json: &str) -> Result<Self> {
        // Try parsing as a full JSON-RPC response first.
        #[derive(Deserialize)]
        struct RpcResponse {
            result: TransactionTrace,
        }

        if let Ok(resp) = serde_json::from_str::<RpcResponse>(json) {
            return Ok(resp.result);
        }

        // Fall back to parsing the bare object.
        serde_json::from_str(json).with_context(|| "failed to parse TransactionTrace JSON")
    }
}

/// Configuration for replaying a StarkNet transaction.
#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// Transaction hash to replay.
    pub tx_hash: String,

    /// StarkNet JSON-RPC endpoint URL.
    pub rpc_url: String,

    /// Optional directory containing contract source code for source-level
    /// tracing. When `None`, the replay will still produce a trace but
    /// without source-line mapping.
    pub source_dir: Option<PathBuf>,
}

/// The execution context reconstructed from a transaction trace.
///
/// Contains all the information needed to re-execute the transaction locally
/// with tracing enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionContext {
    /// The contract address that was called.
    pub contract_address: String,

    /// The entry point selector (function identifier).
    pub entry_point_selector: String,

    /// The calldata passed to the entry point.
    pub calldata: Vec<String>,

    /// Storage state extracted from the transaction's state diff.
    /// Each entry is `(contract_address, key, value)`.
    pub storage_state: Vec<(String, String, String)>,
}

/// Reconstruct an [`ExecutionContext`] from a [`TransactionTrace`].
///
/// Extracts the contract address, entry point selector, calldata from the
/// top-level `execute_invocation`, and storage state from the `state_diff`.
pub fn reconstruct_execution_context(tx: &TransactionTrace) -> ExecutionContext {
    let invocation = &tx.execute_invocation;

    let mut storage_state = Vec::new();
    if let Some(ref diff) = tx.state_diff {
        for storage_diff in &diff.storage_diffs {
            for entry in &storage_diff.storage_entries {
                storage_state.push((
                    storage_diff.address.clone(),
                    entry.key.clone(),
                    entry.value.clone(),
                ));
            }
        }
    }

    ExecutionContext {
        contract_address: invocation.contract_address.clone(),
        entry_point_selector: invocation.entry_point_selector.clone(),
        calldata: invocation.calldata.clone(),
        storage_state,
    }
}

/// Entry point for the `replay` CLI subcommand.
///
/// Fetches a transaction trace from a StarkNet node and reconstructs the
/// execution context. The actual re-execution with tracing is a placeholder
/// for now — it requires a running node and contract artifacts.
pub fn replay_transaction(config: &ReplayConfig) -> Result<ExecutionContext> {
    let client = StarknetRpcClient::new(&config.rpc_url);

    eprintln!("Fetching trace for tx {} ...", config.tx_hash);
    let trace = client.trace_transaction(&config.tx_hash)?;

    let ctx = reconstruct_execution_context(&trace);

    eprintln!("Reconstructed execution context:");
    eprintln!("  contract: {}", ctx.contract_address);
    eprintln!("  selector: {}", ctx.entry_point_selector);
    eprintln!("  calldata: {:?}", ctx.calldata);
    eprintln!("  storage entries: {}", ctx.storage_state.len());

    if let Some(ref src) = config.source_dir {
        eprintln!("  source dir: {}", src.display());
    }

    Ok(ctx)
}

// ---------------------------------------------------------------------------
// M5: write a CTFS trace bundle from a fetched on-chain transaction trace.
// ---------------------------------------------------------------------------
//
// The M5 vision is "fetch on-chain tx → re-execute locally with
// CodeTracer tracing".  Three components were originally outstanding:
//
//   1. Class-hash resolution: given a contract address, call
//      `starknet_getClassAt` to fetch the Sierra (or deprecated Cairo 0)
//      compiled class.  *Implemented* via [`StarknetRpcClient::get_class_at`]
//      using `ureq` (blocking JSON-RPC).
//   2. Block-context reconstruction: build a `BlockContext` carrying the
//      original tx's block number, timestamp, gas price, and caller
//      address.  *Still partial*: the on-chain
//      `starknet_traceTransaction` response does not surface block-level
//      fields, so the local re-execution path runs against the runner's
//      default Starknet state (sufficient for stateless re-execution of
//      pure functions, lossy for state-dependent calls).
//   3. Local execution via `SierraCasmRunner` against the fetched class
//      with the original calldata + preloaded storage state.
//      *Implemented* via [`reexecute_entry_point`] which extracts the
//      Sierra program from the fetched class, looks up the entry point
//      by selector, and invokes the runner with the calldata felts.
//      The runner's `relocated_trace` is not yet routed back through
//      the `tracer.rs` step/call pipeline — wiring that would unlock
//      per-Sierra-instruction granularity in the produced CTFS bundle.
//      The current path keeps the per-invocation granularity from the
//      on-chain trace JSON and additionally validates that the fetched
//      class can be locally re-invoked end-to-end without panicking.
//
// [`write_replay_trace`] (below) walks the already-fetched
// [`TransactionTrace`] (call tree, calldata, events, storage diffs) and
// emits a CodeTracer CTFS bundle directly from that — the same on-disk
// shape as `record` / `trace-starknet`.  This is end-to-end useful
// today: a user can record a real on-chain tx and load the resulting
// `.ct` in CodeTracer without a Cairo toolchain.
//
// TODO(M5-followup-2): route the SierraCasmRunner `relocated_trace`
// through the existing `CairoTracer` step/call/return pipeline so the
// produced CTFS bundle gains per-Sierra-instruction granularity.  The
// current [`reexecute_entry_point`] runs the entry point but discards
// the relocated trace — a follow-up should diff the runner's memory /
// trace_entries against the source map (`source_map.rs`) and emit
// matching `register_step` / `register_call` / `register_return`
// records via [`TraceWriter`].

/// Walk a [`TransactionTrace`]'s invocation tree (DFS) and emit a CTFS
/// trace bundle into `out_dir`.
///
/// The bundle is written as the canonical multi-stream `.ct` container
/// plus `trace_metadata.json` / `trace_paths.json` sidecars, mirroring
/// the output of the `record` and `trace-starknet` subcommands.
///
/// Each invocation in the tree produces a Call/Return frame named
/// `<contract_address>::<entry_point_selector>`, with calldata felts
/// staged as canonical call args (`calldata0`, `calldata1`, …) and one
/// `Step` event per invocation.  Nested `calls` are walked recursively
/// so the on-chain call depth is preserved in the trace.
///
/// Storage diffs from the transaction's `state_diff` surface as
/// `EventLogKind::Write` special events at the top level after the
/// invocation walk — they are post-tx state, not per-call effects, so
/// pinning them to the post-walk position keeps the call-tree shape
/// faithful to the on-chain ordering.
///
/// Emitted events from each invocation surface as
/// `EventLogKind::EvmEvent` special events inside the invocation's
/// frame (mirroring the `write_starknet_trace` contract for the
/// snforge `Event` arm).
///
/// `tx_hash` is recorded in the trace's path name so the output bundle
/// is identifiable as belonging to a specific on-chain transaction.
pub fn write_replay_trace(tx_hash: &str, trace: &TransactionTrace, out_dir: &Path) -> Result<()> {
    // Use the tx hash as the synthetic "program path" — there is no
    // local source file for an on-chain transaction.  Downstream
    // consumers display this in the trace metadata, and it gives the
    // bundle a stable identity tied to the tx itself.
    let synthetic_path = PathBuf::from(format!("starknet-tx://{}", tx_hash));

    let format = TraceEventsFileFormat::Ctfs;
    let program_str = synthetic_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_str, &[], format);

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    let events_path = out_dir.join("trace.ctfs");
    let metadata_path = out_dir.join("trace_metadata.json");
    let paths_path = out_dir.join("trace_paths.json");

    TraceWriter::begin_writing_trace_events(&mut *writer, &events_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_metadata(&mut *writer, &metadata_path)
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::begin_writing_trace_paths(&mut *writer, &paths_path).map_err(|e| eyre!("{e}"))?;

    TraceWriter::start(&mut *writer, &synthetic_path, Line(1));

    let str_type_id = TraceWriter::ensure_type_id(&mut *writer, TypeKind::String, "felt252");

    // Walk the invocation tree DFS, emitting a Call/Step/Return per
    // invocation.  `line` is a synthetic counter assigned in invocation
    // visit order — the invocation tree is the only structure we have
    // (no source-line mapping for an on-chain tx).
    let mut line_counter: u32 = 1;
    write_invocation(
        &mut *writer,
        &synthetic_path,
        &trace.execute_invocation,
        str_type_id,
        &mut line_counter,
    );

    // Storage diffs are post-tx state — emit them as Write special
    // events after the invocation walk so the call tree stays clean.
    if let Some(ref state_diff) = trace.state_diff {
        for storage_diff in &state_diff.storage_diffs {
            for entry in &storage_diff.storage_entries {
                let metadata = "StorageWrite";
                let content = format!("{}:{}={}", storage_diff.address, entry.key, entry.value);
                TraceWriter::register_special_event(
                    &mut *writer,
                    EventLogKind::Write,
                    metadata,
                    &content,
                );
            }
        }
    }

    TraceWriter::finish_writing_trace_events(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_metadata(&mut *writer).map_err(|e| eyre!("{e}"))?;
    TraceWriter::finish_writing_trace_paths(&mut *writer).map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;

    Ok(())
}

/// DFS helper for `write_replay_trace`: emit a Call/Step/Return frame
/// for one invocation, then recurse into its nested `calls`.
///
/// The Call frame's name is `<contract>::<selector>`, calldata felts
/// are staged as `calldata{N}` args, and the invocation's emitted
/// events surface as `EventLogKind::EvmEvent` special events inside
/// the frame so the on-chain call → event ordering is preserved.
fn write_invocation(
    writer: &mut dyn TraceWriter,
    synthetic_path: &Path,
    invocation: &InvocationTrace,
    str_type_id: codetracer_trace_types::TypeId,
    line_counter: &mut u32,
) {
    let line = *line_counter;
    *line_counter += 1;

    TraceWriter::register_step(writer, synthetic_path, Line(line as i64));

    let name = format!(
        "{}::{}",
        invocation.contract_address, invocation.entry_point_selector
    );
    let fn_id = TraceWriter::ensure_function_id(writer, &name, synthetic_path, Line(1));

    // Stage caller / contract / selector / each calldata felt as args.
    let _ = TraceWriter::arg(
        writer,
        "caller",
        str_value(&invocation.caller_address, str_type_id),
    );
    let _ = TraceWriter::arg(
        writer,
        "contract",
        str_value(&invocation.contract_address, str_type_id),
    );
    let _ = TraceWriter::arg(
        writer,
        "selector",
        str_value(&invocation.entry_point_selector, str_type_id),
    );
    for (idx, item) in invocation.calldata.iter().enumerate() {
        let _ = TraceWriter::arg(
            writer,
            &format!("calldata{idx}"),
            str_value(item, str_type_id),
        );
    }

    TraceWriter::register_call(writer, fn_id, vec![]);

    // Surface the invocation's emitted events inside the frame so the
    // call → event ordering matches the on-chain trace.
    for event in &invocation.events {
        let metadata = format!("StarknetEvent:{}", invocation.contract_address);
        let content = format!(
            "{metadata} keys=[{}] data=[{}]",
            event.keys.join(", "),
            event.data.join(", "),
        );
        TraceWriter::register_special_event(writer, EventLogKind::EvmEvent, &metadata, &content);
    }

    // Recurse into nested invocations BEFORE the return — this preserves
    // on-chain call depth in the trace's call/return nesting.
    for nested in &invocation.calls {
        write_invocation(writer, synthetic_path, nested, str_type_id, line_counter);
    }

    // Surface the result felts as a string-typed return record so the
    // top-of-call-stack value is recoverable.  Multiple felts collapse
    // into a single comma-joined string — the typed multi-felt encoding
    // would require a Sequence type registered against felt252, which
    // is overkill for the M5 minimum.
    let return_record = if invocation.result.is_empty() {
        NONE_VALUE
    } else {
        str_value(&invocation.result.join(","), str_type_id)
    };
    TraceWriter::register_return(writer, return_record);
}

/// Count the total number of invocations in a [`TransactionTrace`]'s
/// call tree (DFS).  Exposed for test-pinning the Call/Step/Return
/// counts emitted by [`write_replay_trace`] without re-walking the
/// invocation tree in test code.
pub fn count_invocations(trace: &TransactionTrace) -> usize {
    fn walk(invocation: &InvocationTrace) -> usize {
        let mut total = 1;
        for nested in &invocation.calls {
            total += walk(nested);
        }
        total
    }
    walk(&trace.execute_invocation)
}

/// Count the total number of emitted events across every invocation
/// in a [`TransactionTrace`]'s call tree (DFS).  Exposed for test
/// pinning of the per-invocation `EventLogKind::EvmEvent` count.
pub fn count_events(trace: &TransactionTrace) -> usize {
    fn walk(invocation: &InvocationTrace) -> usize {
        let mut total = invocation.events.len();
        for nested in &invocation.calls {
            total += walk(nested);
        }
        total
    }
    walk(&trace.execute_invocation)
}

/// Count the total number of storage-diff entries across every
/// contract in a [`TransactionTrace`]'s state diff.  Exposed for test
/// pinning of the post-walk `EventLogKind::Write` count.
pub fn count_storage_entries(trace: &TransactionTrace) -> usize {
    let Some(ref state_diff) = trace.state_diff else {
        return 0;
    };
    state_diff
        .storage_diffs
        .iter()
        .map(|d| d.storage_entries.len())
        .sum()
}

// ---------------------------------------------------------------------------
// M5 follow-up: local re-execution of a fetched Sierra contract class.
// ---------------------------------------------------------------------------

/// Outcome of locally re-executing a Starknet entry point against a
/// fetched [`SierraContractClass`].
///
/// `gas_counter` mirrors the runner's post-execution gas value; `value`
/// is the entry point's [`RunResultValue`] (Success / Panic) carrying
/// the returned felt vector or panic payload.  These two together pin
/// the runner-side outcome the integration test asserts against.
#[derive(Debug, Clone)]
pub struct ReexecutionResult {
    /// Remaining gas reported by [`SierraCasmRunner`] after the entry
    /// point returned.  `None` when the runner did not track gas.
    pub gas_counter: Option<starknet_types_core::felt::Felt>,
    /// The entry point's return value, as classified by the runner.
    pub value: cairo_lang_runner::RunResultValue,
}

/// Locate the [`ContractEntryPoint`] in `class.entry_points_by_type`
/// matching `selector`.
///
/// Searches `external`, `l1_handler`, then `constructor` in that
/// order.  `selector` is accepted as a `0x`-prefixed (or bare) hex
/// string — matching the on-chain `entry_point_selector` form
/// surfaced by `starknet_traceTransaction`.
pub fn find_entry_point<'a>(
    class: &'a SierraContractClass,
    selector: &str,
) -> Result<&'a cairo_lang_starknet_classes::contract_class::ContractEntryPoint> {
    let bare = selector.strip_prefix("0x").unwrap_or(selector);
    let needle = num_bigint::BigUint::parse_bytes(bare.as_bytes(), 16)
        .ok_or_else(|| eyre!("entry-point selector is not a valid hex string: {selector}"))?;

    let groups = [
        ("external", &class.entry_points_by_type.external),
        ("l1_handler", &class.entry_points_by_type.l1_handler),
        ("constructor", &class.entry_points_by_type.constructor),
    ];

    for (_kind, group) in groups {
        if let Some(ep) = group.iter().find(|ep| ep.selector == needle) {
            return Ok(ep);
        }
    }

    Err(eyre!(
        "no entry point with selector {selector} found in class \
         (external: {}, l1_handler: {}, constructor: {})",
        class.entry_points_by_type.external.len(),
        class.entry_points_by_type.l1_handler.len(),
        class.entry_points_by_type.constructor.len(),
    ))
}

/// Re-execute the entry point identified by `(class, selector)` locally
/// using [`SierraCasmRunner`], passing `calldata` as the entry point's
/// felt252 arguments.
///
/// Steps:
///   1. Extract the Sierra program from the fetched contract class via
///      [`SierraContractClass::extract_sierra_program`] (with debug
///      info so function names round-trip back to source).
///   2. Build a [`SierraCasmRunner`] with default Starknet contracts
///      info / no profiler.
///   3. Look up the entry point in the program by `function_idx`
///      (resolved from the selector against
///      [`ContractEntryPoints`]).
///   4. Convert each calldata felt (accepted as `0x`-prefixed or bare
///      decimal) into a [`cairo_lang_runner::Arg`] and invoke
///      [`SierraCasmRunner::run_function_with_starknet_context`] with
///      a fresh [`StarknetState`].
///
/// **Limitations** (tracked in the M5 module-level comment):
///   - The runner runs against an empty Starknet state; storage reads
///     issued by the entry point will see default-zero values.  Pre-
///     loading the trace's `state_diff` into the runner's
///     `StarknetState` is the next milestone.
///   - The returned [`ReexecutionResult`] discards the runner's
///     `relocated_trace`.  Routing it back through the
///     `tracer.rs` step/call pipeline (so the produced CTFS bundle
///     gains per-Sierra-instruction granularity) is the M5 follow-up
///     2 task documented above.
pub fn reexecute_entry_point(
    class: &SierraContractClass,
    selector: &str,
    calldata: &[String],
) -> Result<ReexecutionResult> {
    use cairo_lang_runner::{Arg, SierraCasmRunner, StarknetState};
    use cairo_lang_utils::ordered_hash_map::OrderedHashMap;

    let entry_point = find_entry_point(class, selector)?;
    let function_idx = entry_point.function_idx;

    let extracted = class
        .extract_sierra_program(true)
        .map_err(|e| eyre!("failed to extract Sierra program from class: {e:?}"))?;

    let function = extracted
        .program
        .funcs
        .get(function_idx)
        .ok_or_else(|| {
            eyre!(
                "entry-point function_idx {function_idx} out of range \
                 (program has {} functions)",
                extracted.program.funcs.len()
            )
        })?
        .clone();

    let runner = SierraCasmRunner::new(extracted.program, None, OrderedHashMap::default(), None)
        .map_err(|e| eyre!("failed to build SierraCasmRunner: {e}"))?;

    let mut args: Vec<Arg> = Vec::with_capacity(calldata.len());
    for item in calldata {
        let felt =
            parse_felt(item).with_context(|| format!("failed to parse calldata felt: {item}"))?;
        args.push(Arg::Value(felt));
    }

    let result = runner
        .run_function_with_starknet_context(&function, args, None, StarknetState::default())
        .map_err(|e| eyre!("local entry-point re-execution failed: {e}"))?;

    Ok(ReexecutionResult {
        gas_counter: result.gas_counter,
        value: result.value,
    })
}

/// Parse a felt252 from either a `0x`-prefixed hex string or a bare
/// decimal string.  Used to convert on-chain calldata felts into
/// [`Felt252`] runner arguments.
fn parse_felt(s: &str) -> Result<starknet_types_core::felt::Felt> {
    use starknet_types_core::felt::Felt;
    if let Some(hex) = s.strip_prefix("0x") {
        Felt::from_hex(&format!("0x{hex}")).map_err(|e| eyre!("invalid hex felt {s}: {e}"))
    } else {
        Felt::from_dec_str(s).map_err(|e| eyre!("invalid decimal felt {s}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_entries() -> Vec<TraceEntry> {
        vec![
            TraceEntry::ContractCall {
                caller: "0x1".to_string(),
                callee: "0x2".to_string(),
                selector: "increase_balance".to_string(),
                calldata: vec!["42".to_string()],
                visibility: String::new(),
                self_kind: String::new(),
                dispatcher_trait: String::new(),
            },
            TraceEntry::StorageRead {
                contract: "0x2".to_string(),
                key: "balance".to_string(),
                value: "0".to_string(),
            },
            TraceEntry::StorageWrite {
                contract: "0x2".to_string(),
                key: "balance".to_string(),
                old_value: "0".to_string(),
                new_value: "42".to_string(),
            },
            TraceEntry::Event {
                contract: "0x2".to_string(),
                keys: vec!["BalanceIncreased".to_string()],
                data: vec!["0x1".to_string(), "42".to_string()],
            },
        ]
    }

    #[test]
    fn test_trace_entry_serialization_roundtrip() {
        let entries = sample_entries();
        let json = serde_json::to_string_pretty(&SnforgeTrace {
            test_name: "test_increase".to_string(),
            contract_address: "0x1".to_string(),
            entries: entries.clone(),
        })
        .unwrap();

        let parsed: SnforgeTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entries, entries);
    }

    #[test]
    fn test_parse_snforge_trace_from_file() {
        let trace_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-programs/starknet/mock_trace.json");
        let entries = parse_snforge_trace(&trace_path).unwrap();

        assert!(!entries.is_empty(), "mock trace should have entries");

        // First entry should be a contract call.
        assert!(
            matches!(&entries[0], TraceEntry::ContractCall { .. }),
            "first entry should be a ContractCall, got: {:?}",
            entries[0]
        );
    }

    #[test]
    fn test_convert_contract_call() {
        let entries = vec![TraceEntry::ContractCall {
            caller: "0x1".to_string(),
            callee: "0x2".to_string(),
            selector: "transfer".to_string(),
            calldata: vec!["0x3".to_string(), "100".to_string()],
            visibility: String::new(),
            self_kind: String::new(),
            dispatcher_trait: String::new(),
        }];

        let events = convert_snforge_trace(&entries);

        // Should produce: Step, Call, Variable(caller), Variable(callee),
        // Variable(selector), Variable(calldata), Return
        assert_eq!(events.len(), 7);
        assert!(matches!(events[0], TraceEvent::Step { line: 1 }));
        assert!(matches!(&events[1], TraceEvent::Call { name } if name == "0x2::transfer"));
        assert!(matches!(&events[2], TraceEvent::Variable { name, .. } if name == "caller"));
        assert!(matches!(&events[3], TraceEvent::Variable { name, .. } if name == "callee"));
        assert!(matches!(&events[4], TraceEvent::Variable { name, .. } if name == "selector"));
        assert!(matches!(&events[5], TraceEvent::Variable { name, value }
            if name == "calldata" && value == "[0x3, 100]"));
        assert!(matches!(events[6], TraceEvent::Return));
    }

    #[test]
    fn test_convert_storage_read() {
        let entries = vec![TraceEntry::StorageRead {
            contract: "0x2".to_string(),
            key: "balance".to_string(),
            value: "500".to_string(),
        }];

        let events = convert_snforge_trace(&entries);

        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], TraceEvent::Step { line: 1 }));
        assert!(matches!(&events[1], TraceEvent::Call { name } if name == "0x2::storage_read"));
        assert!(matches!(&events[2], TraceEvent::Variable { name, value }
            if name == "key" && value == "balance"));
        assert!(matches!(&events[3], TraceEvent::Variable { name, value }
            if name == "value" && value == "500"));
        assert!(matches!(events[4], TraceEvent::Return));
    }

    #[test]
    fn test_convert_storage_write() {
        let entries = vec![TraceEntry::StorageWrite {
            contract: "0x2".to_string(),
            key: "balance".to_string(),
            old_value: "500".to_string(),
            new_value: "600".to_string(),
        }];

        let events = convert_snforge_trace(&entries);

        assert_eq!(events.len(), 6);
        assert!(matches!(&events[1], TraceEvent::Call { name } if name == "0x2::storage_write"));
        assert!(matches!(&events[3], TraceEvent::Variable { name, value }
            if name == "old_value" && value == "500"));
        assert!(matches!(&events[4], TraceEvent::Variable { name, value }
            if name == "new_value" && value == "600"));
    }

    #[test]
    fn test_convert_event() {
        let entries = vec![TraceEntry::Event {
            contract: "0x2".to_string(),
            keys: vec!["Transfer".to_string()],
            data: vec!["0x1".to_string(), "0x3".to_string(), "100".to_string()],
        }];

        let events = convert_snforge_trace(&entries);

        assert_eq!(events.len(), 5);
        assert!(matches!(&events[1], TraceEvent::Call { name } if name == "0x2::emit_event"));
        assert!(matches!(&events[2], TraceEvent::Variable { name, value }
            if name == "event_keys" && value == "[Transfer]"));
        assert!(matches!(&events[3], TraceEvent::Variable { name, value }
            if name == "event_data" && value == "[0x1, 0x3, 100]"));
    }

    #[test]
    fn test_convert_full_trace() {
        let entries = sample_entries();
        let events = convert_snforge_trace(&entries);

        // Each entry produces at least Step + Call + Return (3 events minimum).
        assert!(
            events.len() >= 12,
            "full trace should produce at least 12 events, got {}",
            events.len()
        );

        // Verify line numbers increment.
        let step_lines: Vec<u32> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Step { line } => Some(*line),
                _ => None,
            })
            .collect();
        assert_eq!(step_lines, vec![1, 2, 3, 4]);

        // Verify all four entry types produced calls.
        let call_names: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Call { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(call_names.contains(&"0x2::increase_balance"));
        assert!(call_names.contains(&"0x2::storage_read"));
        assert!(call_names.contains(&"0x2::storage_write"));
        assert!(call_names.contains(&"0x2::emit_event"));
    }

    #[test]
    fn test_convert_empty_trace() {
        let events = convert_snforge_trace(&[]);
        assert!(events.is_empty());
    }

    #[test]
    fn test_contract_call_no_calldata() {
        let entries = vec![TraceEntry::ContractCall {
            caller: "0x1".to_string(),
            callee: "0x2".to_string(),
            selector: "get_balance".to_string(),
            calldata: vec![],
            visibility: String::new(),
            self_kind: String::new(),
            dispatcher_trait: String::new(),
        }];

        let events = convert_snforge_trace(&entries);
        // Without calldata: Step, Call, caller, callee, selector, Return (6 events)
        assert_eq!(events.len(), 6);
    }

    // -----------------------------------------------------------------------
    // On-chain replay tests (M5)
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_mock_tx_trace() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-programs/starknet/mock_tx_trace.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let trace = TransactionTrace::from_json(&content).unwrap();

        assert_eq!(trace.tx_type, "INVOKE");
        assert_eq!(
            trace.execute_invocation.contract_address,
            "0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7"
        );
        assert_eq!(
            trace.execute_invocation.entry_point_selector,
            "0x0083afd3f4caedc6eebf44246fe54e38c95e3179a5ec9ea81740eca5b482d12e"
        );
        assert_eq!(trace.execute_invocation.calldata.len(), 3);
        assert_eq!(trace.execute_invocation.calls.len(), 1);
        assert!(trace.state_diff.is_some());
    }

    #[test]
    fn test_reconstruct_execution_context() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-programs/starknet/mock_tx_trace.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let trace = TransactionTrace::from_json(&content).unwrap();

        let ctx = reconstruct_execution_context(&trace);

        assert_eq!(
            ctx.contract_address,
            "0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7"
        );
        assert_eq!(
            ctx.entry_point_selector,
            "0x0083afd3f4caedc6eebf44246fe54e38c95e3179a5ec9ea81740eca5b482d12e"
        );
        assert_eq!(
            ctx.calldata,
            vec![
                "0x03e85bfbb8e2a42b7bead9e88e9a1b19dbccf661471061807292120462396ec9",
                "0x0de0b6b3a7640000",
                "0x00"
            ]
        );
        // Two storage entries from the state diff
        assert_eq!(ctx.storage_state.len(), 2);
        assert_eq!(
            ctx.storage_state[0],
            (
                "0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7".to_string(),
                "0x0110e2f729c9c2b988559994a3daccd838cf548bb3859e0468e075e62687e555".to_string(),
                "0x0de0b6b3a7640000".to_string(),
            )
        );
    }

    #[test]
    fn test_reconstruct_context_no_state_diff() {
        let trace = TransactionTrace {
            tx_type: "INVOKE".to_string(),
            execute_invocation: InvocationTrace {
                contract_address: "0xabc".to_string(),
                entry_point_selector: "0xdef".to_string(),
                calldata: vec!["0x1".to_string()],
                caller_address: "0x0".to_string(),
                result: vec![],
                calls: vec![],
                events: vec![],
                messages: vec![],
            },
            state_diff: None,
        };

        let ctx = reconstruct_execution_context(&trace);
        assert_eq!(ctx.contract_address, "0xabc");
        assert_eq!(ctx.entry_point_selector, "0xdef");
        assert_eq!(ctx.calldata, vec!["0x1"]);
        assert!(ctx.storage_state.is_empty());
    }

    #[test]
    fn test_transaction_trace_from_bare_json() {
        // Test parsing without the JSON-RPC envelope.
        let bare = r#"{
            "type": "INVOKE",
            "execute_invocation": {
                "contract_address": "0x1",
                "entry_point_selector": "0x2",
                "calldata": ["0x3"],
                "caller_address": "0x0",
                "result": [],
                "calls": [],
                "events": [],
                "messages": []
            }
        }"#;

        let trace = TransactionTrace::from_json(bare).unwrap();
        assert_eq!(trace.tx_type, "INVOKE");
        assert_eq!(trace.execute_invocation.contract_address, "0x1");
    }

    #[test]
    fn test_starknet_rpc_client_new() {
        let client = StarknetRpcClient::new("https://example.com/rpc");
        assert_eq!(client.rpc_url, "https://example.com/rpc");
    }

    #[test]
    fn test_replay_config_creation() {
        let config = ReplayConfig {
            tx_hash: "0xabc123".to_string(),
            rpc_url: "https://example.com/rpc".to_string(),
            source_dir: Some(PathBuf::from("/tmp/sources")),
        };
        assert_eq!(config.tx_hash, "0xabc123");
        assert_eq!(config.rpc_url, "https://example.com/rpc");
        assert_eq!(config.source_dir, Some(PathBuf::from("/tmp/sources")));
    }

    #[test]
    fn test_invocation_nested_calls() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-programs/starknet/mock_tx_trace.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let trace = TransactionTrace::from_json(&content).unwrap();

        // The mock trace has one nested call.
        let nested = &trace.execute_invocation.calls;
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].events.len(), 1);
        assert_eq!(nested[0].events[0].keys.len(), 1);
        assert_eq!(nested[0].events[0].data.len(), 4);
    }
}
