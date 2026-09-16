## codetracer-cairo-recorder

A recorder for Cairo programs that produces [CodeTracer](https://github.com/metacraft-labs/CodeTracer) traces.

> **Note:** This project is in early development. APIs and trace formats may change.
> We welcome contributions and discussion!

### Overview

`codetracer-cairo-recorder` compiles Cairo source files through the
Sierra/CASM pipeline, executes them on the Cairo VM, and captures
step-level execution traces in the canonical CodeTracer CTFS multi-stream
format. It also supports converting `snforge --save-trace-data` output
and replaying on-chain StarkNet transactions.

### Building

```bash
cargo build
```

Or enter the Nix dev shell first:

```bash
nix develop
cargo build
```

### Usage

#### Record a Cairo program

```bash
codetracer-cairo-recorder record <file.cairo> --out-dir <dir>
```

Compiles the `.cairo` source through Sierra/CASM, executes it, and
writes a CTFS trace bundle to `--out-dir`.

The recorder always writes traces in the canonical CodeTracer CTFS
multi-stream format (a single `.ct` container plus
`trace_metadata.json` / `trace_paths.json` sidecars). There is no
`--format` flag — see "Converting traces" below for human-readable
output.

#### Convert an snforge trace

```bash
codetracer-cairo-recorder trace-starknet <snforge-trace.json> --out-dir <dir>
```

Parses the JSON trace output produced by `snforge --save-trace-data` and
converts it to CodeTracer CTFS.

#### Replay a StarkNet transaction (stub)

```bash
codetracer-cairo-recorder replay --tx-hash <tx-hash> --rpc-url <url>
```

Replays a StarkNet on-chain transaction with tracing.

#### Converting traces to JSON / text

The recorder is CTFS-only. To convert a recorded `.ct` bundle to a
human-readable form, use `ct print` from
[`codetracer-trace-format-nim`](https://github.com/metacraft-labs/codetracer-trace-format-nim):

```bash
ct-print --json <recording-dir>/<program>.ct
```

`ct-print` accepts `--json`, `--json-events`, `--summary`, and
`--follow` modes; see its `--help` for details. This conversion path
is the canonical way to produce textual oracles for golden-snapshot
tests, debugging, and interop with non-CodeTracer tools — see
`Recorder-CLI-Conventions.md` §4 in the `codetracer-specs` repo.

### Architecture

The recorder is structured around the following modules in `src/`:

| Module | Purpose |
|---|---|
| `main.rs` | CLI entry point (clap) |
| `recorder.rs` | Top-level recording orchestration |
| `tracer.rs` | Step-level trace capture during Cairo VM execution |
| `source_map.rs` | Mapping between CASM offsets and Cairo source locations |
| `starknet.rs` | Parsing and converting snforge / StarkNet trace data |
| `lib.rs` | Public library API |

### Examples

See [`examples/`](examples/README.md) for a walkthrough that records
and replays a few small Cairo and Starknet programs through the `ct`
launcher (`ct record`, `ct replay`, `ct run`), including a
column-aware step-over demo.

### Testing

```bash
cargo test
```

Test programs live in:

- `test-programs/cairo/` -- standalone Cairo programs
- `test-programs/starknet/` -- StarkNet contract examples

### Environment variables

The recorder respects the standard CodeTracer recorder env-var contract
defined in `Recorder-CLI-Conventions.md` §5:

| Variable | CLI equivalent | Description |
|---|---|---|
| `CODETRACER_CAIRO_RECORDER_OUT_DIR` | `--out-dir` | Fallback output directory when `--out-dir` is omitted. The CLI flag always wins. |
| `CODETRACER_CAIRO_RECORDER_DISABLED` | — | Set to `1` or `true` to run the recorder in pass-through mode (no trace artefacts written). |
| `CODETRACER_CAIRO_RECORDER_LOG_LEVEL` | — | Recorder log verbosity (advisory; the Cairo recorder currently logs to stderr unconditionally). |
| `CAIRO_CORELIB_DIR` | — | Path to the Cairo corelib directory. Required unless `nix develop` provides it or the corelib is found relative to the binary / `CARGO_MANIFEST_DIR`. |

### Contributing

We'd be very happy if the community finds this useful, and if anyone wants to:

* Use and test the Cairo support or CodeTracer.
* Provide feedback and discuss alternative implementation ideas: in the issue tracker, or in our [discord](https://discord.gg/qSDCAFMP).
* Contribute code to enhance the Cairo support of CodeTracer.
* Provide [sponsorship](https://opencollective.com/codetracer), so we can hire dedicated full-time maintainers for this project.

### Legal info

LICENSE: MIT

Copyright (c) 2025 Metacraft Labs Ltd
