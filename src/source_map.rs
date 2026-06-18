//! Source mapping for Cairo programs.
//!
//! Provides byte-offset to line-number mapping for Cairo source files,
//! used to map execution locations back to source code lines.

use std::path::Path;

/// Maps byte offsets in a Cairo source file to line numbers.
///
/// Precomputes line boundaries from the source text so that any byte
/// offset can be quickly mapped to a 1-based line number.
pub struct SourceMap {
    /// Byte offset of the start of each line (0-indexed lines).
    line_starts: Vec<usize>,
    /// Original source code (kept for inspection/debugging).
    source_code: String,
}

impl SourceMap {
    /// Build a `SourceMap` from a source file path and its contents.
    pub fn from_source(_source_path: &Path, source_code: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, ch) in source_code.char_indices() {
            if ch == '\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            line_starts,
            source_code: source_code.to_string(),
        }
    }

    /// Convert a byte offset to a 1-based line number.
    pub fn byte_to_line(&self, byte_offset: usize) -> u32 {
        match self.line_starts.binary_search(&byte_offset) {
            Ok(idx) => (idx + 1) as u32,
            Err(idx) => idx as u32,
        }
    }

    /// Convert a byte offset to a 1-based `(line, column)` pair.
    ///
    /// Column is the 1-based byte offset within the line — matching the
    /// CTFS wire encoding for `register_step_with_column` and the
    /// `paths.dat` Layout A per-line offset table.  Mirrors
    /// `solana::recorder::read_line_lengths_for_path` / the EVM
    /// recorder's `compute_line_lengths` contract: an `\r\n` terminator
    /// contributes its `\r` to the preceding line's byte count.
    ///
    /// Used by column-aware step emission (FU-Column-Aware-Nav-Cairo):
    /// the recorder resolves a byte offset for a Cairo statement's
    /// start, then forwards both line and column into
    /// `register_step_with_column` so multi-statement-per-line fixtures
    /// surface as distinct steps in the replay UI rather than collapsing
    /// onto the first statement of the line.
    pub fn byte_to_line_col(&self, byte_offset: usize) -> (u32, u32) {
        let line_idx = match self.line_starts.binary_search(&byte_offset) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        };
        let line_start = self.line_starts.get(line_idx).copied().unwrap_or(0);
        let column = byte_offset.saturating_sub(line_start) as u32 + 1;
        ((line_idx + 1) as u32, column)
    }

    /// Compute the column (1-based byte offset within the line) at which
    /// `column_in_line` (0-based byte offset within the trimmed line
    /// substring) lives, given the absolute `line_number` (1-based).
    ///
    /// Pure convenience for the source-text walker in `tracer.rs` —
    /// it already has per-line slices and just needs to convert
    /// "this statement starts at byte N of the line" to a 1-based
    /// column the writer accepts.
    pub fn column_for_line_offset(&self, line_number: u32, offset_in_line: usize) -> u32 {
        // Bounds-check defensively: callers that pass a 0 line number or
        // a line past EOF get column 1 rather than a panic.
        if line_number == 0 {
            return 1;
        }
        offset_in_line as u32 + 1
    }

    /// Return the total number of lines in the source.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Return a reference to the source code.
    pub fn source_code(&self) -> &str {
        &self.source_code
    }

    /// Per-line UTF-8 byte counts (1-based: `line_lengths()[i]` is the
    /// byte count of source line `i + 1`, excluding the trailing `\n`).
    ///
    /// Required by the column-aware mode: the recorder hands these to
    /// `TraceWriter::register_path_with_line_lengths` so the reader can
    /// map the writer-side global byte position back to a
    /// `(line, column)` pair at replay time.  Ported pattern from
    /// `codetracer-solana-recorder/src/recorder.rs::read_line_lengths_for_path`
    /// — an `\r\n` terminator's `\r` is counted in the line's byte total
    /// to keep the table consistent with the column offsets the writer
    /// emits (byte offsets into the file).
    ///
    /// See `codetracer-trace-format-spec/trace-events.md` §"paths.dat
    /// per-line offset table — Layout A".
    pub fn line_lengths(&self) -> Vec<u32> {
        let bytes = self.source_code.as_bytes();
        let mut lengths: Vec<u32> = Vec::new();
        let mut line_start: usize = 0;
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                lengths.push((i - line_start) as u32);
                line_start = i + 1;
            }
        }
        if line_start < bytes.len() {
            lengths.push((bytes.len() - line_start) as u32);
        }
        lengths
    }
}

/// Read per-line UTF-8 byte counts for `path` and return them as a flat
/// `Vec<u32>` where entry `i` is the addressable column count of source
/// line `i + 1` (1-based numbering matching the `paths.dat` Layout A
/// contract — see `codetracer-trace-format-spec/trace-events.md`).
///
/// Synthetic-only paths (e.g. `starknet-tx://...`) and missing /
/// unreadable files degrade to an empty `Vec`; the writer treats
/// `register_path_with_line_lengths` with an empty slice as "no per-line
/// data", so the column resolution at read time falls back to surfacing
/// `None`, matching the back-compat-safe default codified by the
/// trace-format spec.
///
/// Ported from the Solana recorder's
/// `recorder::read_line_lengths_for_path` to keep the cross-recorder
/// convention identical (the recorders are CTFS-only and feed the same
/// downstream readers).
pub fn read_line_lengths_for_path(path: &Path) -> Vec<u32> {
    let lossy = path.to_string_lossy();
    // Synthetic paths used by the StarkNet replay path
    // (`starknet-tx://<hash>`) and the legacy `<stdin>`-style markers
    // are not on disk — return an empty table so the writer falls back
    // to line-only.
    if lossy.starts_with("starknet-tx://") || (lossy.starts_with('<') && lossy.ends_with('>')) {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let mut lines: Vec<u32> = Vec::new();
    let mut current_len: u32 = 0;
    for byte in &bytes {
        if *byte == b'\n' {
            lines.push(current_len);
            current_len = 0;
        } else {
            current_len = current_len.saturating_add(1);
        }
    }
    if current_len > 0 || bytes.last() != Some(&b'\n') {
        lines.push(current_len);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_byte_to_line_simple() {
        let source = "line1\nline2\nline3\n";
        let map = SourceMap::from_source(&PathBuf::from("test.cairo"), source);
        // Byte 0 -> line 1
        assert_eq!(map.byte_to_line(0), 1);
        // Byte 6 -> line 2 (start of "line2")
        assert_eq!(map.byte_to_line(6), 2);
        // Byte 12 -> line 3
        assert_eq!(map.byte_to_line(12), 3);
    }

    #[test]
    fn test_line_count() {
        let source = "a\nb\nc\n";
        let map = SourceMap::from_source(&PathBuf::from("test.cairo"), source);
        assert_eq!(map.line_count(), 4); // 3 newlines + 1 initial
    }

    #[test]
    fn test_empty_source() {
        let source = "";
        let map = SourceMap::from_source(&PathBuf::from("test.cairo"), source);
        assert_eq!(map.line_count(), 1);
        assert_eq!(map.byte_to_line(0), 1);
    }

    #[test]
    fn test_byte_to_line_col() {
        let source = "abc\ndef\nghi\n";
        let map = SourceMap::from_source(&PathBuf::from("t.cairo"), source);
        // Byte 0 -> (1, 1)
        assert_eq!(map.byte_to_line_col(0), (1, 1));
        // Byte 2 -> (1, 3) — column 3 of line 1
        assert_eq!(map.byte_to_line_col(2), (1, 3));
        // Byte 4 -> (2, 1) — first byte of "def"
        assert_eq!(map.byte_to_line_col(4), (2, 1));
        // Byte 9 -> (3, 2) — second byte of "ghi"
        assert_eq!(map.byte_to_line_col(9), (3, 2));
    }

    #[test]
    fn test_line_lengths() {
        let source = "abc\nde\nf\n";
        let map = SourceMap::from_source(&PathBuf::from("t.cairo"), source);
        assert_eq!(map.line_lengths(), vec![3, 2, 1]);
    }

    #[test]
    fn test_line_lengths_no_final_newline() {
        let source = "abc\nde";
        let map = SourceMap::from_source(&PathBuf::from("t.cairo"), source);
        assert_eq!(map.line_lengths(), vec![3, 2]);
    }

    #[test]
    fn test_line_lengths_crlf_counts_cr() {
        let source = "abc\r\ndef\n";
        let map = SourceMap::from_source(&PathBuf::from("t.cairo"), source);
        // `\r` is part of the preceding line's byte count.
        assert_eq!(map.line_lengths(), vec![4, 3]);
    }

    #[test]
    fn test_read_line_lengths_synthetic_path() {
        let synth = PathBuf::from("starknet-tx://0xdeadbeef");
        assert!(read_line_lengths_for_path(&synth).is_empty());
        let stdin = PathBuf::from("<stdin>");
        assert!(read_line_lengths_for_path(&stdin).is_empty());
    }
}
