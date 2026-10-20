//! CodeTracer's execution observer (added by codetracer-cairo-recorder's onchain patch).
//!
//! When a recorder installs a sink on the executing thread, every entry-point frame the
//! executor runs is run with cairo-vm's own trace enabled, and the trace is handed to the
//! sink when the frame ends — together with the step of its parent at which the syscall
//! that opened it was made. Nothing is reconstructed: the program counters are the VM's.
//! With no sink installed this module is inert and the executor behaves exactly as
//! upstream (trace disabled).

use std::cell::RefCell;

use starknet_api::core::{ClassHash, ContractAddress, EntryPointSelector};
use starknet_api::transaction::fields::Calldata;
use starknet_types_core::felt::Felt;

/// One executed instruction: the program counter as an offset into the class's own
/// program segment, and the two registers, as offsets into the execution segment.
#[derive(Clone, Copy, Debug)]
pub struct Step {
    pub pc: usize,
    pub ap: usize,
    pub fp: usize,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub id: usize,
    pub parent: Option<usize>,
    /// How many steps the parent had executed when the syscall that opened this frame
    /// ran — i.e. this frame's steps belong between the parent's `parent_step - 1` and
    /// `parent_step` steps.
    pub parent_step: Option<usize>,
    pub class_hash: ClassHash,
    pub storage_address: ContractAddress,
    pub caller_address: ContractAddress,
    pub selector: EntryPointSelector,
    pub entry_point_type: String,
    pub call_type: String,
    pub cairo0: bool,
    pub calldata: Vec<Felt>,
    pub steps: Vec<Step>,
    /// `Some` when the frame ended in an error the executor returned, with its text.
    pub error: Option<String>,
    pub retdata: Vec<Felt>,
    pub failed: bool,
}

#[derive(Default)]
pub struct Sink {
    pub frames: Vec<Frame>,
    stack: Vec<usize>,
    /// The step count of the frame on top of the stack at its most recent syscall.
    pending_step: Vec<Option<usize>>,
}

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

pub fn install() {
    SINK.with(|s| *s.borrow_mut() = Some(Sink::default()));
}

pub fn take() -> Option<Sink> {
    SINK.with(|s| s.borrow_mut().take())
}

pub fn active() -> bool {
    SINK.with(|s| s.borrow().is_some())
}

#[allow(clippy::too_many_arguments)]
pub fn enter(
    class_hash: ClassHash,
    storage_address: ContractAddress,
    caller_address: ContractAddress,
    selector: EntryPointSelector,
    entry_point_type: String,
    call_type: String,
    cairo0: bool,
    calldata: &Calldata,
) -> Option<usize> {
    SINK.with(|s| {
        let mut b = s.borrow_mut();
        let sink = b.as_mut()?;
        let id = sink.frames.len();
        let parent = sink.stack.last().copied();
        let parent_step = sink.pending_step.last().copied().flatten();
        sink.frames.push(Frame {
            id,
            parent,
            parent_step,
            class_hash,
            storage_address,
            caller_address,
            selector,
            entry_point_type,
            call_type,
            cairo0,
            calldata: calldata.0.to_vec(),
            steps: vec![],
            error: None,
            retdata: vec![],
            failed: false,
        });
        sink.stack.push(id);
        sink.pending_step.push(None);
        Some(id)
    })
}

/// Called from the syscall hint, before the syscall runs, with the executing frame's VM
/// step count.
pub fn syscall_at(step: usize) {
    SINK.with(|s| {
        if let Some(sink) = s.borrow_mut().as_mut() {
            if let Some(p) = sink.pending_step.last_mut() {
                *p = Some(step);
            }
        }
    })
}

/// Called once the frame's run has ended (well or not), while the frame is still the
/// innermost one, with the VM's own trace of it.
pub fn record_steps(steps: Vec<Step>) {
    SINK.with(|s| {
        if let Some(sink) = s.borrow_mut().as_mut() {
            if let Some(&top) = sink.stack.last() {
                sink.frames[top].steps = steps;
            }
        }
    })
}

pub fn exit(id: Option<usize>, error: Option<String>, retdata: Vec<Felt>, failed: bool) {
    let Some(id) = id else { return };
    SINK.with(|s| {
        if let Some(sink) = s.borrow_mut().as_mut() {
            let f = &mut sink.frames[id];
            f.error = error;
            f.retdata = retdata;
            f.failed = failed;
            sink.stack.pop();
            sink.pending_step.pop();
        }
    })
}

/// The VM's own trace, as program-segment offsets. A pc outside segment 0 (none is
/// expected) is kept with its offset and flagged by a `usize::MAX` ap so it cannot be
/// mistaken for a program position.
pub fn steps_of(runner: &cairo_vm::vm::runners::cairo_runner::CairoRunner) -> Vec<Step> {
    match runner.get_relocatable_trace() {
        Ok(t) => t
            .iter()
            .map(|e| Step {
                pc: e.pc.offset,
                ap: if e.pc.segment_index == 0 { e.ap } else { usize::MAX },
                fp: e.fp,
            })
            .collect(),
        Err(_) => vec![],
    }
}
