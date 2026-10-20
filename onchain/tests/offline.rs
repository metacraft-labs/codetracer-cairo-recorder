//! Offline end-to-end checks over a committed transcript of Starknet mainnet block
//! 16,664,301 (one INVOKE v3, Starknet 0.14.4), recorded from api.cartridge.gg.
//!
//! Each check has a control arm, so a pass is a verdict and not a default:
//!   * the block reproduces (receipt facts and the block's whole state diff) — and a
//!     transcript with ONE prestate value changed does not;
//!   * the recording's steps come from the VM — and with the VM observer switched off
//!     nothing is recorded at all, because no other source of steps exists.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const BLOCK: &str = "16664301";
const TX: &str = "0x295683d081359be8f7b3d2f7384d2e2c56bf683c14543cfb5b1f6729849c79d";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mainnet-16664301")
}

fn run(transcript: &Path, out: &Path, envs: &[(&str, &str)]) -> Value {
    let mut c = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-onchain"));
    c.args(["trace-block", BLOCK, "--transcript-in"])
        .arg(transcript)
        .arg("-o")
        .arg(out);
    for (k, v) in envs {
        c.env(k, v);
    }
    let o = c.output().expect("the recorder runs");
    assert!(
        o.status.success(),
        "trace-block failed: {}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_str(&std::fs::read_to_string(out.join("block.json")).unwrap()).unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ct-cairo-onchain-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to.join("answers")).unwrap();
    std::fs::copy(from.join("index.json"), to.join("index.json")).unwrap();
    for e in std::fs::read_dir(from.join("answers")).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join("answers").join(e.file_name())).unwrap();
    }
}

#[test]
fn the_block_reproduces_from_the_transcript() {
    let out = scratch("ok");
    let b = run(&fixture(), &out, &[]);
    assert_eq!(b["transactions"], 1);
    assert_eq!(b["replayed"], 1, "{b:#}");
    for k in ["storageEntries", "nonces", "classHashes"] {
        assert_eq!(b["blockStateDiff"][k]["matches"], true, "{k}: {b:#}");
    }
    let r: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(TX).join("report.json")).unwrap())
            .unwrap();
    assert_eq!(r["outcome"], "replayed");
    assert_eq!(r["effects"]["mismatched"], 0, "{:#}", r["effects"]);
    // Exact figures, so a change in what the VM ran is a visible change here.
    assert_eq!(r["vm"]["steps"], 18902);
    assert_eq!(
        r["container"]["steps"], 18903,
        "VM steps plus the writer's opening step"
    );
}

#[test]
fn control_one_changed_prestate_value_is_a_divergence() {
    // Change the value of the first storage read the node's simulation reported, in a
    // copy of the transcript. The replay must then disagree with the chain somewhere —
    // a comparison that cannot fire would read clean on this run too.
    let t = scratch("mutated-transcript");
    copy_dir(&fixture(), &t);
    let idx: Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("index.json")).unwrap()).unwrap();
    let (key, file) = idx["answers"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(k, _)| k.starts_with("starknet_simulateTransactions"))
        .map(|(k, f)| (k.clone(), f.as_str().unwrap().to_string()))
        .expect("the transcript prefetched through simulateTransactions");
    let path = t.join("answers").join(&file);
    let gz = std::fs::read(&path).unwrap();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut s).unwrap();
    let mut v: Value = serde_json::from_str(&s).unwrap();
    let reads = v["result"]["initial_reads"]["storage"]
        .as_array_mut()
        .unwrap();
    // The fee token's balance slots are what every transaction reads and writes; take
    // the first storage read that is one and add one to it.
    let target = reads
        .iter_mut()
        .find(|e| {
            e["contract_address"]
                .as_str()
                .unwrap_or("")
                .starts_with("0x4718f5a0fc34")
        })
        .unwrap_or_else(|| panic!("no fee-token read in {key}"));
    let old = u128::from_str_radix(
        target["value"].as_str().unwrap().trim_start_matches("0x"),
        16,
    )
    .unwrap();
    target["value"] = Value::String(format!("{:#x}", old + 1));
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, serde_json::to_string(&v).unwrap().as_bytes()).unwrap();
    std::fs::write(&path, enc.finish().unwrap()).unwrap();

    let b = run(&t, &scratch("mutated"), &[]);
    assert_eq!(
        b["blockStateDiff"]["storageEntries"]["matches"], false,
        "one prestate value changed by one and the block diff still matched: {b:#}"
    );
}

#[test]
fn control_without_the_vm_observer_nothing_is_recorded() {
    let out = scratch("no-observer");
    let b = run(
        &fixture(),
        &out,
        &[("CODETRACER_CAIRO_ONCHAIN_NO_OBSERVER", "1")],
    );
    assert_eq!(b["recorded"], 0, "{b:#}");
    // The execution itself is unaffected — the observer only watches.
    assert_eq!(b["replayed"], 1, "{b:#}");
    assert!(!out.join(TX).exists());
}

#[test]
fn two_runs_write_the_same_container_bytes() {
    // The recording id is derived from the block and the transaction and the listings
    // are interned relative to the capture directory, so where a run writes must not
    // change what it writes. A build that holds a published container to the one it
    // produces (blocktracer's F4) depends on exactly this.
    let a = scratch("bytes-a");
    let b = scratch("bytes-b-in-a-differently-named-directory");
    run(&fixture(), &a, &[]);
    run(&fixture(), &b, &[]);
    let name = format!("starknet-{BLOCK}-{TX}.ct");
    let x = std::fs::read(a.join(TX).join(&name)).unwrap();
    let y = std::fs::read(b.join(TX).join(&name)).unwrap();
    assert!(!x.is_empty());
    assert!(
        x == y,
        "two runs over one transcript wrote different containers"
    );
}

#[test]
fn a_block_of_an_era_the_executor_does_not_carry_is_refused_by_name() {
    // The same transcript with the block header's version rewritten to 0.12.3 — the era
    // below the oldest versioned constants the executor carries. The run must SUCCEED and
    // say so per row, rather than fail or record anything: a window over that era is
    // measured as zero, not missing.
    let t = scratch("old-era-transcript");
    copy_dir(&fixture(), &t);
    let idx: Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("index.json")).unwrap()).unwrap();
    let file = idx["answers"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(k, _)| k.starts_with("starknet_getBlockWithReceipts"))
        .map(|(_, f)| f.as_str().unwrap().to_string())
        .unwrap();
    let path = t.join("answers").join(&file);
    let gz = std::fs::read(&path).unwrap();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut s).unwrap();
    let mut v: Value = serde_json::from_str(&s).unwrap();
    v["result"]["starknet_version"] = Value::String("0.12.3".into());
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, serde_json::to_string(&v).unwrap().as_bytes()).unwrap();
    std::fs::write(&path, enc.finish().unwrap()).unwrap();

    let out = scratch("old-era");
    let b = run(&t, &out, &[]);
    assert_eq!(b["replayed"], 0, "{b:#}");
    assert_eq!(b["recorded"], 0, "{b:#}");
    let row = &b["perTransaction"][0];
    assert_eq!(row["outcome"], "refused");
    assert_eq!(row["refusalReason"], "runtime-refused");
    assert!(
        row["refusal"].as_str().unwrap().contains("0.12.3"),
        "the refusal names the era: {row:#}"
    );
    assert!(b["blockStateDiff"]["notMeasured"].is_string());
}

#[test]
fn control_a_mutated_call_tree_is_caught_by_the_differential() {
    // One selector changed in the node's trace of the block — the call-tree differential
    // must report the row `divergent` on `callTree` and nothing else, while every
    // consensus fact and the block diff still match. A differential that cannot fire would
    // read clean here too.
    let t = scratch("calltree-transcript");
    copy_dir(&fixture(), &t);
    let idx: Value =
        serde_json::from_str(&std::fs::read_to_string(t.join("index.json")).unwrap()).unwrap();
    let file = idx["answers"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(k, _)| k.starts_with("starknet_traceBlockTransactions"))
        .map(|(_, f)| f.as_str().unwrap().to_string())
        .unwrap();
    let path = t.join("answers").join(&file);
    let gz = std::fs::read(&path).unwrap();
    let mut s = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut s).unwrap();
    let mut v: Value = serde_json::from_str(&s).unwrap();
    v["result"][0]["trace_root"]["validate_invocation"]["entry_point_selector"] =
        Value::String("0x1".into());
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut enc, serde_json::to_string(&v).unwrap().as_bytes()).unwrap();
    std::fs::write(&path, enc.finish().unwrap()).unwrap();

    let b = run(&t, &scratch("calltree"), &[]);
    let row = &b["perTransaction"][0];
    assert_eq!(row["outcome"], "divergent", "{row:#}");
    let bad: Vec<&str> = row["effects"]["facts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["matches"] == false)
        .map(|f| f["field"].as_str().unwrap())
        .collect();
    assert_eq!(bad, vec!["callTree"]);
    assert_eq!(b["blockStateDiff"]["storageEntries"]["matches"], true);
}
