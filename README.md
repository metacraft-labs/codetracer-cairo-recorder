## codetracer-cairo-recorder

A recorder for Cairo programs that produces [CodeTracer](https://github.com/metacraft-labs/CodeTracer) traces.

> **Note:** This project is in early development. APIs and trace formats may change.
> We welcome contributions and discussion!

### Overview

`codetracer-cairo-recorder` compiles Cairo source files through the
Sierra/CASM pipeline, executes them on the Cairo VM, and captures
step-level execution traces in the CodeTracer trace format. It also
supports converting `snforge --save-trace-data` output and replaying
on-chain StarkNet transactions.

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
codetracer-cairo-recorder record <file.cairo> --out-dir <dir> [--format binary|json]
```

Compiles the `.cairo` source through Sierra/CASM, executes it, and writes
CodeTracer trace files to `--out-dir`.

#### Convert an snforge trace

```bash
codetracer-cairo-recorder trace-starknet <snforge-trace.json> --out-dir <dir> [--format binary|json]
```

Parses the JSON trace output produced by `snforge --save-trace-data` and
converts it to CodeTracer format.

#### Replay a StarkNet transaction (stub)

```bash
codetracer-cairo-recorder replay <tx-hash> --out-dir <dir>
```

Replays a StarkNet on-chain transaction with tracing.

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

### Testing

```bash
cargo test
```

Test programs live in:

- `test-programs/cairo/` -- standalone Cairo programs
- `test-programs/starknet/` -- StarkNet contract examples

### Environment variables

| Variable | Description |
|---|---|
| `CAIRO_CORELIB_DIR` | Path to the Cairo corelib directory. Required unless `nix develop` provides it or the corelib is found relative to the binary / `CARGO_MANIFEST_DIR`. |

### Contributing

We'd be very happy if the community finds this useful, and if anyone wants to:

* Use and test the Cairo support or CodeTracer.
* Provide feedback and discuss alternative implementation ideas: in the issue tracker, or in our [discord](https://discord.gg/qSDCAFMP).
* Contribute code to enhance the Cairo support of CodeTracer.
* Provide [sponsorship](https://opencollective.com/codetracer), so we can hire dedicated full-time maintainers for this project.

### Legal info

LICENSE: MIT

Copyright (c) 2025 Metacraft Labs Ltd
