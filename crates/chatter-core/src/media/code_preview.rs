//! Code preview data types.
//!
//! Simple data structures for syntax-highlighted code.
//! The actual rendering (via slint-code-view) lives in the UI crate.

/// A single syntax-highlighted span within a line.
#[derive(Clone, Debug)]
pub struct HighlightedSpan {
    /// The text content.
    pub text: String,
    /// Foreground color as ARGB.
    pub color: u32,
    /// Whether bold.
    pub bold: bool,
    /// Whether italic.
    pub italic: bool,
}

/// A syntax-highlighted line of code.
#[derive(Clone, Debug)]
pub struct HighlightedLine {
    /// Line number (1-indexed).
    pub line_number: u32,
    /// Highlighted spans in this line.
    pub spans: Vec<HighlightedSpan>,
    /// Raw text content (without highlighting).
    pub raw_text: String,
}
