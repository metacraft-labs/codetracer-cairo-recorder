//! One mined transaction's execution, as cairo-vm ran it, into a CodeTracer CTFS
//! container.
//!
//! EVERY STEP IN THE CONTAINER IS ONE INSTRUCTION THE VM EXECUTED, in execution order,
//! across every entry-point frame the transaction ran (validate, execute, every nested
//! `call_contract` / `library_call` / deploy, the fee transfer). The order is the VM's:
//! a nested frame's steps are spliced into its parent's at the step whose syscall opened
//! it, which the executor reported (see `blockifier-patch/bt_trace.rs`). Nothing here
//! reconstructs a step.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use blockifier::execution::bt_trace::Frame;
use codetracer_trace_types::{EventLogKind, Line, TypeKind, ValueRecord, NONE_VALUE};
use codetracer_trace_writer_nim::trace_writer::TraceWriter;
use codetracer_trace_writer_nim::{NimTraceWriter, TraceEventsFileFormat};
use eyre::{eyre, Result};
use starknet_api::core::ClassHash;
use starknet_types_core::felt::Felt;

use crate::listing::{abi_names, Listing};

/// A valid UUIDv7 (RFC 9562) that is a function of the transaction alone: the 48-bit
/// time field is the block timestamp in milliseconds (so ids still sort by when the
/// transaction ran) and the 74 "random" bits are the leading bits of the transaction
/// hash. The same derivation as codetracer-evm-recorder's `derived_recording_id`.
pub fn derived_recording_id(block_timestamp_secs: u64, tx_hash: &Felt) -> String {
    let ms = block_timestamp_secs.saturating_mul(1000) & 0xFFFF_FFFF_FFFF;
    let h = tx_hash.to_bytes_be();
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | (h[0] & 0x0f);
    b[7] = h[1];
    b[8] = 0x80 | (h[2] & 0x3f);
    b[9..].copy_from_slice(&h[3..10]);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The events one frame emitted, as `(keys, data)`, in emission order.
pub type FrameEvents = Vec<(Vec<Felt>, Vec<Felt>)>;

pub struct Recorded {
    pub container: PathBuf,
    pub steps: usize,
    pub frames: usize,
    pub internal_calls: usize,
    pub events_written: usize,
    /// Steps positioned on a Cairo source line rather than a CASM listing line.
    pub cairo_steps: usize,
}

fn big(f: &Felt, type_id: codetracer_trace_types::TypeId) -> ValueRecord {
    let bytes = f.to_bytes_be();
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(31);
    ValueRecord::BigInt {
        b: bytes[first..].to_vec(),
        negative: false,
        type_id,
    }
}

fn seq(
    fs: &[Felt],
    elem: codetracer_trace_types::TypeId,
    ty: codetracer_trace_types::TypeId,
) -> ValueRecord {
    ValueRecord::Sequence {
        elements: fs.iter().map(|f| big(f, elem)).collect(),
        is_slice: false,
        type_id: ty,
    }
}

pub struct Context<'a> {
    pub listings: &'a HashMap<ClassHash, Listing>,
    /// The path each class's listing is INTERNED under: `./sources/<class_hash>.casm`,
    /// relative to the capture directory, so a container names no work directory.
    pub paths: &'a HashMap<ClassHash, PathBuf>,
    pub abis: &'a HashMap<ClassHash, serde_json::Value>,
    /// Per class with a bound build that embeds its sources: pc -> the Cairo file (as
    /// interned) and line the step is shown at. A pc not here stays on its CASM line.
    pub cairo_at: &'a HashMap<ClassHash, HashMap<usize, (PathBuf, u32)>>,
    /// Every Cairo file those name, as interned, with its line lengths.
    pub cairo_files: &'a [(PathBuf, Vec<u32>)],
}

#[allow(clippy::too_many_arguments)]
pub fn write_container(
    program_label: &str,
    recording_id: &str,
    out_dir: &Path,
    frames: &[Frame],
    events_by_frame: &BTreeMap<usize, FrameEvents>,
    revert_error: Option<&str>,
    cx: &Context,
) -> Result<Recorded> {
    // A recording id that is a function of the execution, so two runs over the same
    // inputs write the same bytes — what a producer publishing the container at an
    // input-derived URL, and a build checking it was produced rather than stored, need.
    // The setter is on the Nim writer, not on the trait, so the concrete writer is
    // built here; `create_trace_writer` builds the same one for `Ctfs`.
    let mut nim = NimTraceWriter::new(program_label, &[], TraceEventsFileFormat::Ctfs);
    nim.set_recording_id(recording_id)
        .map_err(|e| eyre!("the writer refused recording id {recording_id}: {e}"))?;
    let mut writer: Box<dyn TraceWriter> = Box::new(nim);
    let w = &mut *writer;
    TraceWriter::begin_writing_trace_events(w, &out_dir.join("trace.json"))
        .map_err(|e| eyre!("{e}"))?;
    TraceWriter::enable_column_aware_steps(w);

    let mut registered = std::collections::HashSet::new();
    for f in frames {
        if registered.insert(f.class_hash) {
            let path = &cx.paths[&f.class_hash];
            let lens: Vec<u32> = cx.listings[&f.class_hash]
                .text
                .lines()
                .map(|l| l.len() as u32)
                .collect();
            TraceWriter::register_path_with_line_lengths(w, path, &lens)
                .map_err(|e| eyre!("{e}"))?;
        }
    }
    for (path, lens) in cx.cairo_files {
        TraceWriter::register_path_with_line_lengths(w, path, lens).map_err(|e| eyre!("{e}"))?;
    }
    let first_path = frames
        .first()
        .map(|f| cx.paths[&f.class_hash].clone())
        .ok_or_else(|| eyre!("{program_label}: the executor ran no entry point at all"))?;
    TraceWriter::start(w, &first_path, Line(1));

    let felt_t = TraceWriter::ensure_type_id(w, TypeKind::Int, "felt252");
    let felts_t = TraceWriter::ensure_type_id(w, TypeKind::Seq, "Span<felt252>");
    let str_t = TraceWriter::ensure_type_id(w, TypeKind::String, "String");

    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for f in frames {
        children.entry(f.parent).or_default().push(f.id);
    }
    let names: HashMap<ClassHash, HashMap<Felt, String>> =
        cx.abis.iter().map(|(c, a)| (*c, abi_names(a))).collect();

    let mut rec = Recorded {
        container: out_dir.join(format!("{program_label}.ct")),
        steps: 1,
        frames: frames.len(),
        internal_calls: 0,
        cairo_steps: 0,
        events_written: 0,
    };

    struct Ctx<'b> {
        frames: &'b [Frame],
        children: &'b HashMap<Option<usize>, Vec<usize>>,
        names: &'b HashMap<ClassHash, HashMap<Felt, String>>,
        cx: &'b Context<'b>,
        events_by_frame: &'b BTreeMap<usize, FrameEvents>,
        felt_t: codetracer_trace_types::TypeId,
        felts_t: codetracer_trace_types::TypeId,
        str_t: codetracer_trace_types::TypeId,
    }

    fn emit(w: &mut dyn TraceWriter, c: &Ctx, id: usize, rec: &mut Recorded) {
        let f = &c.frames[id];
        let listing = &c.cx.listings[&f.class_hash];
        let path = &c.cx.paths[&f.class_hash];
        let line_of = |pc: usize| Line(listing.pc_line.get(&pc).copied().unwrap_or(1) as i64);
        let name = c
            .names
            .get(&f.class_hash)
            .and_then(|m| m.get(&f.selector.0))
            .cloned()
            .unwrap_or_else(|| f.selector.0.to_hex_string());
        let entry_line = f.steps.first().map(|s| line_of(s.pc)).unwrap_or(Line(1));
        let fn_id = TraceWriter::ensure_function_id(w, &name, path, entry_line);
        let _ = TraceWriter::arg(
            w,
            "contract",
            ValueRecord::String {
                text: f.storage_address.0.key().to_hex_string(),
                type_id: c.str_t,
            },
        );
        let _ = TraceWriter::arg(
            w,
            "class_hash",
            ValueRecord::String {
                text: f.class_hash.0.to_hex_string(),
                type_id: c.str_t,
            },
        );
        let _ = TraceWriter::arg(
            w,
            "caller",
            ValueRecord::String {
                text: f.caller_address.0.key().to_hex_string(),
                type_id: c.str_t,
            },
        );
        let _ = TraceWriter::arg(w, "calldata", seq(&f.calldata, c.felt_t, c.felts_t));
        TraceWriter::register_call(w, fn_id, vec![]);

        let kids: Vec<usize> = c.children.get(&Some(id)).cloned().unwrap_or_default();
        let mut kid_at: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut kid_end = vec![];
        for k in kids {
            match c.frames[k].parent_step {
                Some(s) if s < f.steps.len() => kid_at.entry(s).or_default().push(k),
                _ => kid_end.push(k),
            }
        }
        let mut depth = 0usize;
        let mut pending_call = false;
        for (i, s) in f.steps.iter().enumerate() {
            if pending_call {
                let callee = listing
                    .function_at
                    .get(&s.pc)
                    .cloned()
                    .unwrap_or_else(|| format!("fn@0x{:04x}", s.pc));
                let id = TraceWriter::ensure_function_id(w, &callee, path, line_of(s.pc));
                TraceWriter::register_call(w, id, vec![]);
                depth += 1;
                rec.internal_calls += 1;
                pending_call = false;
            }
            if let Some(ks) = kid_at.get(&i) {
                for k in ks {
                    emit(w, c, *k, rec);
                }
            }
            match c.cx.cairo_at.get(&f.class_hash).and_then(|m| m.get(&s.pc)) {
                Some((p, l)) => {
                    TraceWriter::register_step_with_column(w, p, Line(*l as i64), None);
                    rec.cairo_steps += 1;
                }
                None => TraceWriter::register_step_with_column(w, path, line_of(s.pc), None),
            }
            rec.steps += 1;
            use cairo_vm::types::instruction::Opcode;
            match listing.decoded.get(&s.pc).map(|i| i.opcode) {
                Some(Opcode::Call) => pending_call = true,
                Some(Opcode::Ret) if depth > 0 => {
                    TraceWriter::register_return(w, NONE_VALUE);
                    depth -= 1;
                }
                _ => {}
            }
        }
        for k in kid_end {
            emit(w, c, k, rec);
        }
        // A frame that stopped mid-function (an error) leaves internal calls open; they
        // are closed here so the call stream stays balanced, and the error says why.
        while depth > 0 {
            TraceWriter::register_return(w, NONE_VALUE);
            depth -= 1;
        }
        if let Some(evs) = c.events_by_frame.get(&id) {
            for (keys, data) in evs {
                let content = format!(
                    "keys=[{}] data=[{}]",
                    keys.iter()
                        .map(|k| k.to_hex_string())
                        .collect::<Vec<_>>()
                        .join(","),
                    data.iter()
                        .map(|d| d.to_hex_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                TraceWriter::register_special_event(
                    w,
                    EventLogKind::EvmEvent,
                    &format!("event from {}", f.storage_address.0.key().to_hex_string()),
                    &content,
                );
                rec.events_written += 1;
            }
        }
        match &f.error {
            Some(e) => {
                TraceWriter::register_special_event(w, EventLogKind::Error, "entry point error", e);
                TraceWriter::register_return(
                    w,
                    ValueRecord::Error {
                        msg: e.clone(),
                        type_id: c.str_t,
                    },
                );
            }
            None => TraceWriter::register_return(w, seq(&f.retdata, c.felt_t, c.felts_t)),
        }
    }

    let c = Ctx {
        frames,
        children: &children,
        names: &names,
        cx,
        events_by_frame,
        felt_t,
        felts_t,
        str_t,
    };
    for top in children.get(&None).cloned().unwrap_or_default() {
        emit(w, &c, top, &mut rec);
    }
    if let Some(e) = revert_error {
        TraceWriter::register_special_event(w, EventLogKind::Error, "transaction reverted", e);
    }
    TraceWriter::finish_writing_trace_events(w).map_err(|e| eyre!("{e}"))?;
    writer
        .write_meta_dat("codetracer-cairo-recorder")
        .map_err(|e| eyre!("{e}"))?;
    writer.close().map_err(|e| eyre!("{e}"))?;
    Ok(rec)
}
