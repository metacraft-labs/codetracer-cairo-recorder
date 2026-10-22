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
use std::path::{Path, PathBuf};

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
    /// Per Sierra statement, `(file, line, column)` — 1-based — or none.
    pub locations: Vec<Option<(usize, u32, u32)>>,
    /// Per file index: its stable path, and its text when the binding embeds it.
    pub files: Vec<(String, Option<String>)>,
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
    /// pc -> `(file, line, column)` for every pc whose statement has a location in a file
    /// the binding embeds. A pc absent here stays on its CASM listing line.
    pub pc_location: HashMap<usize, (usize, u32, u32)>,
    /// The embedded files, by index: stable path and text.
    pub files: Vec<(String, Option<String>)>,
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
pub fn bind(
    chain_class: &Path,
    built: &Path,
    class_hash: Felt,
    build: Value,
    roots: &[(String, PathBuf)],
    embed: bool,
) -> Result<Value> {
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
    // The names are kept; the tool annotations are not — they carry the build machine's
    // absolute paths. Code locations are re-stated below in machine-independent form.
    let mut names_only = debug.clone();
    if let Some(o) = names_only.as_object_mut() {
        o.remove("annotations");
    }
    let (locations, files) = match parsed.annotations.get(CODE_LOCATIONS) {
        Some(a) => code_locations(a, cs_statements(&cs)?, roots, embed)?,
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "format": FORMAT,
        "classHash": class_hash.to_hex_string(),
        "sierraProgramHash": Poseidon::hash_array(&cs).to_hex_string(),
        "compared": compared,
        "build": build,
        "functions": parsed.user_func_names.len(),
        "codeLocations": !locations.is_null(),
        "debugInfo": names_only,
        "locations": locations,
        "files": files,
    }))
}

/// How many statements the class's Sierra program has.
fn cs_statements(sierra: &[Felt]) -> Result<usize> {
    let cc = CairoLangContractClass {
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
    Ok(cc
        .extract_sierra_program(false)
        .map_err(|e| eyre!("extract Sierra program: {e}"))?
        .program
        .statements
        .len())
}

/// A build path, named so it means the same on every machine: under a `--root LABEL=DIR`
/// it is `LABEL/<relative>`; in Scarb's cache it is the package and version it belongs to
/// (`core@2.15.0/src/integer.cairo`, `openzeppelin_token@3.0.0/src/erc20/erc20.cairo`,
/// `starkware-starknet-utils@8f41888/packages/…`). A path that is neither is refused:
/// nothing machine-specific may reach a published binding.
fn stable_path(p: &str, roots: &[(String, PathBuf)]) -> Result<String> {
    for (label, dir) in roots {
        if let Ok(rel) = Path::new(p).strip_prefix(dir) {
            return Ok(format!("{label}/{}", rel.display()));
        }
    }
    let after = |m: &str| p.find(m).map(|i| &p[i + m.len()..]);
    if let Some(rest) = after("/registry/std/") {
        // v2.15.0/core/src/x.cairo
        let mut it = rest.splitn(3, '/');
        if let (Some(v), Some(pkg), Some(tail)) = (it.next(), it.next(), it.next()) {
            return Ok(format!("{pkg}@{}/{tail}", v.trim_start_matches('v')));
        }
    }
    if let Some(rest) = after("/registry/src/") {
        // <registry>/<pkg>-<ver>/x.cairo
        let mut it = rest.splitn(3, '/');
        if let (Some(_), Some(pv), Some(tail)) = (it.next(), it.next(), it.next()) {
            if let Some((pkg, ver)) = pv.rsplit_once('-') {
                return Ok(format!("{pkg}@{ver}/{tail}"));
            }
        }
    }
    if let Some(rest) = after("/registry/git/checkouts/") {
        // <name>-<hash>/<rev>/x.cairo
        let mut it = rest.splitn(3, '/');
        if let (Some(nh), Some(rev), Some(tail)) = (it.next(), it.next(), it.next()) {
            let name = nh.rsplit_once('-').map_or(nh, |(n, _)| n);
            return Ok(format!("{name}@{rev}/{tail}"));
        }
    }
    bail!("source path {p} is under no --root and not in Scarb's cache; name its root")
}

/// Per Sierra statement, the ONE Cairo location a step there is shown at, and the files
/// those locations name.
///
/// A statement's locations are the inlining chain the compiler recorded, innermost first,
/// each flagged when it is macro-generated. The location used is the innermost one that
/// is not macro-generated — the code that actually emitted the statement — and, when
/// every entry is generated, the innermost one. A statement with no location has `null`.
///
/// With `embed`, each named file's text is carried with its sha256, read from the build
/// tree at bind time; without it only the sha256 is, and a trace positions on CASM.
fn code_locations(
    annotation: &Value,
    statements: usize,
    roots: &[(String, PathBuf)],
    embed: bool,
) -> Result<(Value, Value)> {
    let map = annotation["statements_code_locations"]
        .as_object()
        .ok_or_else(|| eyre!("{CODE_LOCATIONS}: no statements_code_locations"))?;
    let mut file_idx: BTreeMap<String, usize> = BTreeMap::new();
    let mut file_abs: Vec<String> = vec![];
    let mut out = vec![Value::Null; statements];
    for (k, entries) in map {
        let idx: usize = k.parse().map_err(|_| eyre!("statement index {k}"))?;
        if idx >= statements {
            bail!("a location for statement {idx}, past the program's {statements}");
        }
        let entries = entries.as_array().cloned().unwrap_or_default();
        let pick = entries
            .iter()
            .find(|e| e[2] == Value::Bool(false))
            .or_else(|| entries.first());
        let Some(e) = pick else { continue };
        let abs = e[0]
            .as_str()
            .ok_or_else(|| eyre!("a location without a file"))?;
        let name = stable_path(abs, roots)?;
        let n = file_idx.len();
        let fi = *file_idx.entry(name).or_insert_with(|| {
            file_abs.push(abs.to_string());
            n
        });
        let start = &e[1]["start"];
        // The compiler counts lines and columns from 0.
        out[idx] = json!([
            fi,
            start["line"].as_u64().unwrap_or(0) + 1,
            start["col"].as_u64().unwrap_or(0) + 1
        ]);
    }
    let mut files = vec![];
    for (name, i) in &file_idx {
        let bytes = std::fs::read(&file_abs[*i])
            .map_err(|e| eyre!("{}: {e} (the build tree must still exist)", file_abs[*i]))?;
        let sha = sha256_hex(&bytes);
        let mut f = json!({ "index": i, "path": name, "sha256": sha });
        if embed {
            f["text"] = json!(String::from_utf8(bytes)?);
        }
        files.push(f);
    }
    files.sort_by_key(|f| f["index"].as_u64());
    Ok((Value::Array(out), Value::Array(files)))
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
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
        let mut files = vec![];
        for f in doc["files"].as_array().cloned().unwrap_or_default() {
            let path = f["path"].as_str().unwrap_or("").to_string();
            let text = f["text"].as_str().map(String::from);
            // An embedded text is the file the build read, or it is not used.
            if let Some(t) = &text {
                if Some(sha256_hex(t.as_bytes()).as_str()) != f["sha256"].as_str() {
                    bail!(
                        "{}: the embedded {path} does not hash to its sha256",
                        p.display()
                    );
                }
            }
            files.push((path, text));
        }
        let locations = doc["locations"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|l| {
                l.as_array().map(|a| {
                    let n = |i: usize| a.get(i).and_then(|v| v.as_u64()).unwrap_or(0);
                    (n(0) as usize, n(1) as u32, n(2) as u32)
                })
            })
            .collect();
        let b = Binding {
            class_hash,
            sierra_program_hash: hash("sierraProgramHash")?,
            debug_info: serde_json::from_value(doc["debugInfo"].clone())
                .map_err(|e| eyre!("{}: debugInfo: {e}", p.display()))?,
            locations,
            files,
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
    let mut pc_location = HashMap::new();
    if !b.locations.is_empty() {
        if b.locations.len() != rec.sierra_statements.len() {
            bail!(
                "class {}: the binding locates {} statements, the program has {}",
                rec.class_hash,
                b.locations.len(),
                rec.sierra_statements.len()
            );
        }
        for (st, loc) in rec.sierra_statements.iter().zip(&b.locations) {
            if let Some(l) = loc {
                if b.files.get(l.0).is_some_and(|f| f.1.is_some()) {
                    for pc in st.start..st.end {
                        pc_location.insert(pc, *l);
                    }
                }
            }
        }
    }
    Ok(Bound {
        function_at,
        code_locations: !b.locations.is_empty(),
        pc_location,
        files: b.files.clone(),
        provenance: json!({
            "classHash": b.doc["classHash"],
            "compared": b.doc["compared"],
            "build": b.doc["build"],
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_path_is_named_the_same_on_every_machine() {
        let roots = vec![("repo".to_string(), PathBuf::from("/w/checkout"))];
        let n = |p: &str| stable_path(p, &roots).unwrap();
        assert_eq!(
            n("/w/checkout/packages/a/src/x.cairo"),
            "repo/packages/a/src/x.cairo"
        );
        assert_eq!(
            n("/home/u/.cache/scarb/registry/std/v2.15.0/core/src/integer.cairo"),
            "core@2.15.0/src/integer.cairo"
        );
        assert_eq!(
            n("/c/registry/src/scarbs.xyz-9djtpev4jug5q/openzeppelin_token-3.0.0/src/erc20/erc20.cairo"),
            "openzeppelin_token@3.0.0/src/erc20/erc20.cairo"
        );
        assert_eq!(
            n("/c/registry/git/checkouts/starkware-starknet-utils-co5ag083kk1bq/8f41888/packages/utils/src/errors.cairo"),
            "starkware-starknet-utils@8f41888/packages/utils/src/errors.cairo"
        );
        // CONTROL: a path under no root and outside Scarb's cache is refused, not leaked.
        assert!(stable_path("/home/u/elsewhere/x.cairo", &roots).is_err());
    }
}
