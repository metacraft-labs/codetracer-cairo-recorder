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

    /// Return the total number of lines in the source.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Return a reference to the source code.
    pub fn source_code(&self) -> &str {
        &self.source_code
    }
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
}
