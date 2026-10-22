//! A listing of the program a class's VM frames actually ran: one line per CASM
//! instruction, `0x<pc>  <instruction>`, decoded from the bytecode the executor loaded.
//!
//! WHY A LISTING AND NOT CAIRO SOURCE. A class on chain carries its Sierra program and
//! its ABI and nothing that maps a program counter to a Cairo source line — the Sierra
//! debug info is not part of a declared class. So a step can be placed exactly at the
//! instruction it executed, and not at a source line, until verified sources are
//! resolved for the class (Recorder-Cairo CXP-CAIRO-4). The listing is built out of the
//! bytecode itself, so it needs nothing recovered from anywhere, and every position in
//! the recording points into it.
//!
//! Lines that do not start with `0x` are LABELS (an entry point, a function a `call`
//! instruction targets) and no step is ever placed on one.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use cairo_vm::types::instruction::{
    ApUpdate, Instruction, Op1Addr, Opcode, OpcodeExtension, PcUpdate, Register, Res,
};
use cairo_vm::vm::decoding::decoder::decode_instruction;
use serde_json::Value;
use starknet_types_core::felt::Felt;

use crate::state::ClassRecord;

pub struct Listing {
    pub text: String,
    /// 1-based line of each instruction's first word.
    pub pc_line: HashMap<usize, u32>,
    pub instructions: usize,
    /// Every pc a `call` in this program targets, with the label the listing gave it.
    pub function_at: BTreeMap<usize, String>,
    /// pc -> decoded instruction, for the call/return structure of a frame.
    pub decoded: HashMap<usize, Instruction>,
    /// Entry point offset -> the ABI name of the function it dispatches to.
    pub entry_names: BTreeMap<usize, String>,
    pub cairo0: bool,
}

fn reg(r: Register) -> &'static str {
    match r {
        Register::AP => "ap",
        Register::FP => "fp",
    }
}

fn off(o: isize) -> String {
    if o >= 0 {
        format!("+{o}")
    } else {
        format!("{o}")
    }
}

/// A felt as a reader would write it: small values in decimal, values within 2^64 of the
/// prime as negatives, and anything else in hex.
pub fn felt_text(f: &Felt) -> String {
    let neg = -*f;
    if f.bits() <= 64 {
        f.to_biguint().to_string()
    } else if neg.bits() <= 64 {
        format!("-{}", neg.to_biguint())
    } else {
        f.to_hex_string()
    }
}

fn format(ins: &Instruction, imm: Option<&Felt>) -> String {
    let dst = format!("[{}{}]", reg(ins.dst_register), off(ins.off0));
    let op0 = format!("[{}{}]", reg(ins.op0_register), off(ins.off1));
    let op1 = match ins.op1_addr {
        Op1Addr::Imm => imm.map(felt_text).unwrap_or_else(|| "<imm?>".into()),
        Op1Addr::AP => format!("[ap{}]", off(ins.off2)),
        Op1Addr::FP => format!("[fp{}]", off(ins.off2)),
        Op1Addr::Op0 => format!("[{op0}{}]", off(ins.off2)),
    };
    let res = match ins.res {
        Res::Op1 => op1.clone(),
        Res::Add => format!("{op0} + {op1}"),
        Res::Mul => format!("{op0} * {op1}"),
        Res::Unconstrained => op1.clone(),
    };
    let mut s = match (ins.opcode, ins.pc_update) {
        (Opcode::AssertEq, _) => format!("{dst} = {res}"),
        (Opcode::Call, PcUpdate::JumpRel) => format!("call rel {res}"),
        (Opcode::Call, _) => format!("call abs {res}"),
        (Opcode::Ret, _) => "ret".to_string(),
        (Opcode::NOp, PcUpdate::Jnz) => format!("jmp rel {op1} if {dst} != 0"),
        (Opcode::NOp, PcUpdate::JumpRel) => format!("jmp rel {res}"),
        (Opcode::NOp, PcUpdate::Jump) => format!("jmp abs {res}"),
        (Opcode::NOp, PcUpdate::Regular) => match ins.ap_update {
            ApUpdate::Add => format!("ap += {res}"),
            _ => "nop".to_string(),
        },
    };
    if ins.opcode != Opcode::NOp || ins.ap_update != ApUpdate::Add {
        match ins.ap_update {
            ApUpdate::Add1 => s.push_str(", ap++"),
            ApUpdate::Add2 => s.push_str(", ap += 2"),
            ApUpdate::Add if ins.opcode != Opcode::NOp => s.push_str(&format!(", ap += {res}")),
            _ => {}
        }
    }
    match ins.opcode_extension {
        OpcodeExtension::Stone => s,
        other => format!("{other:?} {s}"),
    }
}

/// ABI function names by selector, for both ABI shapes the chain serves (a Cairo 1 ABI
/// nests functions inside `interface` items; a Cairo 0 ABI is flat).
pub fn abi_names(abi: &Value) -> HashMap<Felt, String> {
    let mut out = HashMap::new();
    fn walk(v: &Value, out: &mut HashMap<Felt, String>) {
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => {
                let ty = o.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if matches!(ty, "function" | "l1_handler" | "constructor") {
                    if let Some(n) = o.get("name").and_then(|n| n.as_str()) {
                        let sel = starknet_api::abi::abi_utils::selector_from_name(n).0;
                        out.insert(sel, n.to_string());
                    }
                }
                if let Some(items) = o.get("items") {
                    walk(items, out);
                }
            }
            _ => {}
        }
    }
    walk(abi, &mut out);
    out
}

pub fn build(rec: &ClassRecord) -> Listing {
    let names = abi_names(&rec.abi);
    let (words, entry_points, cairo0): (Vec<Felt>, Vec<(usize, Felt, String)>, bool) =
        if let Some(casm) = &rec.casm {
            let words = casm
                .bytecode
                .iter()
                .map(|b| Felt::from(b.value.clone()))
                .collect();
            let mut eps = vec![];
            for (kind, list) in [
                ("external", &casm.entry_points_by_type.external),
                ("l1_handler", &casm.entry_points_by_type.l1_handler),
                ("constructor", &casm.entry_points_by_type.constructor),
            ] {
                for ep in list {
                    eps.push((ep.offset, Felt::from(ep.selector.clone()), kind.to_string()));
                }
            }
            (words, eps, false)
        } else {
            let d = rec
                .deprecated
                .as_ref()
                .expect("a class is Cairo 0 or Cairo 1");
            let words = d
                .program
                .data
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|w| {
                    w.as_str()
                        .and_then(|s| Felt::from_hex(s).ok())
                        .unwrap_or(Felt::ZERO)
                })
                .collect();
            let mut eps = vec![];
            for (ty, list) in &d.entry_points_by_type {
                for ep in list {
                    eps.push((ep.offset.0, ep.selector.0, format!("{ty:?}").to_lowercase()));
                }
            }
            (words, eps, true)
        };

    // Cairo 0 programs name their functions in `identifiers`; Cairo 1 classes do not, so
    // a function is named by the pc a `call` reaches it at.
    let mut cairo0_names: BTreeMap<usize, String> = BTreeMap::new();
    if let Some(d) = &rec.deprecated {
        if let Some(ids) = d.program.identifiers.as_object() {
            for (name, v) in ids {
                if v.get("type").and_then(|t| t.as_str()) == Some("function") {
                    if let Some(pc) = v.get("pc").and_then(|p| p.as_u64()) {
                        cairo0_names.insert(pc as usize, name.clone());
                    }
                }
            }
        }
    }

    // First pass: decode, so call targets are known before any line is written.
    let mut decoded: HashMap<usize, Instruction> = HashMap::new();
    let mut order = vec![];
    let mut pc = 0usize;
    while pc < words.len() {
        let w = words[pc];
        let ins = if w.bits() <= 128 {
            decode_instruction(w.to_biguint().try_into().unwrap_or(0)).ok()
        } else {
            None
        };
        match ins {
            Some(ins) => {
                order.push(pc);
                let size = ins.size();
                decoded.insert(pc, ins);
                pc += size;
            }
            None => {
                order.push(pc);
                pc += 1;
            }
        }
    }
    let mut targets: BTreeSet<usize> = BTreeSet::new();
    for (&pc, ins) in &decoded {
        if ins.opcode == Opcode::Call
            && ins.pc_update == PcUpdate::JumpRel
            && ins.op1_addr == Op1Addr::Imm
        {
            if let Some(imm) = words.get(pc + 1) {
                let rel = if (-*imm).bits() <= 64 {
                    -((-*imm).to_biguint().to_string().parse::<i64>().unwrap_or(0))
                } else {
                    imm.to_biguint().to_string().parse::<i64>().unwrap_or(0)
                };
                let t = pc as i64 + rel;
                if t >= 0 {
                    targets.insert(t as usize);
                }
            }
        }
    }

    let mut entry_names = BTreeMap::new();
    let mut labels: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (offset, selector, kind) in &entry_points {
        let name = names
            .get(selector)
            .cloned()
            .unwrap_or_else(|| selector.to_hex_string());
        entry_names.insert(*offset, name.clone());
        labels.entry(*offset).or_default().push(format!(
            "── {kind} entry point {name} ({}) ──",
            selector.to_hex_string()
        ));
    }
    let mut function_at = BTreeMap::new();
    for t in &targets {
        let name = cairo0_names
            .get(t)
            .cloned()
            .unwrap_or_else(|| format!("fn@0x{t:04x}"));
        function_at.insert(*t, name.clone());
        labels.entry(*t).or_default().push(format!("{name}:"));
    }
    for (pc, name) in &cairo0_names {
        if !targets.contains(pc) {
            labels.entry(*pc).or_default().push(format!("{name}:"));
            function_at.entry(*pc).or_insert_with(|| name.clone());
        }
    }

    // The Sierra statement each CASM range implements, as a label line before the range's
    // first instruction — `// sierra #<idx>: <statement>`. Steps stay on instruction lines.
    for (idx, st) in rec.sierra_statements.iter().enumerate() {
        if st.end > st.start {
            labels
                .entry(st.start)
                .or_default()
                .push(format!("// sierra #{idx}: {}", st.text));
        }
    }
    let mut text = String::new();
    let mut pc_line = HashMap::new();
    let mut line = 0u32;
    let mut instructions = 0;
    for pc in order {
        if let Some(ls) = labels.get(&pc) {
            for l in ls {
                text.push_str(l);
                text.push('\n');
                line += 1;
            }
        }
        line += 1;
        pc_line.insert(pc, line);
        match decoded.get(&pc) {
            Some(ins) => {
                instructions += 1;
                let imm = if ins.op1_addr == Op1Addr::Imm {
                    words.get(pc + 1)
                } else {
                    None
                };
                text.push_str(&format!("0x{pc:04x}  {}\n", format(ins, imm)));
            }
            None => text.push_str(&format!("0x{pc:04x}  dw {}\n", words[pc].to_hex_string())),
        }
    }
    Listing {
        text,
        pc_line,
        instructions,
        function_at,
        decoded,
        entry_names,
        cairo0,
    }
}
