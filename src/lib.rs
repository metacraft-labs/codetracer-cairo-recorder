//! CodeTracer recorder for Cairo/StarkNet programs.
//!
//! This crate captures execution traces from Cairo programs compiled through
//! the Cairo -> Sierra -> CASM pipeline and converts them into the CodeTracer
//! trace format for debugging and analysis.

pub mod recorder;
pub mod source_map;
pub mod tracer;
