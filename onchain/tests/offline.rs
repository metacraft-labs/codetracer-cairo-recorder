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

// ── Source binding (Recorder-Cairo CXP-CAIRO-4) ─────────────────────────────────────
//
// `tests/fixtures/class-sources/` holds ONE binding, written by `bind-class` from a build
// of argentlabs/argent-contracts-starknet @ account-0.4.0 (1352198956) with Scarb 2.6.3:
// the account class this block's transaction validates and executes through. The build
// itself (2 MB) is not committed; the tests below rebuild an equivalent document from the
// chain's own answer plus the binding's debug info, which is exactly what a matching
// build is. To reproduce the binding: check out the tag, append
//     [profile.srcmap]          inherits = "release"
//     [profile.srcmap.cairo]    sierra-replace-ids = true
// to Scarb.toml (names only — the Sierra program is unchanged, which `bind-class` checks),
// `scarb --profile srcmap build` with Scarb 2.6.3 (the repo's .tool-versions), then
//     codetracer-cairo-onchain bind-class <class> --chain-class <getClass answer> \
//       --built target/srcmap/argent_ArgentAccount.contract_class.json --build k=v… -o …

const ACCOUNT: &str = "0x36078334509b514626504edc9fb252328d1a240e4e948bef8d0c08dff45927f";

fn sources() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/class-sources")
}

fn gz_json(p: &Path) -> Value {
    use std::io::Read;
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(p).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    serde_json::from_str(&s).unwrap()
}

fn binding_file() -> PathBuf {
    sources().join(format!("{ACCOUNT}.json.gz"))
}

/// The chain's `getClass` answer for the account class, from the transcript.
fn chain_class_file() -> PathBuf {
    let idx: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture().join("index.json")).unwrap())
            .unwrap();
    let key = idx["answers"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(k, _)| k.starts_with("starknet_getClass ") && k.contains(&ACCOUNT[2..]))
        .map(|(_, v)| v.as_str().unwrap().to_string())
        .expect("the transcript holds the account class");
    fixture().join("answers").join(key)
}

fn class_rows(out: &Path) -> Vec<Value> {
    let r: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(TX).join("report.json")).unwrap())
            .unwrap();
    assert_eq!(r["outcome"], "replayed", "a binding never changes what ran");
    assert_eq!(r["vm"]["steps"], 18902);
    r["classes"].as_array().unwrap().clone()
}

fn listing(out: &Path) -> String {
    std::fs::read_to_string(out.join(TX).join("sources").join(format!("{ACCOUNT}.casm"))).unwrap()
}

fn run_with_sources(dir: &Path, out: &Path) {
    let o = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-onchain"))
        .args(["trace-block", BLOCK, "--transcript-in"])
        .arg(fixture())
        .arg("-o")
        .arg(out)
        .arg("--class-sources")
        .arg(dir)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn a_bound_class_is_listed_with_its_build_names() {
    let plain = scratch("src-plain");
    run(&fixture(), &plain, &[]);
    let bound = scratch("src-bound");
    run_with_sources(&sources(), &bound);

    let row = class_rows(&bound)
        .into_iter()
        .find(|c| c["classHash"] == ACCOUNT.replacen("0x0", "0x", 1))
        .expect("the account class ran");
    assert_eq!(row["source"]["bound"], true, "{row:#}");
    assert_eq!(row["source"]["functionsNamed"], 198);
    assert_eq!(
        row["source"]["codeLocations"], false,
        "Cairo 2.6.3 records no file:line"
    );
    assert_eq!(row["source"]["provenance"]["build"]["tag"], "account-0.4.0");
    for c in class_rows(&bound) {
        // Exactly the classes with a committed binding say they are bound.
        let has_binding = std::fs::read_dir(sources()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(c["classHash"].as_str().unwrap())
        });
        assert_eq!(c["source"]["bound"] == true, has_binding, "{c:#}");
    }

    // Every call target the plain listing could only name by pc is named by the build.
    let (p, b) = (listing(&plain), listing(&bound));
    assert_eq!(p.matches("\nfn@0x").count(), 148);
    assert_eq!(b.matches("\nfn@0x").count(), 0);
    assert!(b.contains(
        "\nargent::presets::argent_account::ArgentAccount::__wrapper__AccountImpl____validate__:\n"
    ));
    assert!(b.contains(
        "function_call<user@argent::presets::argent_account::ArgentAccount::AccountImpl::__validate__>"
    ));
    // Same instructions at the same pcs: the binding adds names, never code.
    let ins = |s: &str| {
        s.lines()
            .filter(|l| l.starts_with("0x"))
            .map(String::from)
            .collect::<Vec<_>>()
    };
    assert_eq!(ins(&p), ins(&b));
}

#[test]
fn control_a_binding_for_another_program_is_refused_and_not_applied() {
    // The same binding with its Sierra program hash changed in the last digit: what a
    // binding of a different build of this class would carry.
    let dir = scratch("src-tampered");
    std::fs::create_dir_all(&dir).unwrap();
    let mut doc = gz_json(&binding_file());
    let h = doc["sierraProgramHash"].as_str().unwrap().to_string();
    let last = h.chars().last().unwrap();
    let flipped = format!(
        "{}{}",
        &h[..h.len() - 1],
        if last == '0' { '1' } else { '0' }
    );
    doc["sierraProgramHash"] = Value::String(flipped);
    std::fs::write(dir.join(format!("{ACCOUNT}.json")), doc.to_string()).unwrap();

    let plain = scratch("src-plain2");
    run(&fixture(), &plain, &[]);
    let out = scratch("src-tampered-out");
    run_with_sources(&dir, &out);
    let row = class_rows(&out)
        .into_iter()
        .find(|c| !c["source"].is_null())
        .expect("the refusal is reported");
    assert_eq!(row["source"]["bound"], false);
    assert!(
        row["source"]["refused"]
            .as_str()
            .unwrap()
            .contains("the binding is for Sierra program"),
        "{row:#}"
    );
    assert_eq!(
        listing(&out),
        listing(&plain),
        "a refused binding changes nothing"
    );
}

fn bind(built: &Value, name: &str) -> std::process::Output {
    let d = scratch(name);
    std::fs::create_dir_all(&d).unwrap();
    let built_path = d.join("built.json");
    std::fs::write(&built_path, built.to_string()).unwrap();
    Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-onchain"))
        .args(["bind-class", ACCOUNT, "--chain-class"])
        .arg(chain_class_file())
        .arg("--built")
        .arg(&built_path)
        .args(["--build", "what=test"])
        .arg("-o")
        .arg(d.join("out.json"))
        .output()
        .unwrap()
}

/// A build document equivalent to a matching build: the chain's own class, its ABI as
/// JSON, and the committed binding's debug info.
fn matching_build() -> Value {
    let mut chain = gz_json(&chain_class_file());
    if let Some(r) = chain.get("result") {
        chain = r.clone();
    }
    let abi: Value = serde_json::from_str(chain["abi"].as_str().unwrap()).unwrap();
    chain["abi"] = abi;
    chain["sierra_program_debug_info"] = gz_json(&binding_file())["debugInfo"].clone();
    chain
}

#[test]
fn bind_class_binds_a_matching_build() {
    let o = bind(&matching_build(), "bind-ok");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(scratch_path("bind-ok").join("out.json")).unwrap(),
    )
    .unwrap();
    let committed = gz_json(&binding_file());
    assert_eq!(doc["sierraProgramHash"], committed["sierraProgramHash"]);
    assert_eq!(doc["compared"], committed["compared"]);
}

#[test]
fn control_bind_class_refuses_one_changed_sierra_felt() {
    let mut built = matching_build();
    let felts = built["sierra_program"].as_array_mut().unwrap();
    let (n, i) = (felts.len(), felts.len() / 2);
    felts[i] = Value::String("0x1234".into());
    let o = bind(&built, "bind-bad");
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains(&format!("NO MATCH on the Sierra program: chain {n} felts"))
            && err.contains(&format!("felt {i}")),
        "{err}"
    );
    assert!(
        !scratch_path("bind-bad").join("out.json").exists(),
        "a refusal writes nothing"
    );
}

/// `scratch` without the wipe, for reading what a step wrote.
fn scratch_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ct-cairo-onchain-{}-{name}", std::process::id()))
}

// ── Cairo source lines (a build with statement locations, its sources embedded) ──────
//
// The two StarkGate v3.0.0 token classes this block's transaction moves (STRK, ETH;
// starknet-io/starkgate-contracts @ 07e11c3, Apache-2.0, Scarb 2.15.1) are bound from a
// build with `add-statements-code-locations-debug-info`, with `--embed-sources`; so is
// Ekubo Core (EkuboProtocol/starknet-contracts v5.0.3, Scarb 2.20.0,
// LicenseRef-Ekubo-Proprietary — republished with its SPDX identifier by operator
// decision, 2026-10-22). The Argent account and Ekubo's Router are names only (Cairo
// 2.6.3 records no locations): those steps stay on CASM. USDC (Circle's FiatToken,
// Apache-2.0, Scarb 2.12.0) is bound from the source Voyager serves for its class hash,
// with locations and its texts embedded.

const STRK: &str = "0x2e77ee61d4df3d988ee1f42ea5442e913862cc82c2584d212ecda76666498fc";
const EKUBO_CORE: &str = "0x423df19e032f2d9bf9bb5bc1ea96db2b06d4c752c8c130cba7d577eed1de20a";
const ERC20: &str = "./sources/cairo/openzeppelin_token@3.0.0/src/erc20/erc20.cairo";

fn container_report(out: &Path) -> Value {
    let r: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join(TX).join("report.json")).unwrap())
            .unwrap();
    assert_eq!(r["outcome"], "replayed");
    assert_eq!(r["container"]["steps"], 18903);
    r
}

#[test]
fn steps_in_a_class_with_embedded_sources_are_on_cairo_lines() {
    let out = scratch("cairo-lines");
    run_with_sources(&sources(), &out);
    let r = container_report(&out);
    assert_eq!(r["container"]["cairoSteps"], 13195, "{:#}", r["container"]);
    let files: Vec<&str> = r["container"]["cairoSources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(files.len(), 53, "only files a step is shown in: {files:?}");
    assert!(files.contains(&ERC20));
    assert!(files
        .contains(&"./sources/cairo/starkgate-contracts/packages/strk/src/erc20_lockable.cairo"));
    assert!(files.contains(&"./sources/cairo/starknet-contracts/src/core.cairo"));
    assert!(files
        .iter()
        .any(|f| f.starts_with("./sources/cairo/stablecoin-starknet/packages/stablecoin/src/")));
    // Three compilers, three corelibs: each class's steps are in its own compiler's corelib.
    assert!(files
        .iter()
        .any(|f| f.starts_with("./sources/cairo/core@2.15.0/")));
    assert!(files
        .iter()
        .any(|f| f.starts_with("./sources/cairo/core@2.20.0/")));
    assert!(files
        .iter()
        .any(|f| f.starts_with("./sources/cairo/core@2.12.0/")));
    // The file written beside the container is the text the build read.
    let binding = gz_json(&sources().join(format!("{STRK}.json.gz")));
    let embedded = binding["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "openzeppelin_token@3.0.0/src/erc20/erc20.cairo")
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        std::fs::read_to_string(out.join(TX).join(ERC20)).unwrap(),
        embedded
    );
    let core = r["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["classHash"] == EKUBO_CORE)
        .unwrap()
        .clone();
    assert_eq!(core["source"]["bound"], true);
    assert_eq!(core["source"]["codeLocations"], true);
    assert!(core["cairoSources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| !v.as_str().unwrap().contains("starkgate-contracts")));
    // Per class, the files its own steps landed in — together, exactly the container's.
    let mut union: Vec<String> = r["classes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            c["cairoSources"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
        })
        .collect();
    union.sort();
    union.dedup();
    let mut all: Vec<String> = files.iter().map(|s| s.to_string()).collect();
    all.sort();
    assert_eq!(union, all);
    let strk = r["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["classHash"] == STRK)
        .unwrap()
        .clone();
    assert!(strk["cairoSources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str().unwrap().ends_with("erc20_lockable.cairo")));
    assert!(!strk["cairoSources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str().unwrap().ends_with("erc20_mintable.cairo")));
}

#[test]
fn control_the_same_bindings_without_their_texts_position_nothing_on_source() {
    let dir = scratch("cairo-no-text");
    std::fs::create_dir_all(&dir).unwrap();
    for e in std::fs::read_dir(sources()).unwrap() {
        let p = e.unwrap().path();
        let mut doc = gz_json(&p);
        if let Some(files) = doc["files"].as_array_mut() {
            for f in files {
                f.as_object_mut().unwrap().remove("text");
            }
        }
        let name = p.file_name().unwrap().to_string_lossy().replace(".gz", "");
        std::fs::write(dir.join(name), doc.to_string()).unwrap();
    }
    let out = scratch("cairo-no-text-out");
    run_with_sources(&dir, &out);
    let r = container_report(&out);
    assert_eq!(r["container"]["cairoSteps"], 0);
    assert_eq!(r["container"]["cairoSources"].as_array().unwrap().len(), 0);
    assert!(!out.join(TX).join("sources/cairo").exists());
    let bound = r["classes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["source"]["bound"] == true)
        .count();
    assert_eq!(bound, 6, "names still apply without the texts");
}

#[test]
fn control_an_embedded_text_that_is_not_the_file_the_build_read_is_refused() {
    let dir = scratch("cairo-bad-text");
    std::fs::create_dir_all(&dir).unwrap();
    let mut doc = gz_json(&sources().join(format!("{STRK}.json.gz")));
    let f = &mut doc["files"].as_array_mut().unwrap()[0];
    let t = f["text"].as_str().unwrap().replacen("fn ", "fn  ", 1);
    f["text"] = Value::String(t);
    std::fs::write(dir.join(format!("{STRK}.json")), doc.to_string()).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-onchain"))
        .args(["trace-block", BLOCK, "--transcript-in"])
        .arg(fixture())
        .arg("-o")
        .arg(scratch("cairo-bad-text-out"))
        .arg("--class-sources")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("does not hash to its sha256"));
}

#[test]
fn control_bind_class_refuses_a_build_whose_abi_renames_one_parameter() {
    // What circlefin/stablecoin-starknet's public source does against mainnet's USDC
    // class: the same Sierra program, but parameters named `to`/`from` where the deployed
    // ABI says `recipient`/`sender` — a different revision of the source. Showing it would
    // mislabel the parameters, so it is no match.
    let mut built = matching_build();
    let s = built["abi"].to_string();
    let i = s.find("\"name\":\"").expect("an ABI names things");
    let renamed = format!("{}\"name\":\"renamed_{}", &s[..i], &s[i + 8..]);
    built["abi"] = serde_json::from_str(&renamed).unwrap();
    let o = bind(&built, "bind-abi");
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("NO MATCH on the ABI"));
    assert!(!scratch_path("bind-abi").join("out.json").exists());
}

// ── An L1-initiated transaction (Recorder-Cairo CXP-CAIRO-4's awkward cases) ──────────
//
// Mainnet block 16,752,201 (Starknet 0.14.4) holds two transactions: index 0 is an
// L1_HANDLER — a StarkGate ETH deposit, started by a message from the L1 bridge
// 0xae0ee0a6…d419, with no Starknet sender at all — and index 1 an ordinary INVOKE.
// Committed transcript recorded 2026-10-22 from api.cartridge.gg.

#[test]
fn an_l1_handler_block_reproduces_from_its_transcript() {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mainnet-16752201");
    let out = scratch("l1h");
    let o = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-onchain"))
        .args(["trace-block", "16752201", "--transcript-in"])
        .arg(&fx)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let b: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("block.json")).unwrap()).unwrap();
    assert_eq!(b["transactions"], 2);
    assert_eq!(b["replayed"], 2, "{b:#}");
    for k in ["storageEntries", "nonces", "classHashes"] {
        assert_eq!(b["blockStateDiff"][k]["matches"], true, "{k}");
    }
    let rows = b["perTransaction"].as_array().unwrap();
    let l1 = &rows[0];
    assert_eq!(
        l1["txHash"],
        "0x677215c36be163338b1da157a5b02d0330ed75ea1a09723de810507b183600b"
    );
    assert_eq!(l1["outcome"], "replayed");
    assert_eq!(l1["vm"]["steps"], 1076);
    // CONTROL: the INVOKE beside it reproduces through the same path.
    assert_eq!(rows[1]["outcome"], "replayed");
    assert_eq!(rows[1]["vm"]["steps"], 9831);
}

// ── A class upgrade, and a block inside the documented hole ──────────────────────────
//
// Mainnet block 16,625,884 (18 transactions): index 0 upgrades account 0x6d626c63…e867 to
// Argent 0.4.0 (0x3607…927f) through `replace_class`; no other transaction changes a class.
// Mainnet block 1,948,151 (5 transactions) lies inside Chain-Support-Matrix §1.3's
// documented ~9,000-block hole, which this pipeline measured as no hole (CXP-CAIRO-1).
// Both transcripts recorded 2026-10-22 from api.cartridge.gg.

fn trace_fixture(name: &str, block: &str) -> Value {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    // Unique per call: tests run in parallel and two of them replay the same fixture.
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = scratch(&format!("{name}-{n}"));
    let o = Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-onchain"))
        .args(["trace-block", block, "--transcript-in"])
        .arg(&fx)
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let b =
        serde_json::from_str(&std::fs::read_to_string(out.join("block.json")).unwrap()).unwrap();
    // block.json carries every transaction's full report; the containers beside it are
    // not read here, and a 16-transaction block's are hundreds of megabytes.
    let _ = std::fs::remove_dir_all(&out);
    b
}

#[test]
fn an_upgrade_is_reproduced_and_named_on_its_own_transaction() {
    let b = trace_fixture("mainnet-16625884", "16625884");
    assert_eq!(
        (b["transactions"].as_u64(), b["replayed"].as_u64()),
        (Some(18), Some(18)),
        "{b:#}"
    );
    assert_eq!(b["blockStateDiff"]["classHashes"]["matches"], true);
    let rows = b["perTransaction"].as_array().unwrap();
    assert_eq!(
        rows[0]["classChanges"],
        serde_json::json!([{
            "contract": "0x6d626c63a40c7ed17053baed7c19ef03b6bb41a491fa0e71cf2d1871b4ce867",
            "classHash": "0x36078334509b514626504edc9fb252328d1a240e4e948bef8d0c08dff45927f",
            "kind": "replaced",
        }])
    );
    // CONTROL: every other transaction of the block changed no class.
    for r in &rows[1..] {
        assert_eq!(r["classChanges"], serde_json::json!([]), "{}", r["txHash"]);
    }
}

#[test]
fn a_block_inside_the_documented_hole_reproduces() {
    let b = trace_fixture("mainnet-1948151", "1948151");
    assert_eq!(
        (b["transactions"].as_u64(), b["replayed"].as_u64()),
        (Some(5), Some(5)),
        "{b:#}"
    );
    for k in ["storageEntries", "nonces", "classHashes"] {
        assert_eq!(b["blockStateDiff"][k]["matches"], true, "{k}");
    }
}

// ── The divergence corpus (Recorder-Cairo CXP-CAIRO-4; Blockchain-Recorder-Test-Suites §4, §7)
//
// `corpus/starknet-divergence-corpus.json` declares every taxonomy cell applicable to
// Starknet, and for each either the captured cases in it or why it is uncovered. This
// test replays EVERY committed fixture, computes each measurable cell's cases from what
// the replays report, and requires the manifest to say exactly that — in both
// directions. A cell that gains a case the manifest does not list fails, and so does a
// listed case the replays do not show, so "covered" can neither go stale nor be claimed.
// Every case must also have reproduced every counted fact. `CORPUS_WRITE=1` prints the
// measured reading instead, for a deliberate update.

const CORPUS_FIXTURES: &[(&str, &str)] = &[
    ("mainnet-16664301", "16664301"),
    ("mainnet-16752201", "16752201"),
    ("mainnet-16625884", "16625884"),
    ("mainnet-1948151", "1948151"),
    ("mainnet-800008", "800008"),
    ("mainnet-1014529", "1014529"),
];

fn measured_cells() -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut c: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |cell: String, case: String| {
        c.entry(cell).or_default().insert(case);
    };
    for (dir, block) in CORPUS_FIXTURES {
        let b = trace_fixture(dir, block);
        assert_eq!(
            b["replayed"], b["transactions"],
            "{dir}: every case reproduces"
        );
        for k in ["storageEntries", "nonces", "classHashes"] {
            assert_eq!(b["blockStateDiff"][k]["matches"], true, "{dir}: {k}");
        }
        add(
            format!("4.1/era/{}", b["starknetVersion"].as_str().unwrap()),
            block.to_string(),
        );
        for r in b["perTransaction"].as_array().unwrap() {
            let case = format!("{block}:{}", r["txIndex"]);
            let (ty, ver) = (r["type"].as_str().unwrap(), r["version"].as_str().unwrap());
            add(format!("4.2/type/{ty}/{ver}"), case.clone());
            let fact = |f: &str| {
                r["effects"]["facts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|x| x["field"] == f)
                    .map(|x| x["published"].clone())
                    .unwrap_or(Value::Null)
            };
            let status = fact("executionStatus");
            if ty == "L1_HANDLER" {
                add("4.3/l1-handler".into(), case.clone());
            }
            if ty == "DEPLOY_ACCOUNT" {
                add("4.5/deploy-account".into(), case.clone());
            }
            if ty == "DECLARE" {
                add("4.5/declare".into(), case.clone());
            }
            for ch in r["classChanges"].as_array().unwrap() {
                match (ch["kind"].as_str(), ty) {
                    (Some("deployed"), t) if t != "DEPLOY_ACCOUNT" => {
                        add("4.5/deploy-from-a-call".into(), case.clone())
                    }
                    (Some("replaced"), _) => add("4.6/replace-class".into(), case.clone()),
                    _ => {}
                }
            }
            let frames = r["frames"].as_array().unwrap();
            let parent: std::collections::HashMap<u64, Option<u64>> = frames
                .iter()
                .map(|f| (f["id"].as_u64().unwrap(), f["parent"].as_u64()))
                .collect();
            let contract: std::collections::HashMap<u64, &str> = frames
                .iter()
                .map(|f| (f["id"].as_u64().unwrap(), f["contract"].as_str().unwrap()))
                .collect();
            if frames.iter().any(|f| f["callType"] == "Delegate") {
                add("4.7/library-call".into(), case.clone());
            }
            let chain = |mut i: u64| {
                let mut v = vec![i];
                while let Some(Some(p)) = parent.get(&i) {
                    v.push(*p);
                    i = *p;
                }
                v
            };
            if frames.iter().any(|f| {
                let ids = chain(f["id"].as_u64().unwrap());
                let mut seen = BTreeSet::new();
                ids.iter().any(|i| !seen.insert(contract[i]))
            }) {
                add("4.7/reentrancy".into(), case.clone());
            }
            if frames
                .iter()
                .any(|f| chain(f["id"].as_u64().unwrap()).len() >= 4)
            {
                add("4.7/entry-point-depth-4".into(), case.clone());
            }
            if status == "REVERTED" {
                add("4.8/reverted".into(), case.clone());
            }
            if status == "SUCCEEDED" && r["vm"]["framesWithError"].as_u64().unwrap_or(0) > 0 {
                add(
                    "4.8/failed-call-in-a-succeeding-transaction".into(),
                    case.clone(),
                );
            }
            if r["vm"]["cairo0Frames"].as_u64().unwrap_or(0) > 0 {
                add("4.10/cairo0-class".into(), case.clone());
            }
            for cl in r["classes"].as_array().unwrap() {
                if let Some(v) = cl["sierraVersion"].as_str() {
                    add(
                        format!("4.10/sierra/{v}"),
                        cl["classHash"].as_str().unwrap().to_string(),
                    );
                }
            }
            if fact("messagesSent").as_u64().unwrap_or(0) > 0 {
                add("4.11/l2-to-l1-message".into(), case.clone());
            }
            match ver {
                "0x3" => add("4.11/fee-in-strk".into(), case.clone()),
                "0x1" | "0x2" if ty != "L1_HANDLER" => add("4.11/fee-in-eth".into(), case.clone()),
                _ => {}
            }
        }
    }
    c
}

#[test]
fn the_divergence_corpus_covers_exactly_what_it_declares() {
    let measured = measured_cells();
    if std::env::var("CORPUS_WRITE").is_ok() {
        println!("{}", serde_json::to_string_pretty(&measured).unwrap());
        return;
    }
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/starknet-divergence-corpus.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let cells = manifest["cells"].as_object().unwrap();
    let mut problems = vec![];
    for (id, cases) in &measured {
        match cells.get(id) {
            None => problems.push(format!(
                "{id}: measured {} case(s); the manifest has no such cell",
                cases.len()
            )),
            Some(cell) => {
                let listed: std::collections::BTreeSet<String> = cell["cases"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                if &listed != cases {
                    problems.push(format!("{id}: listed {listed:?}, measured {cases:?}"));
                }
            }
        }
    }
    for (id, cell) in cells {
        let listed = cell["cases"].as_array().map(|a| a.len()).unwrap_or(0);
        if listed > 0 && !measured.contains_key(id) {
            problems.push(format!(
                "{id}: lists {listed} case(s) the replays do not show"
            ));
        }
        if listed == 0 && cell["uncovered"].as_str().unwrap_or("").is_empty() {
            problems.push(format!(
                "{id}: no case and no stated reason — an uncovered cell says why"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "the corpus manifest disagrees with the replays:\n{}",
        problems.join("\n")
    );
}
