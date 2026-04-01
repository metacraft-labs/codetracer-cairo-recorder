//! CLI entry point for the CodeTracer Cairo recorder.
//!
//! Supports the `record` subcommand which takes a Cairo source file,
//! compiles it through the Sierra/CASM pipeline, executes it,
//! captures the execution trace, and writes CodeTracer trace output files.
//!
//! # Usage
//!
//! ```text
//! codetracer-cairo-recorder record <cairo-file> \
//!     --out-dir <output-dir> \
//!     [--format binary|json]
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use codetracer_trace_writer::TraceEventsFileFormat;
use eyre::{Context, Result};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// CodeTracer Cairo recorder -- record Cairo program execution traces.
#[derive(Debug, Parser)]
#[command(
    name = "codetracer-cairo-recorder",
    version,
    about = "Record Cairo program execution traces for CodeTracer"
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
    /// executes it, captures the execution trace, and writes CodeTracer trace
    /// files to `--out-dir`.
    Record(RecordArgs),

    /// Convert an snforge trace file to CodeTracer format.
    ///
    /// Parses the JSON trace output produced by `snforge --save-trace-data`
    /// and writes CodeTracer trace files to `--out-dir`.
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

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Binary,
    Json,
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// Path to the Cairo source (.cairo) file.
    program: PathBuf,

    /// Directory where the trace files will be written.
    ///
    /// The directory will be created if it does not exist.
    #[arg(short = 'o', long, default_value = "./ct-traces/")]
    out_dir: PathBuf,

    /// Output format for the trace data.
    #[arg(short = 'f', long, default_value = "binary")]
    format: OutputFormat,
}

#[derive(Debug, clap::Args)]
struct TraceStarknetArgs {
    /// Path to the snforge trace JSON file (produced by `snforge --save-trace-data`).
    trace_file: PathBuf,

    /// Directory where the CodeTracer trace files will be written.
    #[arg(short = 'o', long, default_value = "./ct-traces/")]
    out_dir: PathBuf,

    /// Output format for the trace data.
    #[arg(short = 'f', long, default_value = "binary")]
    format: OutputFormat,
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
// `record` implementation
// ---------------------------------------------------------------------------

/// Execute the `trace-starknet` subcommand.
fn trace_starknet(args: TraceStarknetArgs) -> Result<()> {
    let trace_path = args
        .trace_file
        .canonicalize()
        .with_context(|| format!("trace file not found: {}", args.trace_file.display()))?;

    eprintln!("Trace file: {}", trace_path.display());

    let format = match args.format {
        OutputFormat::Binary => TraceEventsFileFormat::Binary,
        OutputFormat::Json => TraceEventsFileFormat::Json,
    };

    let out_dir = &args.out_dir;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    let entries = codetracer_cairo_recorder::starknet::parse_snforge_trace(&trace_path)?;
    eprintln!("Parsed {} trace entries", entries.len());

    codetracer_cairo_recorder::starknet::write_starknet_trace(
        &trace_path,
        &entries,
        out_dir,
        format,
    )?;

    eprintln!("Trace files written to {}", out_dir.display());
    Ok(())
}

/// Execute the `replay` subcommand.
fn replay(args: ReplayArgs) -> Result<()> {
    let config = codetracer_cairo_recorder::starknet::ReplayConfig {
        tx_hash: args.tx_hash,
        rpc_url: args.rpc_url,
        source_dir: args.source_dir,
    };

    match codetracer_cairo_recorder::starknet::replay_transaction(&config) {
        Ok(ctx) => {
            eprintln!("Replay complete.");
            eprintln!("  Contract: {}", ctx.contract_address);
            eprintln!("  Selector: {}", ctx.entry_point_selector);
            eprintln!("  Calldata items: {}", ctx.calldata.len());
            eprintln!("  Storage entries: {}", ctx.storage_state.len());
            // TODO(M5): Re-execute the transaction with CodeTracer tracing
            // once we have contract artifact resolution and local execution.
            eprintln!("Note: local re-execution with tracing is not yet implemented.");
            Ok(())
        }
        Err(e) => {
            eprintln!("Replay failed: {e}");
            eprintln!("Note: the replay subcommand requires a live StarkNet RPC node.");
            eprintln!("This is expected when running without network access.");
            Err(e)
        }
    }
}

/// Execute the `record` subcommand.
fn record(args: RecordArgs) -> Result<()> {
    // 1. Validate the source file exists
    let source_path = args
        .program
        .canonicalize()
        .with_context(|| format!("source file not found: {}", args.program.display()))?;

    eprintln!("Source file: {}", source_path.display());

    let format = match args.format {
        OutputFormat::Binary => TraceEventsFileFormat::Binary,
        OutputFormat::Json => TraceEventsFileFormat::Json,
    };

    // 2. Create the output directory
    let out_dir = &args.out_dir;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create output dir: {}", out_dir.display()))?;

    // 3. Run the recorder
    codetracer_cairo_recorder::recorder::record(&source_path, out_dir, format)?;

    eprintln!("Trace files written to {}", out_dir.display());

    Ok(())
}
