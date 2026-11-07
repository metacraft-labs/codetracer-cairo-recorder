#!/usr/bin/env python3
"""Apply CodeTracer's execution observer to a pristine blockifier crate.

Usage: apply.py <extracted-crate-dir>

Every edit is an exact substring replacement that must match EXACTLY ONCE, so an
upstream change to any of the patched sites is a loud failure here rather than a
silently unpatched executor (which would record no steps and look like an empty
trace).
"""
import pathlib, shutil, sys

root = pathlib.Path(sys.argv[1])
here = pathlib.Path(__file__).resolve().parent


def edit(rel, old, new):
    p = root / rel
    s = p.read_text()
    n = s.count(old)
    if n != 1:
        sys.exit(f"apply.py: {rel}: expected exactly one match, found {n}:\n{old}")
    p.write_text(s.replace(old, new))


shutil.copy(here / "bt_trace.rs", root / "src/execution/bt_trace.rs")
edit("src/execution.rs", "pub mod call_info;\n", "pub mod bt_trace;\npub mod call_info;\n")

ENTER = """    let bt_frame = crate::execution::bt_trace::enter(
        call.class_hash,
        call.storage_address,
        call.caller_address,
        call.entry_point_selector,
        format!("{:?}", call.entry_point_type),
        format!("{:?}", call.call_type),
        CAIRO0,
        &call.calldata,
    );
    let result = execute_entry_point_call_observed(call, compiled_class, state, context);
    match &result {
        Ok(ci) => crate::execution::bt_trace::exit(
            bt_frame,
            None,
            ci.execution.retdata.0.clone(),
            ci.execution.failed,
        ),
        Err(e) => crate::execution::bt_trace::exit(bt_frame, Some(e.to_string()), vec![], true),
    }
    result
}

fn execute_entry_point_call_observed("""

# Cairo 1 (CASM) entry points.
v1 = "src/execution/entry_point_execution.rs"
edit(v1, """/// Executes a specific call to a contract entry point and returns its output.
pub fn execute_entry_point_call(
    call: ExecutableCallEntryPoint,
    compiled_class: CompiledClassV1,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> Result<CallInfo, EntryPointExecutionError> {
""", """/// Executes a specific call to a contract entry point and returns its output.
pub fn execute_entry_point_call(
    call: ExecutableCallEntryPoint,
    compiled_class: CompiledClassV1,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> Result<CallInfo, EntryPointExecutionError> {
    const CAIRO0: bool = false;
""" + ENTER + """
    call: ExecutableCallEntryPoint,
    compiled_class: CompiledClassV1,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> Result<CallInfo, EntryPointExecutionError> {
""")
edit(v1, """    run_entry_point(&mut runner, &mut syscall_handler, entry_point, args, program_segment_size)?;
""", """    let run = run_entry_point(
        &mut runner,
        &mut syscall_handler,
        entry_point,
        args,
        program_segment_size,
    );
    crate::execution::bt_trace::record_steps(crate::execution::bt_trace::steps_of(&runner));
    run?;
""")
edit(v1, """        context,
        ExecutionRunnerMode::Starknet,
    )
}""", """        context,
        if crate::execution::bt_trace::active() {
            ExecutionRunnerMode::Tracing
        } else {
            ExecutionRunnerMode::Starknet
        },
    )
}""")

# Cairo 0 entry points.
v0 = "src/execution/deprecated_entry_point_execution.rs"
edit(v0, """/// Executes a specific call to a contract entry point and returns its output.
pub fn execute_entry_point_call(
    call: ExecutableCallEntryPoint,
    compiled_class: CompiledClassV0,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> Result<CallInfo, EntryPointExecutionError> {
""", """/// Executes a specific call to a contract entry point and returns its output.
pub fn execute_entry_point_call(
    call: ExecutableCallEntryPoint,
    compiled_class: CompiledClassV0,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> Result<CallInfo, EntryPointExecutionError> {
    const CAIRO0: bool = true;
""" + ENTER + """
    call: ExecutableCallEntryPoint,
    compiled_class: CompiledClassV0,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> Result<CallInfo, EntryPointExecutionError> {
""")
edit(v0, """    run_entry_point(&mut runner, &mut syscall_handler, entry_point_pc, args)?;
""", """    let run = run_entry_point(&mut runner, &mut syscall_handler, entry_point_pc, args);
    crate::execution::bt_trace::record_steps(crate::execution::bt_trace::steps_of(&runner));
    run?;
""")
edit(v0, "    let trace_enabled = false;\n",
     "    let trace_enabled = crate::execution::bt_trace::active();\n")

# The syscall hints: note the executing frame's step count before the syscall runs.
edit("src/execution/syscalls/hint_processor.rs",
     "            Hint::Starknet(hint) => Ok(execute_next_syscall(self, vm, hint)?),\n",
     """            Hint::Starknet(hint) => {
                crate::execution::bt_trace::syscall_at(vm.get_current_step());
                Ok(execute_next_syscall(self, vm, hint)?)
            }
""")
edit("src/execution/deprecated_syscalls/hint_processor.rs",
     "        if hint_code::SYSCALL_HINTS.contains(hint.code.as_str()) {\n",
     "        if hint_code::SYSCALL_HINTS.contains(hint.code.as_str()) {\n"
     "            crate::execution::bt_trace::syscall_at(vm.get_current_step());\n")
print(f"apply.py: patched {root}")
