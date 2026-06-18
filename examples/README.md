# Cairo / Starknet recorder examples

This directory holds small Cairo programs you can record with the
CodeTracer Cairo recorder and replay in the CodeTracer GUI. They are
intentionally tiny so the resulting traces are easy to step through.

## Prerequisites

- The `ct` launcher is on your `PATH`. Run `ct --help` to confirm.
- The Cairo recorder has been built (`cargo build` from the repo
  root, or `nix develop` then `cargo build`). The `ct` launcher
  discovers the recorder automatically once it is present on
  `PATH` (or registered via the standard CodeTracer install).

## Two-step workflow: record, then replay

Record a program into a `.ct` trace bundle:

```bash
ct record examples/flow.cairo
```

This produces a `<program>.ct` directory under the current
out-dir (printed at the end of the run). Open it in the GUI with:

```bash
ct replay -t <trace-folder>
```

`<trace-folder>` is the `.ct` directory produced by `ct record`.

## One-step workflow: record and open

For interactive use, `ct run` records the program and opens the GUI
on the resulting trace in a single step:

```bash
ct run examples/flow.cairo
```

## Walkthrough: `flow.cairo`

`flow.cairo` is the simplest fixture in this directory — a `compute`
helper that builds a tuple from a few `let` bindings, called from
`main`. Record and open it:

```bash
ct run examples/flow.cairo
```

In the GUI:

1. Execution starts in `main`, which immediately calls `compute`.
   Use **step into** to descend into `compute`.
2. Use **step over** to advance through each `let` binding. The
   locals view fills in `a`, `b`, `sum_val`, `doubled`, and
   `final_result` one at a time.
3. **Step out** of `compute` to return to `main` and observe the
   returned tuple.

### Column-aware step-over: `column_aware.cairo`

`column_aware.cairo` packs three `let` statements onto a single
source line:

```cairo
let a: felt252 = 1; let b: felt252 = 2; let c: felt252 = 3;
```

Record it and step through the line:

```bash
ct run examples/column_aware.cairo
```

A column-aware step-over surfaces a distinct step for each of the
three statements — the cursor advances by column on that line before
moving on. Without column awareness all three would collapse onto
the same `(line, column=1)` location and only the first would
surface as a step.

## Other examples

- `control_flow.cairo` — `if`, `while`, and `match` so the trace
  contains non-linear control flow.
- `struct_data.cairo` — struct construction and field reads,
  useful for exploring the locals view.
- `simple_contract.cairo` — a minimal Starknet contract with
  storage and an emitted event.

Record any of them the same way:

```bash
ct record examples/<program>.cairo
ct replay -t <trace-folder>
```

or, in one step:

```bash
ct run examples/<program>.cairo
```
