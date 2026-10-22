//! Binding a class on chain to the build that produced it (Recorder-Cairo CXP-CAIRO-4).
//!
//! A declared class carries no debug info, so on its own a pc maps to a Sierra statement
//! and no further. A build of the class's published source DOES carry it — the names of
//! every user function, type and libfunc, and from Cairo 2.8 on (with
//! `add-statements-code-locations-debug-info`) a Cairo file:line per statement — but a
//! build is a claim about the class, and Source-Resolution §4 makes that claim worth
//! nothing until it is checked against the chain.
//!
//! THE CHECK IS ON THE SIERRA PROGRAM ITSELF, NOT ON A NAME. `bind-class` compares the
//! build with the chain's own `getClass` answer component by component — the Sierra
//! program felts, the entry points, the class version, the ABI — and recomputes the class
//! hash from the chain's answer so the answer is known to be the class. Only when all of
//! them agree is a binding written. Because the two Sierra programs are the same felts,
//! statement `i` of the build IS statement `i` of the class the executor ran, so the
//! build's debug info applies to the recording statement for statement, with nothing
//! inferred. A trace re-checks the one thing it can (the Sierra program hash) before it
//! uses a binding, so a binding for a different class is never applied.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use cairo_lang_sierra::debug_info::DebugInfo;
use cairo_lang_starknet_classes::contract_class::ContractClass as CairoLangContractClass;
use eyre::{bail, eyre, Result};
use serde_json::{json, Value};
use starknet_api::core::ClassHash;
use starknet_api::state::SierraContractClass;
use starknet_types_core::felt::Felt;
use starknet_types_core::hash::{Poseidon, StarkHash};

use crate::state::ClassRecord;

pub const FORMAT: &str = "codetracer-cairo-onchain/class-source@1";

/// A binding as `bind-class` writes it, read back for a trace.
#[derive(Clone)]
pub struct Binding {
    pub class_hash: ClassHash,
    pub sierra_program_hash: Felt,
    pub debug_info: DebugInfo,
    /// The whole document, so a report can publish what it says about the build.
    pub doc: Value,
}

/// What a class's recording gained from its binding.
#[derive(Clone, Debug, Default)]
pub struct Bound {
    /// CASM offset -> the Cairo function whose code starts there.
    pub function_at: BTreeMap<usize, String>,
    /// Whether any statement carries a Cairo file:line (Cairo 2.8+ builds only).
    pub code_locations: bool,
    pub provenance: Value,
}

fn read_json(p: &Path) -> Result<Value> {
    let bytes = std::fs::read(p).map_err(|e| eyre!("{}: {e}", p.display()))?;
    let text = if p.extension().is_some_and(|x| x == "gz") {
        let mut s = String::new();
        flate2::read::GzDecoder::new(&bytes[..]).read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8(bytes)?
    };
    Ok(serde_json::from_str(&text)?)
}

fn felts(v: &Value) -> Result<Vec<Felt>> {
    v.as_array()
        .ok_or_else(|| eyre!("sierra_program is not an array"))?
        .iter()
        .map(|f| {
            f.as_str()
                .and_then(|s| Felt::from_hex(s).ok())
                .ok_or_else(|| eyre!("sierra_program holds a non-felt: {f}"))
        })
        .collect()
}

fn entry_points(v: &Value) -> Result<BTreeMap<String, Vec<(Felt, u64)>>> {
    let mut out = BTreeMap::new();
    for (k, list) in v.as_object().ok_or_else(|| eyre!("entry_points_by_type"))? {
        let mut eps = vec![];
        for ep in list.as_array().cloned().unwrap_or_default() {
            let sel = ep["selector"]
                .as_str()
                .and_then(|s| Felt::from_hex(s).ok())
                .ok_or_else(|| eyre!("entry point without a selector"))?;
            let idx = ep["function_idx"]
                .as_u64()
                .ok_or_else(|| eyre!("entry point without a function_idx"))?;
            eps.push((sel, idx));
        }
        eps.sort();
        out.insert(k.to_uppercase(), eps);
    }
    Ok(out)
}

/// `bind-class`: compare a build with the chain's class and, only if every component
/// agrees, write the binding. A disagreement is an error naming the component — the build
/// is discarded (Source-Resolution §4, "No match"), not published with a caveat.
pub fn bind(chain_class: &Path, built: &Path, class_hash: Felt, build: Value) -> Result<Value> {
    let mut chain = read_json(chain_class)?;
    // A transcript answer is the JSON-RPC envelope; a bare class is accepted too.
    if let Some(r) = chain.get("result") {
        chain = r.clone();
    }
    let built_doc = read_json(built)?;

    // The chain's answer must BE the class: its own components hash to the class hash.
    let chain_sierra: SierraContractClass = serde_json::from_value(chain.clone())
        .map_err(|e| eyre!("the chain answer is not a Sierra class: {e}"))?;
    let recomputed = chain_sierra.calculate_class_hash();
    if recomputed.0 != class_hash {
        bail!(
            "the chain answer hashes to {}, not to the class {} it was asked for",
            recomputed.0.to_hex_string(),
            class_hash.to_hex_string()
        );
    }

    let mut compared = serde_json::Map::new();
    let cs = felts(&chain["sierra_program"])?;
    let bs = felts(&built_doc["sierra_program"])?;
    if cs != bs {
        let at = cs.iter().zip(&bs).position(|(a, b)| a != b);
        bail!(
            "NO MATCH on the Sierra program: chain {} felts, build {} felts, first difference at {}",
            cs.len(),
            bs.len(),
            at.map_or("the end of the shorter".to_string(), |i| format!("felt {i}"))
        );
    }
    compared.insert(
        "sierraProgram".into(),
        json!(format!("equal, {} felts", cs.len())),
    );
    if entry_points(&chain["entry_points_by_type"])?
        != entry_points(&built_doc["entry_points_by_type"])?
    {
        bail!("NO MATCH on the entry points");
    }
    compared.insert("entryPoints".into(), json!("equal"));
    if chain["contract_class_version"] != built_doc["contract_class_version"] {
        bail!(
            "NO MATCH on the class version: chain {} build {}",
            chain["contract_class_version"],
            built_doc["contract_class_version"]
        );
    }
    compared.insert("contractClassVersion".into(), json!("equal"));
    // The chain holds the ABI as the string the declarer sent; the build writes it as
    // JSON. The class hash is over the string, which is why the hash is recomputed from
    // the chain's answer above rather than from the build; here the two are compared as
    // what they say.
    let chain_abi: Value = chain["abi"]
        .as_str()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(Value::Null);
    if chain_abi != built_doc["abi"] {
        bail!("NO MATCH on the ABI");
    }
    compared.insert("abi".into(), json!("equal as JSON"));
    compared.insert(
        "classHash".into(),
        json!("recomputed from the chain's answer: equal"),
    );

    let debug = built_doc
        .get("sierra_program_debug_info")
        .filter(|d| !d.is_null())
        .ok_or_else(|| eyre!("the build carries no sierra_program_debug_info"))?;
    let parsed: DebugInfo = serde_json::from_value(debug.clone())
        .map_err(|e| eyre!("sierra_program_debug_info: {e}"))?;
    if parsed.user_func_names.is_empty() {
        bail!(
            "the build's debug info names no function — build with `sierra-replace-ids = true` \
             (it does not change the Sierra program)"
        );
    }
    Ok(json!({
        "format": FORMAT,
        "classHash": class_hash.to_hex_string(),
        "sierraProgramHash": Poseidon::hash_array(&cs).to_hex_string(),
        "compared": compared,
        "build": build,
        "functions": parsed.user_func_names.len(),
        "codeLocations": parsed.annotations.contains_key(CODE_LOCATIONS),
        "debugInfo": debug,
    }))
}

const CODE_LOCATIONS: &str = "github.com/software-mansion/cairo-coverage";

/// Every binding in a directory, by class hash. A file that is not a binding is an error:
/// a sources directory holds nothing else.
pub fn load_dir(dir: &Path) -> Result<HashMap<ClassHash, Binding>> {
    let mut out = HashMap::new();
    for e in std::fs::read_dir(dir).map_err(|e| eyre!("{}: {e}", dir.display()))? {
        let p = e?.path();
        let doc = read_json(&p)?;
        if doc["format"] != FORMAT {
            bail!("{}: not a {FORMAT} document", p.display());
        }
        let hash = |k: &str| {
            doc[k]
                .as_str()
                .and_then(|s| Felt::from_hex(s).ok())
                .ok_or_else(|| eyre!("{}: no {k}", p.display()))
        };
        let class_hash = ClassHash(hash("classHash")?);
        let b = Binding {
            class_hash,
            sierra_program_hash: hash("sierraProgramHash")?,
            debug_info: serde_json::from_value(doc["debugInfo"].clone())
                .map_err(|e| eyre!("{}: debugInfo: {e}", p.display()))?,
            doc,
        };
        out.insert(class_hash, b);
    }
    Ok(out)
}

/// Apply a binding to a class this run loaded: re-check that it is the same Sierra
/// program, then re-render every statement with the build's names and name each Cairo
/// function at the CASM offset its code starts at.
pub fn apply(rec: &mut ClassRecord, b: &Binding) -> Result<Bound> {
    if b.class_hash != rec.class_hash {
        bail!(
            "the binding is for class {}, not {}",
            b.class_hash,
            rec.class_hash
        );
    }
    let sierra = felts(&rec.raw["sierra_program"])?;
    let h = Poseidon::hash_array(&sierra);
    if h != b.sierra_program_hash {
        bail!(
            "class {}: the binding is for Sierra program {}, the chain served {}",
            rec.class_hash,
            b.sierra_program_hash.to_hex_string(),
            h.to_hex_string()
        );
    }
    let for_extraction = CairoLangContractClass {
        sierra_program: sierra
            .iter()
            .map(|f| cairo_lang_utils::bigint::BigUintAsHex {
                value: f.to_biguint(),
            })
            .collect(),
        sierra_program_debug_info: None,
        contract_class_version: String::new(),
        entry_points_by_type: Default::default(),
        abi: None,
    };
    let mut program = for_extraction
        .extract_sierra_program(false)
        .map_err(|e| eyre!("class {}: extract Sierra program: {e}", rec.class_hash))?
        .program;
    if program.statements.len() != rec.sierra_statements.len() {
        bail!(
            "class {}: {} statements in the program, {} compiled",
            rec.class_hash,
            program.statements.len(),
            rec.sierra_statements.len()
        );
    }
    b.debug_info.populate(&mut program);
    for (st, s) in rec.sierra_statements.iter_mut().zip(&program.statements) {
        st.text = s.to_string();
    }
    let mut function_at = BTreeMap::new();
    for f in &program.funcs {
        if let (Some(name), Some(st)) =
            (&f.id.debug_name, rec.sierra_statements.get(f.entry_point.0))
        {
            function_at.insert(st.start, name.to_string());
        }
    }
    Ok(Bound {
        function_at,
        code_locations: b.debug_info.annotations.contains_key(CODE_LOCATIONS),
        provenance: json!({
            "classHash": b.doc["classHash"],
            "compared": b.doc["compared"],
            "build": b.doc["build"],
        }),
    })
}
