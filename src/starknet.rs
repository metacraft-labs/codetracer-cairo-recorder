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

use codetracer_trace_types::{Line, TypeKind, ValueRecord, NONE_VALUE};
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
    #[serde(rename = "contract_call")]
    ContractCall {
        caller: String,
        callee: String,
        selector: String,
        calldata: Vec<String>,
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
/// Creates `trace.json`/`trace.bin` (depending on format), `trace_metadata.json`, and `trace_paths.json`
/// in `out_dir`, mirroring the output of the `record` subcommand.
pub fn write_starknet_trace(
    trace_path: &Path,
    entries: &[TraceEntry],
    out_dir: &Path,
    format: TraceEventsFileFormat,
) -> Result<()> {
    let program_str = trace_path.to_string_lossy();
    let mut writer = create_trace_writer(&program_str, &[], format);

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    let events_filename = match format {
        TraceEventsFileFormat::Json => "trace.json",
        TraceEventsFileFormat::Binary | TraceEventsFileFormat::BinaryV0 => "trace.bin",
        TraceEventsFileFormat::Ctfs => "trace.ctfs",
    };
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

    let converted = convert_snforge_trace(entries);

    for event in &converted {
        match event {
            TraceEvent::Step { line } => {
                TraceWriter::register_step(&mut *writer, trace_path, Line(*line as i64));
            }
            TraceEvent::Call { name } => {
                let fn_id =
                    TraceWriter::ensure_function_id(&mut *writer, name, trace_path, Line(1));
                TraceWriter::register_call(&mut *writer, fn_id, vec![]);
            }
            TraceEvent::Return => {
                TraceWriter::register_return(&mut *writer, NONE_VALUE);
            }
            TraceEvent::Variable { name, value } => {
                let val = ValueRecord::String {
                    text: value.clone(),
                    type_id: str_type_id,
                };
                TraceWriter::register_variable_with_full_value(&mut *writer, name, val);
            }
        }
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
    /// Calls `starknet_traceTransaction` via JSON-RPC.
    ///
    /// **Note**: This currently returns an error because it requires a live
    /// RPC connection. In tests, use [`TransactionTrace::from_json`] with
    /// mock data instead.
    pub fn trace_transaction(&self, tx_hash: &str) -> Result<TransactionTrace> {
        // Placeholder: actual HTTP/JSON-RPC call would go here.
        // We cannot make real network calls in the test/CI environment,
        // so this is left as infrastructure scaffolding.
        Err(eyre!(
            "live RPC calls not yet implemented (would call {} for tx {})",
            self.rpc_url,
            tx_hash
        ))
    }
}

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
