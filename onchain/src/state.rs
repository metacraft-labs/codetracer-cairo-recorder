//! The prestate: chain state at the PARENT block, read one leaf at a time from a node.
//!
//! The executor is StarkWare's own (`blockifier`), and it is the executor that folds
//! transactions 0..k-1 of the block forward before transaction k runs — so what this
//! reader has to serve is exactly the state the block started from, at block N-1, and
//! nothing about the block itself. That is the whole of the prestate strategy: read the
//! parent, replay the preceding transactions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::rc::Rc;

use base64::Engine;
use blockifier::execution::contract_class::RunnableCompiledClass;
use blockifier::state::errors::StateError;
use blockifier::state::state_api::{StateReader, StateResult};
use cairo_lang_starknet_classes::allowed_libfuncs::ListSelector;
use cairo_lang_starknet_classes::casm_contract_class::CasmContractClass;
use cairo_lang_starknet_classes::contract_class::{
    ContractClass as CairoLangContractClass, ContractEntryPoint, ContractEntryPoints,
};
use cairo_lang_utils::bigint::BigUintAsHex;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use starknet_api::contract_class::{ContractClass, EntryPointType, SierraVersion};
use starknet_api::core::{
    ClassHash, CompiledClassHash, ContractAddress, EntryPointSelector, Nonce,
};
use starknet_api::deprecated_contract_class::{
    ContractClass as DeprecatedContractClass, EntryPointOffset, EntryPointV0, Program,
};
use starknet_api::state::{SierraContractClass, StorageKey};
use starknet_types_core::felt::Felt;

use crate::rpc::{Answer, Rpc, CLASS_HASH_NOT_FOUND, CONTRACT_NOT_FOUND};

/// What this run learned about one class, kept so the recorder can write a listing of
/// exactly the program the VM ran.
#[derive(Clone)]
pub struct ClassRecord {
    pub class_hash: ClassHash,
    /// `None` for a Cairo 0 class.
    pub casm: Option<CasmContractClass>,
    pub sierra_version: Option<SierraVersion>,
    /// The Cairo 0 program, for a Cairo 0 class.
    pub deprecated: Option<DeprecatedContractClass>,
    /// The class's ABI as the chain serves it (a JSON string for Sierra classes).
    pub abi: Value,
    /// Raw `getClass` answer — the sierra program felts are needed for the Sierra listing.
    pub raw: Value,
    /// Per Sierra statement, in statement order: the CASM bytecode range it compiled to and
    /// its text. From the SAME compilation that produced `casm`, so a pc maps to the
    /// statement it implements exactly. Empty for a Cairo 0 class.
    pub sierra_statements: Vec<SierraStatement>,
}

#[derive(Clone, Debug)]
pub struct SierraStatement {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// The classes a run loaded, shared so the recorder can still read them after the
/// executor has taken ownership of the reader.
pub type Classes = Rc<RefCell<HashMap<ClassHash, ClassRecord>>>;

pub struct RpcState<'a> {
    pub rpc: &'a Rpc,
    /// The parent block every read is taken at.
    pub block: u64,
    pub classes: Classes,
    /// The node's own answer to "which leaves does this block read, and what did they
    /// hold at the parent block" — see [`RpcState::prefetch`]. A read it does not hold
    /// falls through to a single-leaf query, so a prefetch that missed something costs
    /// time and never correctness.
    prefetched: RefCell<Prefetched>,
    pub prefetch_hits: RefCell<u64>,
}

#[derive(Default)]
struct Prefetched {
    storage: HashMap<(Felt, Felt), Felt>,
    nonces: HashMap<Felt, Felt>,
    class_hashes: HashMap<Felt, Felt>,
}

/// What a prefetch did, so the run can say it rather than leave it to be inferred.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PrefetchReport {
    pub simulated: usize,
    pub skipped: usize,
    pub storage: usize,
    pub nonces: usize,
    pub class_hashes: usize,
    pub refused: Option<String>,
}

fn felt(v: &Value) -> StateResult<Felt> {
    let s = v
        .as_str()
        .ok_or_else(|| StateError::StateReadError(format!("not a felt string: {v}")))?;
    Felt::from_hex(s).map_err(|e| StateError::StateReadError(format!("{s}: {e}")))
}

fn read_err(e: eyre::Report) -> StateError {
    StateError::StateReadError(e.to_string())
}

impl<'a> RpcState<'a> {
    pub fn new(rpc: &'a Rpc, block: u64) -> Self {
        Self {
            rpc,
            block,
            classes: Rc::new(RefCell::new(HashMap::new())),
            prefetched: RefCell::new(Prefetched::default()),
            prefetch_hits: RefCell::new(0),
        }
    }

    /// ASK THE NODE FOR THE BLOCK'S READ SET IN ONE CALL, instead of one call per leaf.
    ///
    /// `starknet_simulateTransactions` at the PARENT block, with the
    /// `RETURN_INITIAL_READS` flag, over the block's own transactions in block order,
    /// answers `initial_reads` — every leaf the sequence read, with the value it held at
    /// the parent. A leaf an earlier transaction WROTE is read back from the
    /// simulation's own cache and so is not an initial read; what comes back is exactly
    /// the parent-block values, which is what this reader serves. StarkWare's
    /// re-execution tool prefetches the same way.
    ///
    /// Only INVOKE and DEPLOY_ACCOUNT v3 bodies can be submitted for simulation; the
    /// others are left out and their reads fall through to single-leaf queries.
    pub fn prefetch(&self, bodies: &[&Value]) -> PrefetchReport {
        let mut report = PrefetchReport::default();
        let mut txs = vec![];
        for b in bodies {
            let ty = b["type"].as_str().unwrap_or("");
            let version = b["version"].as_str().unwrap_or("");
            if (ty == "INVOKE" || ty == "DEPLOY_ACCOUNT") && version == "0x3" {
                let mut t = (*b).clone();
                if let Some(o) = t.as_object_mut() {
                    o.remove("transaction_hash");
                }
                txs.push(t);
            } else {
                report.skipped += 1;
            }
        }
        report.simulated = txs.len();
        if txs.is_empty() {
            return report;
        }
        // THE SIMULATION'S SHAPE NEVER CHANGES WHAT A PREFETCHED VALUE MEANS. Whatever
        // flags it ran under, and whether it ran the block whole or one transaction at a
        // time, an `initial_reads` entry is a leaf's value AT THE PARENT BLOCK — so a
        // relaxed simulation can only read FEWER leaves (they fall through to single
        // queries), never a wrong one. That is why the fallbacks below are safe: the
        // simulator refuses a whole batch when one transaction's resource bounds exceed
        // its balance at simulation time (measured, block 16,664,437 index 1), so the
        // batch is retried with fee charging and then validation relaxed, and finally
        // one transaction at a time.
        let mut batches: Vec<(Vec<Value>, &[&str])> = vec![
            (txs.clone(), &["RETURN_INITIAL_READS"]),
            (txs.clone(), &["RETURN_INITIAL_READS", "SKIP_FEE_CHARGE"]),
            (
                txs.clone(),
                &["RETURN_INITIAL_READS", "SKIP_FEE_CHARGE", "SKIP_VALIDATE"],
            ),
        ];
        let mut all_reads = vec![];
        let mut refusals = vec![];
        let mut attempt = |batch: Vec<Value>, flags: &[&str]| -> Option<Value> {
            let params = json!({
                "block_id": self.block_id(),
                "transactions": batch,
                "simulation_flags": flags,
            });
            match self.rpc.answer("starknet_simulateTransactions", params) {
                Ok(Answer::Ok(v)) => v.get("initial_reads").cloned(),
                Ok(Answer::Error {
                    code,
                    message,
                    data,
                }) => {
                    refusals.push(format!(
                        "{flags:?}: {code} {message} {}",
                        data.map(|d| d.to_string()).unwrap_or_default()
                    ));
                    None
                }
                Err(e) => {
                    refusals.push(format!("{flags:?}: {e}"));
                    None
                }
            }
        };
        let mut whole = None;
        for (batch, flags) in batches.drain(..) {
            if let Some(r) = attempt(batch, flags) {
                whole = Some(r);
                break;
            }
        }
        match whole {
            Some(r) => all_reads.push(r),
            None => {
                for t in &txs {
                    if let Some(r) = attempt(
                        vec![t.clone()],
                        &["RETURN_INITIAL_READS", "SKIP_FEE_CHARGE", "SKIP_VALIDATE"],
                    ) {
                        all_reads.push(r);
                    }
                }
            }
        }
        if !refusals.is_empty() {
            report.refused = Some(refusals.join(" | "));
        }
        let f = |v: &Value| v.as_str().and_then(|s| Felt::from_hex(s).ok());
        let mut p = self.prefetched.borrow_mut();
        for reads in all_reads {
            for e in reads["storage"].as_array().cloned().unwrap_or_default() {
                if let (Some(a), Some(k), Some(v)) =
                    (f(&e["contract_address"]), f(&e["key"]), f(&e["value"]))
                {
                    p.storage.insert((a, k), v);
                }
            }
            for e in reads["nonces"].as_array().cloned().unwrap_or_default() {
                if let (Some(a), Some(n)) = (f(&e["contract_address"]), f(&e["nonce"])) {
                    p.nonces.insert(a, n);
                }
            }
            for e in reads["class_hashes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                if let (Some(a), Some(c)) = (f(&e["contract_address"]), f(&e["class_hash"])) {
                    p.class_hashes.insert(a, c);
                }
            }
        }
        report.storage = p.storage.len();
        report.nonces = p.nonces.len();
        report.class_hashes = p.class_hashes.len();
        report
    }

    fn hit(&self) {
        *self.prefetch_hits.borrow_mut() += 1;
    }

    fn block_id(&self) -> Value {
        json!({ "block_number": self.block })
    }

    /// A read whose "contract not found" answer is the chain's default value.
    fn leaf(&self, method: &str, params: Value) -> StateResult<Felt> {
        match self.rpc.answer(method, params).map_err(read_err)? {
            Answer::Ok(v) => felt(&v),
            Answer::Error { code, .. } if code == CONTRACT_NOT_FOUND => Ok(Felt::ZERO),
            Answer::Error { code, message, .. } => Err(StateError::StateReadError(format!(
                "{method} refused at block {}: {code} {message}",
                self.block
            ))),
        }
    }

    pub fn class_record(&self, class_hash: ClassHash) -> StateResult<ClassRecord> {
        if let Some(c) = self.classes.borrow().get(&class_hash) {
            return Ok(c.clone());
        }
        let params =
            json!({ "block_id": self.block_id(), "class_hash": class_hash.0.to_hex_string() });
        let raw = match self
            .rpc
            .answer("starknet_getClass", params)
            .map_err(read_err)?
        {
            Answer::Ok(v) => v,
            Answer::Error { code, .. } if code == CLASS_HASH_NOT_FOUND => {
                return Err(StateError::UndeclaredClassHash(class_hash));
            }
            Answer::Error { code, message, .. } => {
                return Err(StateError::StateReadError(format!(
                    "getClass: {code} {message}"
                )))
            }
        };
        let rec = class_from_rpc(class_hash, raw).map_err(read_err)?;
        self.classes.borrow_mut().insert(class_hash, rec.clone());
        Ok(rec)
    }
}

/// Builds the compiled class the executor runs from the chain's `getClass` answer.
///
/// A Sierra class is compiled to CASM HERE, with the Sierra→CASM compiler as a library,
/// which is what StarkWare's own re-execution tool does. Whether the CASM so produced is
/// the CASM the sequencer ran is not assumed: the effects comparison after execution
/// (fee, gas, state diff) is what would show a difference.
pub fn class_from_rpc(class_hash: ClassHash, raw: Value) -> Result<ClassRecord> {
    if raw.get("sierra_program").is_some() {
        let sierra: SierraContractClass = serde_json::from_value(raw.clone())
            .map_err(|e| eyre!("class {class_hash}: not a Sierra class: {e}"))?;
        let sierra_version = SierraVersion::extract_from_program(&sierra.sierra_program)
            .map_err(|e| eyre!("class {class_hash}: Sierra version: {e}"))?;
        let for_compilation = CairoLangContractClass {
            sierra_program: sierra
                .sierra_program
                .iter()
                .map(|f| BigUintAsHex {
                    value: f.to_biguint(),
                })
                .collect(),
            sierra_program_debug_info: None,
            contract_class_version: sierra.contract_class_version.clone(),
            entry_points_by_type: ContractEntryPoints {
                external: eps(&sierra.entry_points_by_type.external),
                l1_handler: eps(&sierra.entry_points_by_type.l1handler),
                constructor: eps(&sierra.entry_points_by_type.constructor),
            },
            abi: None,
        };
        let program = for_compilation
            .extract_sierra_program(false)
            .map_err(|e| eyre!("class {class_hash}: extract Sierra program: {e}"))?;
        program
            .validate_version_compatible(ListSelector::ListName("all".to_string()))
            .map_err(|e| eyre!("class {class_hash}: Sierra version validation: {e}"))?;
        let sierra_program = program.program.clone();
        let (casm, debug) = CasmContractClass::from_contract_class_with_debug_info(
            for_compilation,
            program,
            true,
            usize::MAX,
        )
        .map_err(|e| eyre!("class {class_hash}: Sierra -> CASM: {e}"))?;
        // A declared class carries no debug names, so a concrete libfunc prints as `[N]`;
        // its declaration's long id (`felt252_add`, `store_temp<[3]>`, …) is the readable
        // name, and it is in the class itself.
        let names: HashMap<String, String> = sierra_program
            .libfunc_declarations
            .iter()
            .map(|d| (d.id.to_string(), d.long_id.to_string()))
            .collect();
        let sierra_statements = debug
            .sierra_statement_info
            .iter()
            .zip(sierra_program.statements.iter())
            .map(|(info, st)| {
                let mut text = st.to_string();
                if let Some(end) = text.find('(') {
                    if let Some(n) = names.get(&text[..end]) {
                        text = format!("{n}{}", &text[end..]);
                    }
                }
                SierraStatement {
                    start: info.start_offset,
                    end: info.end_offset,
                    text,
                }
            })
            .collect();
        let abi = raw
            .get("abi")
            .and_then(|a| a.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        Ok(ClassRecord {
            class_hash,
            casm: Some(casm),
            sierra_version: Some(sierra_version),
            deprecated: None,
            abi,
            raw,
            sierra_statements,
        })
    } else {
        let program_b64 = raw["program"]
            .as_str()
            .ok_or_else(|| eyre!("class {class_hash}: a Cairo 0 class with no program"))?;
        let gz = base64::engine::general_purpose::STANDARD.decode(program_b64)?;
        let mut text = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut text)?;
        let program: Program = serde_json::from_str(&text)?;
        let mut entry_points_by_type = HashMap::new();
        for (key, ty) in [
            ("CONSTRUCTOR", EntryPointType::Constructor),
            ("EXTERNAL", EntryPointType::External),
            ("L1_HANDLER", EntryPointType::L1Handler),
        ] {
            let mut v = vec![];
            for ep in raw["entry_points_by_type"][key]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                let offset = match &ep["offset"] {
                    Value::String(s) => usize::from_str_radix(s.trim_start_matches("0x"), 16)?,
                    other => other.as_u64().ok_or_else(|| eyre!("offset {other}"))? as usize,
                };
                v.push(EntryPointV0 {
                    offset: EntryPointOffset(offset),
                    selector: EntryPointSelector(Felt::from_hex(
                        ep["selector"].as_str().unwrap_or("0x0"),
                    )?),
                });
            }
            entry_points_by_type.insert(ty, v);
        }
        let abi = raw.get("abi").cloned().unwrap_or(Value::Null);
        Ok(ClassRecord {
            class_hash,
            casm: None,
            sierra_version: None,
            deprecated: Some(DeprecatedContractClass {
                program,
                entry_points_by_type,
                abi: None,
            }),
            abi,
            raw,
            sierra_statements: vec![],
        })
    }
}

fn eps(v: &[starknet_api::state::EntryPoint]) -> Vec<ContractEntryPoint> {
    v.iter()
        .map(|e| ContractEntryPoint {
            selector: e.selector.0.to_biguint(),
            function_idx: e.function_idx.0,
        })
        .collect()
}

impl ClassRecord {
    pub fn contract_class(&self) -> ContractClass {
        match (&self.casm, &self.deprecated) {
            (Some(casm), _) => {
                ContractClass::V1((casm.clone(), self.sierra_version.clone().unwrap()))
            }
            (None, Some(d)) => ContractClass::V0(d.clone()),
            _ => unreachable!(),
        }
    }
}

impl StateReader for RpcState<'_> {
    fn get_storage_at(
        &self,
        contract_address: ContractAddress,
        key: StorageKey,
    ) -> StateResult<Felt> {
        if let Some(v) = self
            .prefetched
            .borrow()
            .storage
            .get(&(*contract_address.0.key(), *key.0.key()))
        {
            self.hit();
            return Ok(*v);
        }
        self.leaf(
            "starknet_getStorageAt",
            json!({
                "contract_address": contract_address.0.key().to_hex_string(),
                "key": key.0.key().to_hex_string(),
                "block_id": self.block_id(),
            }),
        )
    }

    fn get_nonce_at(&self, contract_address: ContractAddress) -> StateResult<Nonce> {
        if let Some(v) = self
            .prefetched
            .borrow()
            .nonces
            .get(contract_address.0.key())
        {
            self.hit();
            return Ok(Nonce(*v));
        }
        Ok(Nonce(self.leaf(
            "starknet_getNonce",
            json!({ "block_id": self.block_id(), "contract_address": contract_address.0.key().to_hex_string() }),
        )?))
    }

    fn get_class_hash_at(&self, contract_address: ContractAddress) -> StateResult<ClassHash> {
        if let Some(v) = self
            .prefetched
            .borrow()
            .class_hashes
            .get(contract_address.0.key())
        {
            self.hit();
            return Ok(ClassHash(*v));
        }
        Ok(ClassHash(self.leaf(
            "starknet_getClassHashAt",
            json!({ "block_id": self.block_id(), "contract_address": contract_address.0.key().to_hex_string() }),
        )?))
    }

    fn get_compiled_class(&self, class_hash: ClassHash) -> StateResult<RunnableCompiledClass> {
        let rec = self.class_record(class_hash)?;
        RunnableCompiledClass::try_from(rec.contract_class())
            .map_err(|e| StateError::StateReadError(format!("class {class_hash}: {e}")))
    }

    fn get_compiled_class_hash(&self, _class_hash: ClassHash) -> StateResult<CompiledClassHash> {
        Err(StateError::StateReadError(
            "this reader does not serve compiled class hashes (no RPC method answers them)".into(),
        ))
    }

    fn get_compiled_class_hash_v2(
        &self,
        class_hash: ClassHash,
        compiled_class: &RunnableCompiledClass,
    ) -> StateResult<CompiledClassHash> {
        blockifier::state::utils::get_compiled_class_hash_v2(self, class_hash, compiled_class)
    }
}
