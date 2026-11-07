# codetracer-cairo-onchain

Records **mined Starknet transactions** as StarkWare's own executor re-runs them, and
says, figure by figure, how the re-run compares with what the chain recorded.

```
just onchain-trace-block <block> <rpc-url> <out-dir> [--tx <hash> ...] [--transcript-out <dir>]
onchain/target/release/codetracer-cairo-onchain trace-block <block> --transcript-in <dir> -o <out>
```

## What runs, and where every step comes from

* **The executor is StarkWare's**: `blockifier` (the sequencer's transaction executor,
  crates.io `0.20.0-rc.1`), which runs every Cairo 1 and Cairo 0 entry point in
  `cairo-vm`. Nothing in this package interprets Cairo.
* **The steps are cairo-vm's own trace**, one per executed instruction. Upstream
  blockifier never enables the VM trace on its execution path, so
  `blockifier-patch/` adds an observer (`bt_trace.rs`) that, when this tool installs it
  on the executing thread, runs each entry-point frame with the trace on, hands the
  trace over when the frame ends, and notes the parent step at which each nested call's
  syscall ran. With no observer installed the executor behaves exactly as upstream.
  `vendor-blockifier.sh` applies the patch to the crates.io tarball, verified against
  `blockifier-patch/CHECKSUMS`; the patched tree is a build input and is not committed.
* **The prestate is the parent block**, read from a node, and the block's transactions
  `0..k-1` are replayed before transaction `k` — so `k` sees the state the sequencer
  gave it. The node is asked for the block's whole read set in one call
  (`starknet_simulateTransactions` + `RETURN_INITIAL_READS` at `N-1`, the way
  StarkWare's re-execution tool prefetches); anything it does not return is read one
  leaf at a time. Every prefetched value is a parent-block value however the
  simulation was shaped, so a fallback can only cost time, never correctness.
* **Sierra classes are compiled to CASM here** (cairo-lang `2.19.5`), as StarkWare's
  re-execution does. Whether that CASM is what the sequencer ran is not assumed — the
  comparison below is what would show it.

## What is compared, and against what

Per transaction (`<out>/<tx>/report.json`), counted toward `reproduced` — all of it
**consensus** data from the receipt or the block:
execution status, actual fee, L2 gas, L1 data gas, L1 gas, event count, every event
(emitter, keys, data, in order), L2→L1 messages, and the call tree (address, class hash
and selector of every entry point, against `starknet_traceBlockTransactions`).

Per block (`<out>/block.json`): the whole state diff (storage, nonces, class hashes)
against `starknet_getStateUpdate`.

Reported but **not counted**: each transaction's own state diff against
`starknet_traceBlockTransactions`. That trace is the node's re-execution, not
consensus — measured on block 16,664,437 index 0, it reports 3,965,440 L2 gas where the
receipt says 4,205,440, and credits the sequencer a correspondingly smaller fee, while
this replay matches the receipt to the unit and the block diff entry for entry.

## What a recording contains

`<out>/<tx>/starknet-<block>-<tx>.ct` (CTFS) plus `sources/<class_hash>.casm`, one
listing per class the transaction entered: one line per CASM instruction,
`0x<pc>  <instruction>`, decoded from the bytecode the VM ran, with label lines at entry
points (named from the class ABI) and at every `call` target (Cairo 0 classes name
their functions from the program's `identifiers`). Steps are positioned by line in
these listings — **instruction level, not Cairo source**: a declared class carries no
debug info mapping a pc to a source line.

The call stream has one call per entry-point frame (`__validate__`, `__execute__`, every
nested `call_contract`/`library_call`, the fee transfer; args: contract, class hash,
caller, calldata; return: retdata or the error) and one per internal Cairo function call
(CASM `call`/`ret`). Starknet events are written as events of the frame that emitted
them.

## Reproducing a run offline

`--transcript-out <dir>` writes every RPC answer the run used (an `index.json` plus one
gzipped answer per request); `--transcript-in <dir>` serves them back with no network
and refuses any request the transcript does not hold. `tests/fixtures/mainnet-16664301`
is one, and `tests/offline.rs` replays it with two control arms (a one-unit change to a
single prestate value must surface as a divergence; with the observer off nothing may be
recorded).

## Limits, stated

* Starknet `0.13.0` through `0.14.4`: the executor carries versioned constants for no
  other version, so a block outside that range is refused by name.
* `DEPLOY` transactions (pre-0.10) are not executable.
* Events are attached to frames only when the executor's call infos and the observed
  frames name the same entry points in the same order (the report says whether they did).
