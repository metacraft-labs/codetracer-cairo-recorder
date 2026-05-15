//! CLI entry point for the CodeTracer Cairo recorder.
//!
//! Supports the `record` subcommand which takes a Cairo source file,
//! compiles it through the Sierra/CASM pipeline, executes it,
//! captures the execution trace, and writes a CodeTracer CTFS trace
//! bundle.
//!
//! # Usage
//!
//! ```text
//! codetracer-cairo-recorder record <cairo-file> --out-dir <output-dir>
//! ```
//!
//! The recorder always writes traces in the canonical CodeTracer multi-stream
//! CTFS format (see `Recorder-CLI-Conventions.md` §4 in `codetracer-specs`).
//! No `--format` flag is exposed: human-readable conversion is handled
//! out-of-band by `ct print` (shipped with `codetracer-trace-format-nim`).
//!
//! # Environment variables
//!
//! * `CODETRACER_CAIRO_RECORDER_OUT_DIR` — fallback for `--out-dir` when the
//!   flag is not given. The CLI flag always wins.
//! * `CODETRACER_CAIRO_RECORDER_DISABLED` — set to `1` or `true` to skip
//!   recording entirely. The recorder still executes the target subcommand
//!   (where applicable) and propagates its exit code.
//! * `CODETRACER_CAIRO_RECORDER_LOG_LEVEL` — recorder log verbosity (advisory;
//!   the Cairo recorder currently logs to stderr unconditionally).
//! * `CAIRO_CORELIB_DIR` — path to the Cairo corelib (Cairo-toolchain specific,
//!   not part of the standard recorder env-var set).

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use eyre::{Context, Result};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Environment variable used as a fallback for `--out-dir` when the CLI
/// flag is omitted.  Convention: see `Recorder-CLI-Conventions.md` §5.
const ENV_OUT_DIR: &str = "CODETRACER_CAIRO_RECORDER_OUT_DIR";

/// Environment variable that, when set to `1`/`true`, disables tracing
/// entirely — the recorder runs as a transparent pass-through.
const ENV_DISABLED: &str = "CODETRACER_CAIRO_RECORDER_DISABLED";

/// Default output directory used when neither `--out-dir` nor
/// `CODETRACER_CAIRO_RECORDER_OUT_DIR` is set.
const DEFAULT_OUT_DIR: &str = "./ct-traces/";

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// CodeTracer Cairo recorder -- record Cairo program execution traces.
///
/// Traces are always written in the canonical CTFS multi-stream format.
/// To convert a recorded `.ct` bundle to JSON / text for inspection, use
/// `ct print` from `codetracer-trace-format-nim`.
#[derive(Debug, Parser)]
#[command(
    name = "codetracer-cairo-recorder",
    version,
    about = "Record Cairo program execution traces for CodeTracer (CTFS-only). \
             Use `ct print` from codetracer-trace-format-nim for human-readable conversion.",
    long_about = "Record Cairo program execution traces for CodeTracer.\n\
                  \n\
                  Output is always written in the canonical CodeTracer CTFS\n\
                  multi-stream format. Use `ct print` (shipped with the\n\
                  codetracer-trace-format-nim sibling) to convert a recorded\n\
                  `.ct` bundle to JSON or other human-readable forms.\n\
                  \n\
                  Environment variables:\n\
                    CODETRACER_CAIRO_RECORDER_OUT_DIR    fallback for --out-dir\n\
                    CODETRACER_CAIRO_RECORDER_DISABLED   set to 1/true to skip recording\n\
                    CODETRACER_CAIRO_RECORDER_LOG_LEVEL  log verbosity (advisory)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Record execution of a Cairo program.
    ///
    /// Compiles the given .cairo source file through the Sierra/CASM pipeline,
    /// executes it, captures the execution trace, and writes a CTFS bundle to
    /// `--out-dir`.
    Record(RecordArgs),

    /// Convert an snforge trace file to CodeTracer format.
    ///
    /// Parses the JSON trace output produced by `snforge --save-trace-data`
    /// and writes a CTFS bundle to `--out-dir`.
    TraceStarknet(TraceStarknetArgs),

    /// Replay a StarkNet on-chain transaction with tracing.
    ///
    /// Fetches the transaction trace from a StarkNet JSON-RPC node,
    /// reconstructs the execution context, and (in the future) re-executes
    /// the transaction locally with full CodeTracer tracing.
    Replay(ReplayArgs),

    /// Print version information.
    Version,
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// Path to the Cairo source (.cairo) file.
    program: PathBuf,

    /// Directory where the trace files will be written.
    ///
    /// The directory will be created if it does not exist.  Falls back to
    /// the `CODETRACER_CAIRO_RECORDER_OUT_DIR` environment variable when the
    /// flag is omitted.
    #[arg(short = 'o', long)]
    out_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct TraceStarknetArgs {
    /// Path to the snforge trace JSON file (produced by `snforge --save-trace-data`).
    trace_file: PathBuf,

    /// Directory where the CodeTracer trace files will be written.
    ///
    /// Falls back to `CODETRACER_CAIRO_RECORDER_OUT_DIR` when omitted.
    #[arg(short = 'o', long)]
    out_dir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ReplayArgs {
    /// Transaction hash to replay (e.g. 0x04a3c...).
    #[arg(long)]
    tx_hash: String,

    /// StarkNet JSON-RPC endpoint URL.
    #[arg(long)]
    rpc_url: String,

    /// Optional directory containing contract source code for source-level tracing.
    #[arg(long)]
    source_dir: Option<PathBuf>,

    /// Optional path to a saved `starknet_traceTransaction` JSON response.
    ///
    /// When supplied, the replay reads the trace JSON from this file
    /// instead of calling the live RPC node.  This is the offline
    /// fallback used by integration tests and by users who captured a
    /// trace out-of-band (e.g. via `curl`).  When omitted, the
    /// recorder attempts to fetch from `--rpc-url` (currently a stub
    /// that returns an error — see `StarknetRpcClient::trace_transaction`).
    #[arg(long)]
    trace_file: Option<PathBuf>,

    /// Directory where the replay trace bundle (.ct + sidecars) will
    /// be written.  Falls back to `CODETRACER_CAIRO_RECORDER_OUT_DIR`
    /// when omitted, then to `./ct-traces/`.
    #[arg(short = 'o', long)]
    out_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the effective output directory:
///   1. `--out-dir` if given on the CLI.
///   2. `CODETRACER_CAIRO_RECORDER_OUT_DIR` env var.
///   3. `DEFAULT_OUT_DIR` ("./ct-traces/").
fn resolve_out_dir(cli_out_dir: Option<PathBuf>) -> PathBuf {
    if let Some(path) = cli_out_dir {
        return path;
    }
    if let Some(value) = std::env::var_os(ENV_OUT_DIR) {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    PathBuf::from(DEFAULT_OUT_DIR)
}

/// Whether the recorder is disabled via env var.  When true, the CLI
/// must execute its target operation in pass-through mode without
/// emitting any trace artefacts.
fn recording_disabled() -> bool {
    match std::env::var(ENV_DISABLED) {
        Ok(value) => {
            let v = value.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Record(args) => record(args),
        Commands::TraceStarknet(args) => trace_starknet(args),
        Commands::Replay(args) => replay(args),
        Commands::Version => {
            println!("codetracer-cairo-recorder {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// `trace-starknet` implementation
// ---------------------------------------------------------------------------

/// Execute the `trace-starknet` subcommand.
fn trace_starknet(args: TraceStarknetArgs) -> Result<()> {
    let trace_path = args
        .trace_file
        .canonicalize()
        .with_context(|| format!("trace file not found: {}", args.trace_file.display()))?;

    eprintln!("Trace file: {}", trace_path.display());

    if recording_disabled() {
        eprintln!("{ENV_DISABLED} is set; skipping trace conversion (no output written).");
        return Ok(());
    }

    let out_dir = resolve_out_dir(args.out_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)?;
    eprintln!("Parsed {} trace entries", entries.len());

    codetracer_cairo_recorder::starknet::write_starknet_trace(&trace_path, &entries, &out_dir)?;

    eprintln!("Trace files written to {}", out_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// `replay` implementation
// ---------------------------------------------------------------------------

/// Execute the `replay` subcommand.
///
/// M5 (2026-05): the replay path is now end-to-end for the
/// trace-write half of the pipeline and live for the JSON-RPC fetch
/// half.  Given a [`TransactionTrace`] (from either a live RPC node
/// via `starknet_traceTransaction` or a saved JSON fixture via
/// `--trace-file`), the recorder walks the invocation tree and writes
/// a CodeTracer CTFS bundle to `--out-dir` — the same on-disk shape
/// as `record` and `trace-starknet`.
///
/// When no `--trace-file` is supplied the recorder additionally
/// fetches the contract class at `execute_invocation.contract_address`
/// via `starknet_getClassAt` and re-executes the entry point locally
/// using [`SierraCasmRunner`] (see `reexecute_entry_point`).  This
/// validates that the on-chain class is reproducible from the recorder
/// side; the re-execution outcome is logged to stderr but does not
/// (yet) feed back into the produced CTFS bundle.  Routing the
/// runner's `relocated_trace` through the existing `tracer.rs` step /
/// call pipeline is the M5 follow-up 2 task documented in
/// `src/starknet.rs`.
fn replay(args: ReplayArgs) -> Result<()> {
    let config = codetracer_cairo_recorder::starknet::ReplayConfig {
        tx_hash: args.tx_hash.clone(),
        rpc_url: args.rpc_url.clone(),
        source_dir: args.source_dir,
    };

    // Source the [`TransactionTrace`] from either the saved fixture
    // (`--trace-file`) or the live RPC node via
    // `starknet_traceTransaction`.  Both paths hand the same
    // [`TransactionTrace`] struct to the downstream write / re-execute
    // pipeline so the on-disk bundle shape is identical regardless of
    // origin.
    let trace = if let Some(ref path) = args.trace_file {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read trace file: {}", path.display()))?;
        codetracer_cairo_recorder::starknet::TransactionTrace::from_json(&content)?
    } else {
        let client = codetracer_cairo_recorder::starknet::StarknetRpcClient::new(&config.rpc_url);
        eprintln!(
            "Fetching trace for tx {} via {} ...",
            config.tx_hash, config.rpc_url
        );
        client.trace_transaction(&config.tx_hash)?
    };

    let ctx = codetracer_cairo_recorder::starknet::reconstruct_execution_context(&trace);
    eprintln!("Reconstructed execution context:");
    eprintln!("  Contract: {}", ctx.contract_address);
    eprintln!("  Selector: {}", ctx.entry_point_selector);
    eprintln!("  Calldata items: {}", ctx.calldata.len());
    eprintln!("  Storage entries: {}", ctx.storage_state.len());

    // Live path: also fetch the contract class and locally re-execute
    // the entry point.  Skipped when running offline against a fixture
    // (no RPC node is reachable to fetch the class from).
    if args.trace_file.is_none() {
        let client = codetracer_cairo_recorder::starknet::StarknetRpcClient::new(&config.rpc_url);
        let block_id = serde_json::json!("latest");
        eprintln!(
            "Fetching contract class at {} for local re-execution ...",
            ctx.contract_address
        );
        match client.get_class_at(&block_id, &ctx.contract_address) {
            Ok(class) => {
                match codetracer_cairo_recorder::starknet::reexecute_entry_point(
                    &class,
                    &ctx.entry_point_selector,
                    &ctx.calldata,
                ) {
                    Ok(result) => {
                        eprintln!("Local re-execution succeeded: {:?}", result.value);
                    }
                    Err(e) => {
                        eprintln!("Local re-execution skipped (non-fatal): {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("Class fetch skipped (non-fatal): {e}");
            }
        }
    }

    if recording_disabled() {
        eprintln!("{ENV_DISABLED} is set; skipping trace recording (no output written).");
        return Ok(());
    }

    let out_dir = resolve_out_dir(args.out_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    codetracer_cairo_recorder::starknet::write_replay_trace(&args.tx_hash, &trace, &out_dir)?;

    eprintln!("Replay trace written to {}", out_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// `record` implementation
// ---------------------------------------------------------------------------

/// Execute the `record` subcommand.
fn record(args: RecordArgs) -> Result<()> {
    // 1. Validate the source file exists
    let source_path = args
        .program
        .canonicalize()
        .with_context(|| format!("source file not found: {}", args.program.display()))?;

    eprintln!("Source file: {}", source_path.display());

    if recording_disabled() {
        // Pass-through: the Cairo recorder doesn't run a separate target
        // process — it compiles & executes the source itself — so disabling
        // recording simply means "don't emit any trace artefacts".
        eprintln!("{ENV_DISABLED} is set; skipping trace recording (no output written).");
        return Ok(());
    }

    // 2. Resolve and create the output directory
    let out_dir = resolve_out_dir(args.out_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // 3. Run the recorder (CTFS only)
    codetracer_cairo_recorder::recorder::record(&source_path, &out_dir)?;

    eprintln!("Trace files written to {}", out_dir.display());

    Ok(())
}
