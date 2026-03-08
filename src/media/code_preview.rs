//! Code preview.
//!
//! Re-exports from slint-code-view crate for syntax highlighting
//! with selection support.

pub use slint_code_view::{
    CodeViewConfig, CodeViewState, HighlightedLine, HighlightedSpan, Highlighter,
    LineComment, LineSelection, SelectionMode, TextSelection,
};

/// Convenience type alias for backward compatibility.
pub type CodePreview = Highlighter;
