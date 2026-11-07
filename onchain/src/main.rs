//! `codetracer-cairo-onchain` — record mined Starknet transactions as StarkWare's own
//! executor (`blockifier`, over `cairo-vm`) re-runs them, and say how the re-run compares
//! with what the chain recorded.
//!
//! THE PRESTATE: the executor starts from the chain state at the PARENT block, read from
//! a node (prefetched as one `starknet_simulateTransactions` + `RETURN_INITIAL_READS`
//! call, single-leaf reads for anything it did not return), and replays the block's
//! transactions 0..k-1 before transaction k. So the state transaction k sees is the state
//! the sequencer gave it, and the block's whole state diff can be — and is — compared
//! with the node's own.
//!
//! THE TRACE: cairo-vm's own, one step per executed instruction, captured through
//! `blockifier-patch/` and written as a CTFS container by `record.rs`. The container is a
//! function of its inputs: its recording id is derived from the block and the
//! transaction, and its listings are interned as `./sources/<class_hash>.casm` relative
//! to the capture directory, so two runs over one transcript write the same bytes.
//!
//! THE COMPARISON, which is what makes a recording a claim and not a guess: for every
//! transaction, status, fee, L2 gas, L1 data gas, events, L2->L1 messages, its own state
//! diff (against `starknet_traceBlockTransactions`), and its call tree (class hash,
//! address and selector of every entry point, against the same trace); for the block,
//! the whole state diff against `starknet_getStateUpdate`. All of it is written to
//! `report.json` and `block.json`, and none of it is summarised away.

mod block;
mod listing;
mod record;
mod rpc;
mod source;
mod state;
mod transcript;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use blockifier::blockifier::config::TransactionExecutorConfig;
use blockifier::blockifier::transaction_executor::{OsInitialReadsCollection, TransactionExecutor};
use blockifier::execution::bt_trace;
use blockifier::execution::call_info::CallInfo;
use blockifier::state::cached_state::StateMaps;
use blockifier::transaction::objects::TransactionExecutionInfo;
use clap::{Parser, Subcommand};
use eyre::{bail, eyre, Result};
use serde_json::{json, Value};
use starknet_types_core::felt::Felt;

#[derive(Parser)]
#[command(name = "codetracer-cairo-onchain", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Re-execute a whole block and record its transactions.
    TraceBlock {
        block: u64,
        /// Starknet JSON-RPC endpoint (spec 0.10+). Not needed with --transcript-in.
        #[arg(long)]
        rpc_url: Option<String>,
        #[arg(short = 'o', long)]
        out_dir: PathBuf,
        /// Record only these transactions (the block is still replayed whole, because
        /// the prestate of index k is what indices 0..k-1 left behind). Default: all.
        #[arg(long = "tx")]
        txs: Vec<String>,
        /// Write every RPC answer this run used to this DIRECTORY (an index plus one
        /// gzipped answer per request), so the run can be reproduced with no network.
        #[arg(long)]
        transcript_out: Option<PathBuf>,
        /// Serve every RPC answer from this transcript directory; a request it does not
        /// hold is a refusal.
        #[arg(long)]
        transcript_in: Option<PathBuf>,
        /// Also measure the lowest block this pipeline can replay (see `probe_floor`).
        #[arg(long)]
        probe_floor: bool,
        /// A directory of `bind-class` bindings: a class that ran and has one there is
        /// listed with its build's function, type and libfunc names.
        #[arg(long)]
        class_sources: Option<PathBuf>,
    },
    /// Prove a build of a class's source against the chain's own class, and write the
    /// binding a trace applies. Refuses — writes nothing — unless the Sierra program,
    /// entry points, class version and ABI all agree and the chain's answer hashes to the
    /// class hash.
    BindClass {
        /// The class hash, as the chain names it.
        class_hash: String,
        /// The chain's `starknet_getClass` answer (JSON, or a transcript's `.json.gz`).
        #[arg(long)]
        chain_class: PathBuf,
        /// The build's `<package>_<contract>.contract_class.json`, built with
        /// `sierra-replace-ids = true` so its debug info names the program.
        #[arg(long)]
        built: PathBuf,
        /// What the build was: `key=value`, repeated (repository, revision, toolchain, …).
        /// Published verbatim beside the binding.
        #[arg(long = "build")]
        build: Vec<String>,
        /// `LABEL=DIR`, repeated: a build path under DIR is published as `LABEL/<relative>`.
        /// Paths in Scarb's cache are named by package and version without one; any other
        /// path is refused, so no machine-specific path reaches a binding.
        #[arg(long = "root")]
        roots: Vec<String>,
        /// Carry the text of every file a statement location names, so a trace can
        /// position steps on Cairo lines. Only for sources whose licences allow it to be
        /// republished; without it the binding carries names and file hashes only.
        #[arg(long)]
        embed_sources: bool,
        #[arg(short = 'o', long)]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::TraceBlock {
            block,
            rpc_url,
            out_dir,
            txs,
            transcript_out,
            transcript_in,
            probe_floor,
            class_sources,
        } => {
            let sources = match &class_sources {
                Some(d) => source::load_dir(d)?,
                None => HashMap::new(),
            };
            let rpc = match (&transcript_in, &rpc_url) {
                (Some(p), _) => {
                    let (label, t) = transcript::read(p)?;
                    rpc::Rpc::replaying(&label, t)
                }
                (None, Some(u)) => rpc::Rpc::new(u),
                (None, None) => bail!("--rpc-url or --transcript-in is required"),
            };
            let r = trace_block(&rpc, block, &out_dir, &txs, probe_floor, &sources);
            if let Some(p) = transcript_out {
                let n = transcript::write(&p, rpc.url(), &rpc.transcript())?;
                eprintln!("transcript: {n} answer(s) -> {}", p.display());
            }
            r
        }
        Command::BindClass {
            class_hash,
            chain_class,
            built,
            build,
            roots,
            embed_sources,
            out,
        } => {
            let roots = roots
                .iter()
                .map(|r| {
                    r.split_once('=')
                        .map(|(l, d)| (l.to_string(), PathBuf::from(d)))
                        .ok_or_else(|| eyre!("--root {r}: expected LABEL=DIR"))
                })
                .collect::<Result<Vec<_>>>()?;
            let h = Felt::from_hex(&class_hash).map_err(|e| eyre!("class hash: {e}"))?;
            let mut b = serde_json::Map::new();
            for kv in &build {
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| eyre!("--build {kv}: expected key=value"))?;
                b.insert(k.to_string(), json!(v));
            }
            let doc = source::bind(
                &chain_class,
                &built,
                h,
                Value::Object(b),
                &roots,
                embed_sources,
            )?;
            let text = serde_json::to_string(&doc)?;
            if out.extension().is_some_and(|x| x == "gz") {
                use std::io::Write;
                let mut gz = flate2::write::GzEncoder::new(
                    std::fs::File::create(&out)?,
                    flate2::Compression::best(),
                );
                gz.write_all(text.as_bytes())?;
                gz.finish()?;
            } else {
                std::fs::write(&out, text)?;
            }
            eprintln!(
                "bound {}: {} functions named, {} -> {}",
                doc["classHash"].as_str().unwrap_or(""),
                doc["functions"],
                doc["compared"]["sierraProgram"].as_str().unwrap_or(""),
                out.display()
            );
            Ok(())
        }
    }
}

fn hexu(v: &Value) -> Option<u128> {
    v.as_str()
        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

fn fh(v: &Value) -> Option<Felt> {
    v.as_str().and_then(|s| Felt::from_hex(s).ok())
}

/// `(contract_address, class_hash, selector)` for every invocation in an RPC trace, in
/// the order the executor runs them.
fn trace_calls(trace_root: &Value) -> Vec<(Felt, Felt, Felt)> {
    fn walk(v: &Value, out: &mut Vec<(Felt, Felt, Felt)>) {
        if v.get("contract_address").is_none() {
            return;
        }
        out.push((
            fh(&v["contract_address"]).unwrap_or_default(),
            fh(&v["class_hash"]).unwrap_or_default(),
            fh(&v["entry_point_selector"]).unwrap_or_default(),
        ));
        for c in v["calls"].as_array().cloned().unwrap_or_default() {
            walk(&c, out);
        }
    }
    let ty = trace_root["type"].as_str().unwrap_or("");
    let order: &[&str] = match ty {
        "DEPLOY_ACCOUNT" => &[
            "constructor_invocation",
            "validate_invocation",
            "fee_transfer_invocation",
        ],
        "L1_HANDLER" => &["function_invocation"],
        _ => &[
            "validate_invocation",
            "execute_invocation",
            "fee_transfer_invocation",
        ],
    };
    let mut out = vec![];
    for k in order {
        walk(&trace_root[*k], &mut out);
    }
    out
}

/// The executor's call infos in execution order (a DEPLOY_ACCOUNT runs its constructor
/// before validation), each tree flattened pre-order.
fn call_infos(info: &TransactionExecutionInfo, deploy_account: bool) -> Vec<&CallInfo> {
    let tops: Vec<&CallInfo> = if deploy_account {
        info.execute_call_info
            .iter()
            .chain(info.validate_call_info.iter())
            .chain(info.fee_transfer_call_info.iter())
            .collect()
    } else {
        info.validate_call_info
            .iter()
            .chain(info.execute_call_info.iter())
            .chain(info.fee_transfer_call_info.iter())
            .collect()
    };
    tops.into_iter().flat_map(|c| c.iter()).collect()
}

/// A per-transaction state diff from an RPC `state_diff` object, as comparable maps.
fn rpc_diff(sd: &Value) -> Diff {
    let mut storage = BTreeMap::new();
    for d in sd["storage_diffs"].as_array().cloned().unwrap_or_default() {
        let a = fh(&d["address"]).unwrap_or_default();
        for e in d["storage_entries"].as_array().cloned().unwrap_or_default() {
            storage.insert(
                (a, fh(&e["key"]).unwrap_or_default()),
                fh(&e["value"]).unwrap_or_default(),
            );
        }
    }
    let mut nonces = BTreeMap::new();
    for n in sd["nonces"].as_array().cloned().unwrap_or_default() {
        nonces.insert(
            fh(&n["contract_address"]).unwrap_or_default(),
            fh(&n["nonce"]).unwrap_or_default(),
        );
    }
    let mut classes = BTreeMap::new();
    for k in ["deployed_contracts", "replaced_classes"] {
        for d in sd[k].as_array().cloned().unwrap_or_default() {
            let a = fh(&d["address"])
                .or_else(|| fh(&d["contract_address"]))
                .unwrap_or_default();
            classes.insert(a, fh(&d["class_hash"]).unwrap_or_default());
        }
    }
    (storage, nonces, classes)
}

fn our_diff(m: &StateMaps) -> Diff {
    let storage = m
        .storage
        .iter()
        .map(|((a, k), v)| ((*a.0.key(), *k.0.key()), *v))
        .collect();
    let nonces = m.nonces.iter().map(|(a, n)| (*a.0.key(), n.0)).collect();
    let classes = m
        .class_hashes
        .iter()
        .map(|(a, c)| (*a.0.key(), c.0))
        .collect();
    (storage, nonces, classes)
}

fn fact(field: &str, published: Value, replayed: Value) -> Value {
    json!({ "field": field, "published": published, "replayed": replayed, "matches": published == replayed })
}

/// A state diff as three comparable maps: storage, nonces, class hashes.
type Diff = (
    BTreeMap<(Felt, Felt), Felt>,
    BTreeMap<Felt, Felt>,
    BTreeMap<Felt, Felt>,
);

struct Executed {
    hash: String,
    index: usize,
    body: Value,
    receipt: Value,
    result: std::result::Result<(TransactionExecutionInfo, StateMaps), String>,
    frames: Vec<bt_trace::Frame>,
}

fn trace_block(
    rpc: &rpc::Rpc,
    number: u64,
    out_dir: &Path,
    wanted: &[String],
    probe: bool,
    sources: &HashMap<starknet_api::core::ClassHash, source::Binding>,
) -> Result<()> {
    if number == 0 {
        bail!("block 0 has no parent block to read a prestate at");
    }
    let started = std::time::Instant::now();
    let blk = block::fetch_block(rpc, number)?;
    eprintln!(
        "block {number}: Starknet {}, {} transaction(s)",
        blk.starknet_version,
        blk.txs.len()
    );
    let wanted: Vec<String> = wanted.iter().map(|h| h.to_lowercase()).collect();
    for w in &wanted {
        if !blk.txs.iter().any(|(h, _, _)| h == w) {
            bail!("{w} is not a transaction of block {number}");
        }
    }
    let traces = rpc.call(
        "starknet_traceBlockTransactions",
        json!([{ "block_number": number }]),
    )?;
    let trace_by_hash: HashMap<String, Value> = traces
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            Some((
                t["transaction_hash"].as_str()?.to_string(),
                t["trace_root"].clone(),
            ))
        })
        .collect();

    // A BLOCK THE EXECUTOR CANNOT RUN IS A RESULT, NOT A FAILURE OF THIS RUN: every row
    // in it is refused by name and the block report says why, so a window over an era
    // the executor does not carry is measured as zero rather than missing.
    // THE CONTROL ARM FOR "THE STEPS ARE THE VM'S": with the observer not installed the
    // executor runs exactly as upstream, and this tool must then record NOTHING — there
    // is no other path that could fill a step stream in. `tests/offline.rs` runs it.
    let observe = std::env::var_os("CODETRACER_CAIRO_ONCHAIN_NO_OBSERVER").is_none();
    let mut executed = vec![];
    let mut summary = None;
    let mut prefetch = state::PrefetchReport::default();
    let classes;
    match block::block_context(&blk) {
        Err(e) => {
            classes = state::Classes::default();
            for (i, (h, body, receipt)) in blk.txs.iter().enumerate() {
                executed.push(Executed {
                    hash: h.clone(),
                    index: i,
                    body: body.clone(),
                    receipt: receipt.clone(),
                    result: Err(e.to_string()),
                    frames: vec![],
                });
            }
        }
        Ok(ctx) => {
            let old = block::old_block_hash(rpc, number)?;
            let reader = state::RpcState::new(rpc, number - 1);
            classes = reader.classes.clone();
            let bodies: Vec<&Value> = blk.txs.iter().map(|(_, b, _)| b).collect();
            prefetch = reader.prefetch(&bodies);
            // A body that does not convert stops the block there: the prestate of every
            // later index is what this one would have left behind.
            let txs: Vec<std::result::Result<_, String>> = blk
                .txs
                .iter()
                .map(|(h, body, _)| block::executable(&reader, h, body).map_err(|e| e.to_string()))
                .collect();
            let mut exec = TransactionExecutor::pre_process_and_create(
                reader,
                ctx,
                old,
                TransactionExecutorConfig::default(),
            )
            .map_err(|e| eyre!("pre-process block {number}: {e}"))?;
            let mut broken: Option<usize> = None;
            for (i, ((h, body, receipt), tx)) in blk.txs.iter().zip(&txs).enumerate() {
                let (result, frames) = match (broken, tx) {
                    (Some(j), _) => (
                        Err(format!(
                            "index {j} of this block was refused, so the state this \
                             transaction starts from is not the one the sequencer gave it"
                        )),
                        vec![],
                    ),
                    (None, Err(e)) => (Err(format!("the body does not convert: {e}")), vec![]),
                    (None, Ok(tx)) => {
                        if observe {
                            bt_trace::install();
                        }
                        let result = exec.execute(tx).map_err(|e| e.to_string());
                        (
                            result,
                            bt_trace::take().map(|s| s.frames).unwrap_or_default(),
                        )
                    }
                };
                if result.is_err() && broken.is_none() {
                    broken = Some(i);
                }
                executed.push(Executed {
                    hash: h.clone(),
                    index: i,
                    body: body.clone(),
                    receipt: receipt.clone(),
                    result,
                    frames,
                });
            }
            if broken.is_none() {
                summary = Some(
                    exec.non_consuming_finalize(OsInitialReadsCollection::Skip)
                        .map_err(|e| eyre!("finalize block {number}: {e}"))?,
                );
            }
        }
    }

    // The block's whole state diff against the node's — measurable only when every
    // transaction of the block ran, because the diff is the sum of all of them.
    let block_diff = match &summary {
        None => json!({
            "notMeasured": "not every transaction of this block was executed, so there is no \
                            replayed block state diff to compare"
        }),
        Some(summary) => {
            let su = rpc.call(
                "starknet_getStateUpdate",
                json!([{ "block_number": number }]),
            )?;
            let (chain_s, chain_n, chain_c) = rpc_diff(&su["state_diff"]);
            let ours = &summary.state_diff;
            let our_s: BTreeMap<(Felt, Felt), Felt> = ours
                .storage_updates
                .iter()
                .flat_map(|(a, m)| m.iter().map(move |(k, v)| ((*a.0.key(), *k.0.key()), *v)))
                .collect();
            let our_n: BTreeMap<Felt, Felt> = ours
                .address_to_nonce
                .iter()
                .map(|(a, n)| (*a.0.key(), n.0))
                .collect();
            let our_c: BTreeMap<Felt, Felt> = ours
                .address_to_class_hash
                .iter()
                .map(|(a, c)| (*a.0.key(), c.0))
                .collect();
            json!({
                "storageEntries": { "published": chain_s.len(), "replayed": our_s.len(), "matches": chain_s == our_s },
                "nonces": { "published": chain_n.len(), "replayed": our_n.len(), "matches": chain_n == our_n },
                "classHashes": { "published": chain_c.len(), "replayed": our_c.len(), "matches": chain_c == our_c },
            })
        }
    };

    std::fs::create_dir_all(out_dir)?;
    let mut tx_reports = vec![];
    for e in &executed {
        let trace_root = trace_by_hash.get(&e.hash).cloned().unwrap_or(Value::Null);
        let ty = e.body["type"].as_str().unwrap_or("").to_string();
        let mut facts = vec![];
        let mut report = json!({
            "txHash": e.hash, "blockNumber": number, "txIndex": e.index, "type": ty,
            "version": e.body["version"],
        });
        match &e.result {
            Err(err) => {
                report["outcome"] = json!("refused");
                // `runtime-refused` is refusal-reasons.json's token for "the replay runtime
                // declined by name": an era the executor carries no constants for, a body
                // that does not convert, an executor error, or a preceding refusal that
                // took this transaction's prestate with it. All are repairable.
                report["refusalReason"] = json!("runtime-refused");
                report["refusal"] = json!(format!("the executor refused the transaction: {err}"));
            }
            Ok((info, maps)) => {
                let r = &e.receipt;
                facts.push(fact(
                    "executionStatus",
                    json!(r["execution_status"]),
                    json!(if info.is_reverted() {
                        "REVERTED"
                    } else {
                        "SUCCEEDED"
                    }),
                ));
                facts.push(fact(
                    "actualFee",
                    json!(hexu(&r["actual_fee"]["amount"]).map(|v| v.to_string())),
                    json!(info.receipt.fee.0.to_string()),
                ));
                // A RECEIPT WHOSE GAS VECTOR IS NOT POPULATED IS NOT A MEASUREMENT. Measured
                // on block 620,000 (Starknet 0.13.1): every receipt reports 0 for all three
                // dimensions while charging a non-zero fee — which no execution can do — so
                // the node did not record the vector for that era. Comparing the replay's
                // gas against those zeros would publish 147 "divergences" that are absences.
                // The rule is narrow on purpose: ALL THREE zero AND a non-zero fee; one zero
                // dimension beside a populated one is still compared.
                let res = &r["execution_resources"];
                let unpopulated = ["l1_gas", "l1_data_gas", "l2_gas"]
                    .iter()
                    .all(|k| res[*k].as_u64() == Some(0))
                    && hexu(&r["actual_fee"]["amount"]).unwrap_or(0) > 0;
                for (field, key, replayed) in [
                    ("l2Gas", "l2_gas", info.receipt.gas.l2_gas.0),
                    ("l1DataGas", "l1_data_gas", info.receipt.gas.l1_data_gas.0),
                    ("l1Gas", "l1_gas", info.receipt.gas.l1_gas.0),
                ] {
                    if unpopulated {
                        facts.push(json!({
                            "field": field, "published": res[key], "replayed": replayed,
                            "matches": null,
                            "notMeasured": "the receipt's gas vector is all zero beside a non-zero fee: the node did not populate it for this era",
                        }));
                    } else {
                        facts.push(fact(field, json!(res[key]), json!(replayed)));
                    }
                }
                let calls = call_infos(info, ty == "DEPLOY_ACCOUNT");
                // `OrderedEvent::order` counts within one top-level phase (validate,
                // execute, fee transfer each start again at 0), so events are ordered
                // within a phase and the phases are concatenated in execution order —
                // which is the order the receipt lists them in.
                let our_events: Vec<Value> = {
                    let tops: Vec<&CallInfo> = if ty == "DEPLOY_ACCOUNT" {
                        info.execute_call_info
                            .iter()
                            .chain(info.validate_call_info.iter())
                            .chain(info.fee_transfer_call_info.iter())
                            .collect()
                    } else {
                        info.validate_call_info
                            .iter()
                            .chain(info.execute_call_info.iter())
                            .chain(info.fee_transfer_call_info.iter())
                            .collect()
                    };
                    let mut out = vec![];
                    for top in tops {
                        let mut evs: Vec<(usize, Value)> = top
                            .iter()
                            .flat_map(|c| {
                                let from = c.call.storage_address.0.key().to_hex_string();
                                c.execution.events.iter().map(move |ev| {
                                    (ev.order, json!({
                                        "from_address": from,
                                        "keys": ev.event.keys.iter().map(|k| k.0.to_hex_string()).collect::<Vec<_>>(),
                                        "data": ev.event.data.0.iter().map(|d| d.to_hex_string()).collect::<Vec<_>>(),
                                    }))
                                })
                            })
                            .collect();
                        evs.sort_by_key(|(o, _)| *o);
                        out.extend(evs.into_iter().map(|(_, v)| v));
                    }
                    out
                };
                let norm = |v: &Value| -> Value {
                    let f = |x: &Value| {
                        x.as_str()
                            .and_then(|s| Felt::from_hex(s).ok())
                            .map(|f| f.to_hex_string())
                    };
                    json!({
                        "from_address": f(&v["from_address"]),
                        "keys": v["keys"].as_array().map(|a| a.iter().map(f).collect::<Vec<_>>()),
                        "data": v["data"].as_array().map(|a| a.iter().map(f).collect::<Vec<_>>()),
                    })
                };
                let chain_events: Vec<Value> = r["events"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(norm)
                    .collect();
                let our_events_n: Vec<Value> = our_events.iter().map(norm).collect();
                facts.push(fact(
                    "eventCount",
                    json!(chain_events.len()),
                    json!(our_events_n.len()),
                ));
                let first_ev_diff = chain_events
                    .iter()
                    .zip(&our_events_n)
                    .position(|(a, b)| a != b)
                    .map(|i| json!({ "index": i, "published": chain_events[i], "replayed": our_events_n[i] }));
                facts.push(json!({
                    "field": "events",
                    "matches": chain_events == our_events_n,
                    "firstDifference": first_ev_diff,
                }));
                let msgs: usize = calls
                    .iter()
                    .map(|c| c.execution.l2_to_l1_messages.len())
                    .sum();
                facts.push(fact(
                    "messagesSent",
                    json!(r["messages_sent"].as_array().map(|a| a.len()).unwrap_or(0)),
                    json!(msgs),
                ));
                // The transaction's own state diff, against the node's trace of it.
                let (ts, tn, tc) = rpc_diff(&trace_root["state_diff"]);
                let (os, on, oc) = our_diff(maps);
                let storage_diffs: Vec<Value> = ts
                    .iter()
                    .filter(|(k, v)| os.get(k) != Some(v))
                    .map(|(k, v)| json!({ "contract": k.0.to_hex_string(), "key": k.1.to_hex_string(),
                                         "published": v.to_hex_string(), "replayed": os.get(k).map(|x| x.to_hex_string()) }))
                    .chain(os.iter().filter(|(k, _)| !ts.contains_key(k)).map(|(k, v)| json!({
                        "contract": k.0.to_hex_string(), "key": k.1.to_hex_string(),
                        "published": null, "replayed": v.to_hex_string() })))
                    .take(5)
                    .collect();
                // NOT COUNTED TOWARD `reproduced`, AND THE REASON IS A MEASUREMENT. These
                // per-transaction diffs come from the NODE'S OWN re-execution
                // (`starknet_traceBlockTransactions`), not from consensus. Measured on
                // block 16,664,437 index 0: the node's trace reports 3,965,440 L2 gas
                // where the receipt reports 4,205,440, and its fee-token diff credits
                // the sequencer 62,557,090,391,068,032 FRI where the receipt's fee is
                // 66,342,333,070,348,032 — while this replay matches the receipt to
                // the unit and the block's state diff entry for entry. A disagreement
                // here is therefore published as a disagreement with that oracle, and
                // consensus is checked by the receipt facts above and the block diff.
                report["traceOracle"] = json!({
                    "method": "starknet_traceBlockTransactions",
                    "counted": false,
                    "stateDiff.storage": { "published": ts.len(), "replayed": os.len(),
                                           "matches": ts == os, "firstDifferences": storage_diffs },
                    "stateDiff.nonces": { "published": tn.len(), "replayed": on.len(), "matches": tn == on },
                    "stateDiff.classHashes": { "published": tc.len(), "replayed": oc.len(), "matches": tc == oc },
                    "l2Gas": trace_root["execution_resources"]["l2_gas"],
                });
                // WHICH CONTRACTS THIS TRANSACTION GAVE A CLASS, from the REPLAY's own
                // state diff (an upgrade's `replace_class`, a deployment). The replay
                // cannot tell the two apart from the class map alone, so `kind` is taken
                // from the node's trace for the same address — and stated only when the
                // trace gives that address the SAME class, never borrowed for a class the
                // replay did not set. Consensus on the classes is the block diff above.
                let kind_of = |k: &str, a: &Felt, c: &Felt| {
                    trace_root["state_diff"][k].as_array().is_some_and(|l| {
                        l.iter().any(|d| {
                            fh(&d["contract_address"]).or_else(|| fh(&d["address"])) == Some(*a)
                                && fh(&d["class_hash"]) == Some(*c)
                        })
                    })
                };
                report["classChanges"] = json!(oc
                    .iter()
                    .map(|(a, c)| json!({
                        "contract": a.to_hex_string(),
                        "classHash": c.to_hex_string(),
                        "kind": if kind_of("replaced_classes", a, c) { json!("replaced") }
                                else if kind_of("deployed_contracts", a, c) { json!("deployed") }
                                else { Value::Null },
                    }))
                    .collect::<Vec<_>>());
                // The call tree, entry point by entry point.
                let theirs = trace_calls(&trace_root);
                // A REVERTED TRANSACTION'S TRACE HAS NO EXECUTE TREE: the node replaces
                // `execute_invocation` with `{revert_reason}` and lists validate and the fee
                // transfer only, while the replay saw the execution that ran and was rolled
                // back (measured: block 620,000, 10 reverted rows, 2 published entry points
                // against 4-65 replayed). So for such a row the reverted top-level frame and
                // everything under it are compared against nothing and reported separately,
                // and the phases the trace does list are compared exactly.
                let reverted_execute = trace_root.get("execute_invocation").is_some_and(|v| {
                    v.get("contract_address").is_none() && v.get("revert_reason").is_some()
                });
                let tops: Vec<usize> = e
                    .frames
                    .iter()
                    .filter(|f| f.parent.is_none())
                    .map(|f| f.id)
                    .collect();
                let dropped_top = if reverted_execute && ty != "DEPLOY_ACCOUNT" && tops.len() == 3 {
                    Some(tops[1])
                } else {
                    None
                };
                let in_dropped = |mut id: usize| -> bool {
                    loop {
                        if Some(id) == dropped_top {
                            return true;
                        }
                        match e.frames[id].parent {
                            Some(p) => id = p,
                            None => return false,
                        }
                    }
                };
                let mine: Vec<(Felt, Felt, Felt)> = e
                    .frames
                    .iter()
                    .filter(|f| !in_dropped(f.id))
                    .map(|f| (*f.storage_address.0.key(), f.class_hash.0, f.selector.0))
                    .collect();
                if dropped_top.is_some() {
                    report["revertedExecuteFrames"] =
                        json!(e.frames.iter().filter(|f| in_dropped(f.id)).count());
                }
                if e.frames.is_empty() && !theirs.is_empty() {
                    // Not observed is not the same as not matching: with the observer off
                    // there are no frames to compare, and the fact is reported as such.
                    facts.push(json!({ "field": "callTree", "published": theirs.len(),
                                       "matches": null, "notMeasured": "the VM observer was not installed" }));
                } else {
                    facts.push(json!({ "field": "callTree", "published": theirs.len(), "replayed": mine.len(), "matches": theirs == mine }));
                }
                if let Some(revert) = &info.revert_error {
                    report["revertError"] = json!(revert.to_string());
                }
                let mismatched = facts
                    .iter()
                    .filter(|f| f["matches"] == json!(false))
                    .count();
                let matched = facts.iter().filter(|f| f["matches"] == json!(true)).count();
                let not_measured = facts.len() - mismatched - matched;
                report["outcome"] = json!(if mismatched == 0 {
                    "replayed"
                } else {
                    "divergent"
                });
                report["effects"] = json!({
                    "reproduced": mismatched == 0,
                    "matched": matched,
                    "mismatched": mismatched,
                    "notMeasured": not_measured,
                    "facts": facts,
                });
            }
        }
        let steps_total: usize = e.frames.iter().map(|f| f.steps.len()).sum();
        report["vm"] = json!({
            "frames": e.frames.len(),
            "steps": steps_total,
            "framesWithError": e.frames.iter().filter(|f| f.error.is_some()).count(),
            "cairo0Frames": e.frames.iter().filter(|f| f.cairo0).count(),
        });
        tx_reports.push(report);
    }

    // The recordings.
    let mut recorded = 0;
    for (e, report) in executed.iter().zip(tx_reports.iter_mut()) {
        if !wanted.is_empty() && !wanted.contains(&e.hash) {
            report["recorded"] = json!(false);
            continue;
        }
        if e.frames.is_empty() {
            report["recorded"] = json!(false);
            report["notRecordedBecause"] = json!(match (&e.result, observe) {
                (Err(err), _) => format!("the executor refused the transaction: {err}"),
                (Ok(_), false) => "CODETRACER_CAIRO_ONCHAIN_NO_OBSERVER is set: the VM's trace \
                                   was not observed, and no other source of steps exists"
                    .to_string(),
                (Ok(_), true) => "the executor ran no entry point for this transaction".to_string(),
            });
            continue;
        }
        let dir = out_dir.join(&e.hash);
        let src = dir.join("sources");
        std::fs::create_dir_all(&src)?;
        let mut listings = HashMap::new();
        let mut paths = HashMap::new();
        let mut abis = HashMap::new();
        let mut class_rows = vec![];
        // Cairo sources a bound build embeds, interned as `./sources/cairo/<stable path>`.
        let mut cairo_at: HashMap<_, HashMap<usize, (PathBuf, u32)>> = HashMap::new();
        let mut cairo_texts: BTreeMap<PathBuf, String> = BTreeMap::new();
        for f in &e.frames {
            if listings.contains_key(&f.class_hash) {
                continue;
            }
            let mut rec = classes
                .borrow()
                .get(&f.class_hash)
                .cloned()
                .ok_or_else(|| {
                    eyre!(
                        "{}: class {} ran and was never loaded through the reader",
                        e.hash,
                        f.class_hash
                    )
                })?;
            // A binding is applied only after it re-proves itself against the class that
            // ran; one that does not is reported and the class is listed without it.
            let (bound, source_row) = match sources.get(&f.class_hash) {
                None => (None, Value::Null),
                Some(b) => match source::apply(&mut rec, b) {
                    Ok(bd) => {
                        let row = json!({
                            "bound": true,
                            "functionsNamed": bd.function_at.len(),
                            "codeLocations": bd.code_locations,
                            "provenance": bd.provenance,
                        });
                        (Some(bd), row)
                    }
                    Err(e) => (None, json!({ "bound": false, "refused": e.to_string() })),
                },
            };
            let l = listing::build(&rec, bound.as_ref());
            if let Some(bd) = bound.as_ref().filter(|bd| !bd.pc_location.is_empty()) {
                let mut at = HashMap::new();
                for (pc, (fi, line, _col)) in &bd.pc_location {
                    let (name, text) = &bd.files[*fi];
                    let interned = PathBuf::from(format!("./sources/cairo/{name}"));
                    if let Some(t) = text {
                        match cairo_texts.get(&interned) {
                            Some(prev) if prev != t => bail!(
                                "{}: two bound classes embed different texts for {name}",
                                e.hash
                            ),
                            Some(_) => {}
                            None => {
                                cairo_texts.insert(interned.clone(), t.clone());
                            }
                        }
                        at.insert(*pc, (interned, *line));
                    }
                }
                cairo_at.insert(f.class_hash, at);
            }
            let p = src.join(format!("{}.casm", f.class_hash.0.to_hex_string()));
            std::fs::write(&p, &l.text)?;
            class_rows.push(json!({
                "classHash": rec.class_hash.0.to_hex_string(),
                "entryPoints": l.entry_names.values().collect::<Vec<_>>(),
                "cairo0": l.cairo0,
                "instructions": l.instructions,
                "listing": format!("sources/{}.casm", f.class_hash.0.to_hex_string()),
                "sierraVersion": rec.sierra_version.as_ref().map(|v| v.to_string()),
                "source": source_row,
            }));
            abis.insert(f.class_hash, rec.abi.clone());
            paths.insert(
                f.class_hash,
                PathBuf::from(format!("./sources/{}.casm", f.class_hash.0.to_hex_string())),
            );
            listings.insert(f.class_hash, l);
        }
        // Events per frame, joined to frames only where the two lists name the same
        // entry points in the same order; otherwise events are left out and the report
        // says so, rather than being attached to the wrong frame.
        let mut events_by_frame = BTreeMap::new();
        let mut events_joined = false;
        if let Ok((info, _)) = &e.result {
            let ty = e.body["type"].as_str().unwrap_or("");
            let calls = call_infos(info, ty == "DEPLOY_ACCOUNT");
            let same = calls.len() == e.frames.len()
                && calls.iter().zip(&e.frames).all(|(c, f)| {
                    c.call.class_hash == Some(f.class_hash)
                        && c.call.storage_address == f.storage_address
                        && c.call.entry_point_selector == f.selector
                });
            if same {
                events_joined = true;
                for (c, f) in calls.iter().zip(&e.frames) {
                    let evs: record::FrameEvents = c
                        .execution
                        .events
                        .iter()
                        .map(|ev| {
                            (
                                ev.event.keys.iter().map(|k| k.0).collect(),
                                ev.event.data.0.clone(),
                            )
                        })
                        .collect();
                    if !evs.is_empty() {
                        events_by_frame.insert(f.id, evs);
                    }
                }
            }
        }
        let revert = e
            .result
            .as_ref()
            .ok()
            .and_then(|(i, _)| i.revert_error.as_ref().map(|r| r.to_string()));
        let label = format!("starknet-{}-{}", number, e.hash);
        // Only the files a step of THIS transaction is shown in: BlockTracer shows the
        // source of the code that ran, not every file the class's build touched
        // (Source-Resolution.md §8).
        let used: std::collections::BTreeSet<&PathBuf> = e
            .frames
            .iter()
            .flat_map(|f| {
                let at = cairo_at.get(&f.class_hash);
                f.steps
                    .iter()
                    .filter_map(move |s| at.and_then(|m| m.get(&s.pc)).map(|x| &x.0))
            })
            .collect();
        // And per class, the files ITS steps are shown in, so a source bundle keyed by
        // class carries exactly those (a corelib file two classes ran is in both).
        let mut used_by: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
        for f in &e.frames {
            if let Some(at) = cairo_at.get(&f.class_hash) {
                for s in &f.steps {
                    if let Some((p, _)) = at.get(&s.pc) {
                        used_by
                            .entry(f.class_hash.0.to_hex_string())
                            .or_default()
                            .insert(p.display().to_string());
                    }
                }
            }
        }
        for row in class_rows.iter_mut() {
            let h = row["classHash"].as_str().unwrap_or("").to_string();
            row["cairoSources"] = json!(used_by.get(&h).cloned().unwrap_or_default());
        }
        let mut cairo_files = vec![];
        for (interned, text) in cairo_texts.iter().filter(|(p, _)| used.contains(p)) {
            let on_disk = dir.join(interned);
            std::fs::create_dir_all(on_disk.parent().unwrap_or(&dir))?;
            std::fs::write(&on_disk, text)?;
            cairo_files.push((
                interned.clone(),
                text.lines().map(|l| l.len() as u32).collect::<Vec<_>>(),
            ));
        }
        let cx = record::Context {
            listings: &listings,
            paths: &paths,
            abis: &abis,
            cairo_at: &cairo_at,
            cairo_files: &cairo_files,
        };
        let recording_id =
            record::derived_recording_id(blk.info.block_timestamp.0, &Felt::from_hex(&e.hash)?);
        let r = record::write_container(
            &label,
            &recording_id,
            &dir,
            &e.frames,
            &events_by_frame,
            revert.as_deref(),
            &cx,
        )?;
        let bytes = std::fs::metadata(&r.container)
            .map(|m| m.len())
            .unwrap_or(0);
        report["recorded"] = json!(true);
        report["container"] = json!({
            "file": r.container.file_name().map(|n| n.to_string_lossy().to_string()),
            "bytes": bytes,
            "recordingId": recording_id,
            "steps": r.steps,
            "entryPointFrames": r.frames,
            "internalCalls": r.internal_calls,
            "eventsWritten": r.events_written,
            "cairoSteps": r.cairo_steps,
            "cairoSources": cairo_files.iter().map(|(p, _)| p.display().to_string()).collect::<Vec<_>>(),
            "eventsJoinedToFrames": events_joined,
        });
        report["classes"] = json!(class_rows);
        report["frames"] =
            json!(e.frames.iter().map(|f| json!({
            "id": f.id, "parent": f.parent, "parentStep": f.parent_step,
            "contract": f.storage_address.0.key().to_hex_string(),
            "classHash": f.class_hash.0.to_hex_string(),
            "selector": f.selector.0.to_hex_string(),
            "entryPointType": f.entry_point_type, "callType": f.call_type,
            "cairo0": f.cairo0, "steps": f.steps.len(), "failed": f.failed, "error": f.error,
        })).collect::<Vec<_>>());
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        recorded += 1;
    }

    let replayed = tx_reports
        .iter()
        .filter(|r| r["outcome"] == "replayed")
        .count();
    let block_report = json!({
        "format": "codetracer-cairo-onchain/block-report@1",
        "recorder": { "name": "codetracer-cairo-onchain", "version": env!("CARGO_PKG_VERSION"),
                      "executor": "blockifier 0.20.0-rc.1 (crates.io) + blockifier-patch" },
        "endpoint": rpc.url(),
        "blockNumber": number,
        "blockHash": blk.header["block_hash"],
        "parentHash": blk.header["parent_hash"],
        "starknetVersion": blk.starknet_version,
        "timestamp": blk.header["timestamp"],
        "prestate": { "readAt": number - 1, "prefetch": prefetch,
                      "rpcCalls": *rpc.calls.borrow() },
        "chain": chain_facts(rpc, &blk)?,
        "floorProbe": if probe { probe_floor(rpc)? } else { Value::Null },
        "zeroGasPricesSubstituted": blk.substituted_gas_prices,
        "transactions": blk.txs.len(),
        "replayed": replayed,
        "recorded": recorded,
        "blockStateDiff": block_diff,
        "elapsedMs": started.elapsed().as_millis() as u64,
        // EVERY ROW'S FULL REPORT, recorded or not: a refused row has no capture
        // directory, and its outcome and reason are still part of the block.
        "perTransaction": tx_reports,
    });
    std::fs::write(
        out_dir.join("block.json"),
        serde_json::to_string_pretty(&block_report)?,
    )?;
    let diff_word = if block_diff.get("notMeasured").is_some() {
        "not measured"
    } else if block_diff["storageEntries"]["matches"] == json!(true)
        && block_diff["nonces"]["matches"] == json!(true)
        && block_diff["classHashes"]["matches"] == json!(true)
    {
        "MATCH"
    } else {
        "DIFF"
    };
    println!(
        "block {number}: {replayed}/{} reproduced, {recorded} recorded, block state diff {diff_word}, {} ms",
        blk.txs.len(),
        started.elapsed().as_millis()
    );
    for r in &tx_reports {
        println!(
            "  {} {} {:>9} steps  {}",
            r["txIndex"],
            r["txHash"].as_str().unwrap_or(""),
            r["vm"]["steps"],
            r["outcome"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// The chain's side of the capture, so a snapshot producer needs no network: the node's
/// tip and L1-accepted head at capture, its spec version, the block header, and every
/// transaction's body and receipt, verbatim.
fn chain_facts(rpc: &rpc::Rpc, blk: &block::Block) -> Result<Value> {
    let tip = rpc.call("starknet_blockNumber", json!([]))?;
    let l1 = rpc.call("starknet_getBlockWithTxHashes", json!(["l1_accepted"]))?;
    let spec = rpc.call("starknet_specVersion", json!([]))?;
    let control = match rpc.answer("thisMethodDoesNotExist", json!([]))? {
        rpc::Answer::Ok(_) => json!({ "classified": "ok" }),
        rpc::Answer::Error { code, .. } => json!({
            "classified": if code == -32601 { "absent" } else { "policy" }, "code": code,
        }),
    };
    let mut header = blk.header.clone();
    if let Some(o) = header.as_object_mut() {
        o.remove("transactions");
    }
    Ok(json!({
        "tip": tip,
        "l1Accepted": l1["block_number"],
        "specVersion": spec,
        "endpointControl": control,
        "header": header,
        "transactions": blk.txs.iter().map(|(h, b, r)| json!({
            "hash": h, "body": b, "receipt": r,
        })).collect::<Vec<_>>(),
    }))
}

/// THE LOWEST BLOCK THIS PIPELINE CAN REPLAY, and what it rests on — measured, with
/// controls, so the published floor is a reading and not a typed number.
///
/// Two limits compose and the higher one is the floor:
///
///   * the EXECUTOR's: the oldest Starknet version it carries versioned constants for.
///     Found by bisecting block versions with the executor's OWN predicate
///     (`VersionedConstants::get`), so the floor is what the executor refuses below and
///     not a version string somebody copied. Controls: the block below is refused and
///     the floor block is accepted, by that same call.
///   * the NODE's: whether it serves state at the floor's parent block at all. Read as
///     the class of a long-lived contract (the ETH fee token) at that block — positive
///     control: a non-zero class hash — against an address nothing was deployed at,
///     which must answer differently (negative control). And once BELOW the floor, so
///     the reading says which of the two limits binds.
fn probe_floor(rpc: &rpc::Rpc) -> Result<Value> {
    use blockifier::blockifier_versioned_constants::VersionedConstants;
    use starknet_api::block::StarknetVersion;
    use starknet_api::versioned_constants_logic::VersionedConstantsTrait;
    let tip = rpc
        .call("starknet_blockNumber", json!([]))?
        .as_u64()
        .ok_or_else(|| eyre!("starknet_blockNumber is not a number"))?;
    let version_at = |b: u64| -> Result<String> {
        let h = rpc.call(
            "starknet_getBlockWithTxHashes",
            json!([{ "block_number": b }]),
        )?;
        Ok(h["starknet_version"].as_str().unwrap_or("").to_string())
    };
    let accepted = |v: &str| -> bool {
        StarknetVersion::try_from(v.to_string())
            .ok()
            .map(|sv| VersionedConstants::get(&sv).is_ok())
            .unwrap_or(false)
    };
    if !accepted(&version_at(tip)?) {
        bail!("the executor carries no constants for the chain's TIP version; no floor exists");
    }
    let (mut lo, mut hi) = (0u64, tip);
    if accepted(&version_at(0)?) {
        hi = 0;
    } else {
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if accepted(&version_at(mid)?) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
    }
    let floor = hi;
    let below = if floor > 0 {
        Some(version_at(floor - 1)?)
    } else {
        None
    };
    let at = version_at(floor)?;
    const ETH: &str = "0x49d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7";
    const NOTHING: &str = "0xdead";
    let class_at = |addr: &str, b: u64| -> Result<Value> {
        Ok(
            match rpc.answer(
                "starknet_getClassHashAt",
                json!({ "block_id": { "block_number": b }, "contract_address": addr }),
            )? {
                rpc::Answer::Ok(v) => json!({ "classHash": v }),
                rpc::Answer::Error { code, message, .. } => {
                    json!({ "error": code, "message": message })
                }
            },
        )
    };
    let parent = floor.saturating_sub(1);
    let deep = parent / 2;
    let at_parent = class_at(ETH, parent)?;
    let nothing = class_at(NOTHING, parent)?;
    let below_floor = class_at(ETH, deep)?;
    let positive = at_parent
        .get("classHash")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c != "0x0");
    let negative = at_parent != nothing;
    Ok(json!({
        "pipelineFloor": floor,
        "executorFloor": {
            "block": floor,
            "version": at,
            "versionBelow": below,
            "predicate": "blockifier VersionedConstants::get(starknet_version)",
            "acceptedAtFloor": accepted(&at),
            "refusedBelow": below.as_deref().map(|v| !accepted(v)),
        },
        "nodeState": {
            "method": "starknet_getClassHashAt",
            "atFloorParent": { "block": parent, "address": ETH, "answer": at_parent },
            "negativeControl": { "block": parent, "address": NOTHING, "answer": nothing },
            "belowFloor": { "block": deep, "address": ETH, "answer": below_floor },
            "positiveControlHeld": positive,
            "negativeControlHeld": negative,
        },
        "binds": "executor",
        "tipAtProbe": tip,
    }))
}
