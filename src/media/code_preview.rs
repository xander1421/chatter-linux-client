//! Code preview.
//!
//! Syntax highlighting for 40+ languages using syntect.
//!
//! Uses lazy static initialization for syntax and theme sets to avoid
//! repeated loading overhead while keeping the struct itself lightweight.

use std::fs;
use std::path::Path;

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::error::MediaError;

/// Lazily loaded syntax set with all default syntaxes.
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);

/// Lazily loaded theme set with default themes.
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

/// Default theme for syntax highlighting (dark theme to match UI constraint).
const DEFAULT_THEME: &str = "base16-ocean.dark";

/// A highlighted line of code.
#[derive(Debug, Clone)]
pub struct HighlightedLine {
    /// Line number (1-indexed).
    pub line_number: usize,
    /// Highlighted segments in this line.
    pub segments: Vec<HighlightedSegment>,
}

/// A segment of highlighted text within a line.
#[derive(Debug, Clone)]
pub struct HighlightedSegment {
    /// The text content of this segment.
    pub text: String,
    /// The style to apply to this segment.
    pub style: SegmentStyle,
}

/// Style information for a highlighted segment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SegmentStyle {
    /// Foreground color as RGB tuple.
    pub foreground: (u8, u8, u8),
    /// Whether the text should be bold.
    pub bold: bool,
    /// Whether the text should be italic.
    pub italic: bool,
    /// Whether the text should be underlined.
    pub underline: bool,
}

impl From<Style> for SegmentStyle {
    fn from(style: Style) -> Self {
        Self {
            foreground: (style.foreground.r, style.foreground.g, style.foreground.b),
            bold: style.font_style.contains(syntect::highlighting::FontStyle::BOLD),
            italic: style.font_style.contains(syntect::highlighting::FontStyle::ITALIC),
            underline: style.font_style.contains(syntect::highlighting::FontStyle::UNDERLINE),
        }
    }
}

/// Code preview with syntax highlighting.
///
/// This is a lightweight struct that uses lazily-loaded static syntax
/// and theme sets. It provides methods for highlighting code strings
/// and files with automatic language detection.
pub struct CodePreview;

impl CodePreview {
    /// Creates a new CodePreview instance.
    ///
    /// The instance is lightweight - syntax and theme sets are stored
    /// in static variables and shared across all instances.
    pub fn new() -> Self {
        Self
    }

    /// Highlights code with the specified language.
    ///
    /// # Arguments
    ///
    /// * `code` - The source code to highlight.
    /// * `language` - The language name (e.g., "rust", "python", "javascript").
    ///
    /// # Returns
    ///
    /// A vector of highlighted lines. If the language is not recognized,
    /// returns plain text (no syntax highlighting) without error.
    pub fn highlight(&self, code: &str, language: &str) -> Result<Vec<HighlightedLine>, MediaError> {
        let syntax = SYNTAX_SET
            .find_syntax_by_token(language)
            .or_else(|| SYNTAX_SET.find_syntax_by_extension(language))
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());

        let theme = THEME_SET
            .themes
            .get(DEFAULT_THEME)
            .ok_or_else(|| MediaError::PreviewError("default theme not found".to_string()))?;

        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut result = Vec::new();

        for (line_number, line) in LinesWithEndings::from(code).enumerate() {
            let ranges: Vec<(Style, &str)> = highlighter.highlight_line(line, &SYNTAX_SET).map_err(|e| {
                MediaError::PreviewError(format!("highlight error: {e}"))
            })?;

            let segments: Vec<HighlightedSegment> = ranges
                .into_iter()
                .map(|(style, text)| HighlightedSegment {
                    text: text.to_string(),
                    style: style.into(),
                })
                .collect();

            result.push(HighlightedLine {
                line_number: line_number + 1, // 1-indexed
                segments,
            });
        }

        Ok(result)
    }

    /// Highlights a file with automatic language detection.
    ///
    /// Language is detected from the file extension.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the source file.
    ///
    /// # Returns
    ///
    /// A vector of highlighted lines.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub fn highlight_file(&self, path: &Path) -> Result<Vec<HighlightedLine>, MediaError> {
        let code = fs::read_to_string(path)?;
        let language = self.detect_language(path).unwrap_or_else(|| "txt".to_string());
        self.highlight(&code, &language)
    }

    /// Detects the programming language from a file path.
    ///
    /// Detection is based on the file extension.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file.
    ///
    /// # Returns
    ///
    /// The detected language name, or `None` if unknown.
    pub fn detect_language(&self, path: &Path) -> Option<String> {
        let extension = path.extension()?.to_str()?;

        // Try to find syntax by extension
        SYNTAX_SET
            .find_syntax_by_extension(extension)
            .map(|syntax| syntax.name.to_lowercase())
    }

    /// Returns a list of supported file extensions.
    ///
    /// These are extensions that syntect can highlight.
    pub fn supported_extensions() -> &'static [&'static str] {
        // Common extensions from syntect's default set
        &[
            // Systems programming
            "rs", "c", "h", "cpp", "hpp", "cc", "cxx",
            // Web
            "js", "ts", "jsx", "tsx", "html", "htm", "css", "scss", "sass", "less",
            // Scripting
            "py", "rb", "pl", "sh", "bash", "zsh", "fish", "ps1",
            // JVM
            "java", "kt", "kts", "scala", "groovy", "clj",
            // .NET
            "cs", "fs", "vb",
            // Functional
            "hs", "ml", "mli", "elm", "ex", "exs", "erl",
            // Data/Config
            "json", "yaml", "yml", "toml", "xml", "ini", "conf",
            // Documentation
            "md", "rst", "tex", "latex",
            // Database
            "sql",
            // Mobile
            "swift", "m", "mm",
            // Go
            "go",
            // PHP
            "php",
            // Lua
            "lua",
            // Makefile
            "mk", "makefile",
            // Docker
            "dockerfile",
            // Other
            "diff", "patch", "vim", "r",
        ]
    }
}

impl Default for CodePreview {
    fn default() -> Self {
        Self::new()
    }
}
