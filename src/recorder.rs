//! Recording logic for Cairo execution traces.
//!
//! This module provides the top-level `record` function that reads a Cairo
//! source file, compiles and runs it through the tracer, and writes
//! CodeTracer output.

use std::path::Path;

use codetracer_trace_writer::TraceEventsFileFormat;
use eyre::{Context, Result};

use crate::tracer::CairoTracer;

/// Record a Cairo execution trace.
///
/// Reads the Cairo source file at `source_path`, compiles it through the
/// Sierra/CASM pipeline, executes it, captures the trace, and writes
/// CodeTracer trace files to `out_dir`.
pub fn record(
    source_path: &Path,
    out_dir: &Path,
    format: TraceEventsFileFormat,
) -> Result<()> {
    let source_code = std::fs::read_to_string(source_path)
        .with_context(|| format!("failed to read source file: {}", source_path.display()))?;

    CairoTracer::trace_program(source_path, &source_code, out_dir, format)
}
