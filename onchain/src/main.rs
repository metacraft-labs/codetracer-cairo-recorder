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
        } => {
            let rpc = match (&transcript_in, &rpc_url) {
                (Some(p), _) => {
                    let (label, t) = transcript::read(p)?;
                    rpc::Rpc::replaying(&label, t)
                }
                (None, Some(u)) => rpc::Rpc::new(u),
                (None, None) => bail!("--rpc-url or --transcript-in is required"),
            };
            let r = trace_block(&rpc, block, &out_dir, &txs);
            if let Some(p) = transcript_out {
                let n = transcript::write(&p, rpc.url(), &rpc.transcript())?;
                eprintln!("transcript: {n} answer(s) -> {}", p.display());
            }
            r
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

fn trace_block(rpc: &rpc::Rpc, number: u64, out_dir: &Path, wanted: &[String]) -> Result<()> {
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
    let ctx = block::block_context(&blk)?;
    let old = block::old_block_hash(rpc, number)?;
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

    let reader = state::RpcState::new(rpc, number - 1);
    let classes = reader.classes.clone();
    let bodies: Vec<&Value> = blk.txs.iter().map(|(_, b, _)| b).collect();
    let prefetch = reader.prefetch(&bodies);
    let mut txs = vec![];
    for (h, body, _) in &blk.txs {
        txs.push(block::executable(&reader, h, body)?);
    }
    let mut exec = TransactionExecutor::pre_process_and_create(
        reader,
        ctx,
        old,
        TransactionExecutorConfig::default(),
    )
    .map_err(|e| eyre!("pre-process block {number}: {e}"))?;

    let mut executed = vec![];
    // THE CONTROL ARM FOR "THE STEPS ARE THE VM'S": with the observer not installed the
    // executor runs exactly as upstream, and this tool must then record NOTHING — there
    // is no other path that could fill a step stream in. `tests/offline.rs` runs it.
    let observe = std::env::var_os("CODETRACER_CAIRO_ONCHAIN_NO_OBSERVER").is_none();
    for (i, ((h, body, receipt), tx)) in blk.txs.iter().zip(&txs).enumerate() {
        if observe {
            bt_trace::install();
        }
        let result = exec.execute(tx).map_err(|e| e.to_string());
        let frames = bt_trace::take().map(|s| s.frames).unwrap_or_default();
        executed.push(Executed {
            hash: h.clone(),
            index: i,
            body: body.clone(),
            receipt: receipt.clone(),
            result,
            frames,
        });
    }
    let summary = exec
        .non_consuming_finalize(OsInitialReadsCollection::Skip)
        .map_err(|e| eyre!("finalize block {number}: {e}"))?;

    // The block's whole state diff against the node's.
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
    let block_diff = json!({
        "storageEntries": { "published": chain_s.len(), "replayed": our_s.len(), "matches": chain_s == our_s },
        "nonces": { "published": chain_n.len(), "replayed": our_n.len(), "matches": chain_n == our_n },
        "classHashes": { "published": chain_c.len(), "replayed": our_c.len(), "matches": chain_c == our_c },
    });

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
                facts.push(fact(
                    "l2Gas",
                    json!(r["execution_resources"]["l2_gas"]),
                    json!(info.receipt.gas.l2_gas.0),
                ));
                facts.push(fact(
                    "l1DataGas",
                    json!(r["execution_resources"]["l1_data_gas"]),
                    json!(info.receipt.gas.l1_data_gas.0),
                ));
                facts.push(fact(
                    "l1Gas",
                    json!(r["execution_resources"]["l1_gas"]),
                    json!(info.receipt.gas.l1_gas.0),
                ));
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
                // The call tree, entry point by entry point.
                let theirs = trace_calls(&trace_root);
                let mine: Vec<(Felt, Felt, Felt)> = e
                    .frames
                    .iter()
                    .map(|f| (*f.storage_address.0.key(), f.class_hash.0, f.selector.0))
                    .collect();
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
            report["notRecordedBecause"] = json!(if observe {
                "the executor ran no entry point for this transaction"
            } else {
                "CODETRACER_CAIRO_ONCHAIN_NO_OBSERVER is set: the VM's trace was not observed, and no other source of steps exists"
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
        for f in &e.frames {
            if listings.contains_key(&f.class_hash) {
                continue;
            }
            let rec = classes
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
            let l = listing::build(&rec);
            let p = src.join(format!("{}.casm", f.class_hash.0.to_hex_string()));
            std::fs::write(&p, &l.text)?;
            class_rows.push(json!({
                "classHash": rec.class_hash.0.to_hex_string(),
                "entryPoints": l.entry_names.values().collect::<Vec<_>>(),
                "cairo0": l.cairo0,
                "instructions": l.instructions,
                "listing": format!("sources/{}.casm", f.class_hash.0.to_hex_string()),
                "sierraVersion": rec.sierra_version.as_ref().map(|v| v.to_string()),
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
        let cx = record::Context {
            listings: &listings,
            paths: &paths,
            abis: &abis,
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
        "transactions": blk.txs.len(),
        "replayed": replayed,
        "recorded": recorded,
        "blockStateDiff": block_diff,
        "elapsedMs": started.elapsed().as_millis() as u64,
        "perTransaction": tx_reports.iter().map(|r| json!({
            "txHash": r["txHash"], "txIndex": r["txIndex"], "outcome": r["outcome"],
            "recorded": r["recorded"], "steps": r["vm"]["steps"],
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        out_dir.join("block.json"),
        serde_json::to_string_pretty(&block_report)?,
    )?;
    println!(
        "block {number}: {replayed}/{} reproduced, {recorded} recorded, block state diff {}, {} ms",
        blk.txs.len(),
        if block_diff["storageEntries"]["matches"] == json!(true)
            && block_diff["nonces"]["matches"] == json!(true)
        {
            "MATCH"
        } else {
            "DIFF"
        },
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
