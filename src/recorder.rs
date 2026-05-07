//! Recording logic for Cairo execution traces.
//!
//! This module provides the top-level `record` function that reads a Cairo
//! source file, compiles and runs it through the tracer, and writes a
//! CodeTracer CTFS trace bundle.

use std::path::Path;

use eyre::{Context, Result};

use crate::tracer::CairoTracer;

/// Record a Cairo execution trace.
///
/// Reads the Cairo source file at `source_path`, compiles it through the
/// Sierra/CASM pipeline, executes it, captures the trace, and writes a
/// CTFS bundle to `out_dir`.
///
/// The output format is fixed to CTFS — see
/// `Recorder-CLI-Conventions.md` §4 in `codetracer-specs`.  Use
/// `ct print` (from `codetracer-trace-format-nim`) for human-readable
/// conversion of the produced bundle.
pub fn record(source_path: &Path, out_dir: &Path) -> Result<()> {
    let source_code = std::fs::read_to_string(source_path)
        .with_context(|| format!("failed to read source file: {}", source_path.display()))?;

    CairoTracer::trace_program(source_path, &source_code, out_dir)
}
