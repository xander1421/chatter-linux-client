//! File attachment data structures for Slint UI bindings.
//!
//! Provides Rust structs that map to Slint struct definitions for
//! file attachment display components.

use crate::media::code_preview::HighlightedLine;

// ============================================================================
// Code Line Data
// ============================================================================

/// A single syntax-highlighted segment within a line.
#[derive(Clone, Debug, Default)]
pub struct CodeSegmentData {
    /// The text content.
    pub text: String,
    /// RGB color as (r, g, b).
    pub color: (u8, u8, u8),
}

impl CodeSegmentData {
    /// Create a new segment with text and color.
    #[must_use]
    pub fn new(text: impl Into<String>, color: (u8, u8, u8)) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

/// A single line of code for display in the code preview panel.
///
/// Contains line number and syntax-highlighted segments.
#[derive(Clone, Debug, Default)]
pub struct CodeLineData {
    /// Line number (1-indexed).
    pub line_number: i32,
    /// Syntax-highlighted segments with colors.
    pub segments: Vec<CodeSegmentData>,
}

impl CodeLineData {
    /// Create a new code line entry.
    #[must_use]
    pub fn new(line_number: i32, segments: Vec<CodeSegmentData>) -> Self {
        Self {
            line_number,
            segments,
        }
    }

    /// Create a plain text line (single segment, default color).
    #[must_use]
    pub fn plain(line_number: i32, content: impl Into<String>) -> Self {
        Self {
            line_number,
            segments: vec![CodeSegmentData::new(content, (192, 197, 206))], // code-text color
        }
    }
}

/// Convert highlighted lines from syntect into CodeLineData for Slint.
///
/// Preserves syntax highlighting colors and converts tabs to spaces.
#[must_use]
pub fn flatten_code_lines(lines: &[HighlightedLine]) -> Vec<CodeLineData> {
    lines
        .iter()
        .map(|line| {
            let segments = line
                .spans
                .iter()
                .map(|span| {
                    // Convert tabs to 4 spaces and strip trailing newlines
                    let text = span.text.replace('\t', "    ").trim_end_matches('\n').to_string();
                    // Extract RGB from ARGB u32
                    let r = ((span.color >> 16) & 0xFF) as u8;
                    let g = ((span.color >> 8) & 0xFF) as u8;
                    let b = (span.color & 0xFF) as u8;
                    CodeSegmentData::new(text, (r, g, b))
                })
                .collect();
            CodeLineData {
                line_number: line.line_number as i32,
                segments,
            }
        })
        .collect()
}

// ============================================================================
// File Preview Data
// ============================================================================

/// Preview type enum for file preview panels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PreviewType {
    /// Code file with syntax highlighting.
    Code,
    /// PDF document.
    Pdf,
    /// Image file.
    Image,
    /// Video file.
    Video,
    /// Audio file.
    Audio,
    /// No preview available.
    #[default]
    None,
}

impl PreviewType {
    /// Convert to string representation for Slint binding.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Pdf => "pdf",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::None => "none",
        }
    }
}

impl From<&str> for PreviewType {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "code" => Self::Code,
            "pdf" => Self::Pdf,
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            _ => Self::None,
        }
    }
}

/// File preview data for panel state binding.
///
/// Contains metadata and state for code preview panels and PDF preview panels.
#[derive(Clone, Debug, Default)]
pub struct FilePreviewData {
    /// File name to display.
    pub file_name: String,
    /// Full file path for opening externally.
    pub file_path: String,
    /// Type of preview: "code", "pdf", "image", "video", "audio", "none".
    pub preview_type: PreviewType,
    /// Programming language for code files (e.g., "rust", "python").
    pub language: String,
    /// Total line count for code files.
    pub line_count: i32,
    /// Total page count for PDF files.
    pub page_count: i32,
    /// Current page number for PDF files (1-indexed).
    pub current_page: i32,
    /// Error message if preview failed.
    pub error_message: String,
}

impl FilePreviewData {
    /// Create a new file preview data entry.
    #[must_use]
    pub fn new(file_name: impl Into<String>, file_path: impl Into<String>) -> Self {
        Self {
            file_name: file_name.into(),
            file_path: file_path.into(),
            preview_type: PreviewType::None,
            language: String::new(),
            line_count: 0,
            page_count: 0,
            current_page: 1,
            error_message: String::new(),
        }
    }

    /// Configure for code preview.
    #[must_use]
    pub fn with_code(mut self, language: impl Into<String>, line_count: i32) -> Self {
        self.preview_type = PreviewType::Code;
        self.language = language.into();
        self.line_count = line_count;
        self
    }

    /// Configure for PDF preview.
    #[must_use]
    pub fn with_pdf(mut self, page_count: i32) -> Self {
        self.preview_type = PreviewType::Pdf;
        self.page_count = page_count;
        self.current_page = 1;
        self
    }

    /// Configure for image preview.
    #[must_use]
    pub fn with_image(mut self) -> Self {
        self.preview_type = PreviewType::Image;
        self
    }

    /// Configure for video preview.
    #[must_use]
    pub fn with_video(mut self) -> Self {
        self.preview_type = PreviewType::Video;
        self
    }

    /// Configure for audio preview.
    #[must_use]
    pub fn with_audio(mut self) -> Self {
        self.preview_type = PreviewType::Audio;
        self
    }

    /// Set error message for failed preview.
    #[must_use]
    pub fn with_error(mut self, message: impl Into<String>) -> Self {
        self.error_message = message.into();
        self
    }

    /// Set current page for PDF preview.
    #[must_use]
    pub fn with_current_page(mut self, page: i32) -> Self {
        self.current_page = page.max(1);
        self
    }

    /// Get preview type as string for Slint binding.
    #[must_use]
    pub fn preview_type_str(&self) -> &'static str {
        self.preview_type.as_str()
    }

    /// Check if preview has an error.
    #[must_use]
    pub fn has_error(&self) -> bool {
        !self.error_message.is_empty()
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Get an icon emoji for a file category.
///
/// Returns an appropriate emoji icon based on the file category string.
#[must_use]
pub fn category_icon(category: &str) -> &'static str {
    match category {
        "image" => "\u{1F5BC}\u{FE0F}", // Framed picture
        "video" => "\u{1F3AC}",          // Clapper board
        "audio" => "\u{1F3B5}",          // Musical note
        "document" => "\u{1F4C4}",       // Page facing up
        "code" => "\u{1F4DD}",           // Memo
        "archive" => "\u{1F4E6}",        // Package
        _ => "\u{1F4CE}",                // Paperclip (default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_category_icon() {
        assert_eq!(category_icon("image"), "\u{1F5BC}\u{FE0F}");
        assert_eq!(category_icon("video"), "\u{1F3AC}");
        assert_eq!(category_icon("audio"), "\u{1F3B5}");
        assert_eq!(category_icon("document"), "\u{1F4C4}");
        assert_eq!(category_icon("code"), "\u{1F4DD}");
        assert_eq!(category_icon("archive"), "\u{1F4E6}");
        assert_eq!(category_icon("unknown"), "\u{1F4CE}");
        assert_eq!(category_icon(""), "\u{1F4CE}");
    }

    // ========================================================================
    // CodeLineData tests
    // ========================================================================

    #[test]
    fn test_code_segment_data_new() {
        let segment = CodeSegmentData::new("hello", (255, 128, 0));

        assert_eq!(segment.text, "hello");
        assert_eq!(segment.color, (255, 128, 0));
    }

    #[test]
    fn test_code_line_data_new() {
        let segments = vec![
            CodeSegmentData::new("fn ", (100, 100, 100)),
            CodeSegmentData::new("main", (200, 200, 200)),
        ];
        let line = CodeLineData::new(1, segments);

        assert_eq!(line.line_number, 1);
        assert_eq!(line.segments.len(), 2);
        assert_eq!(line.segments[0].text, "fn ");
        assert_eq!(line.segments[1].text, "main");
    }

    #[test]
    fn test_code_line_data_plain() {
        let line = CodeLineData::plain(5, "let x = 42;");

        assert_eq!(line.line_number, 5);
        assert_eq!(line.segments.len(), 1);
        assert_eq!(line.segments[0].text, "let x = 42;");
        assert_eq!(line.segments[0].color, (192, 197, 206)); // default color
    }

    #[test]
    fn test_flatten_code_lines() {
        use crate::media::code_preview::{HighlightedLine, HighlightedSpan};

        // Color format: 0xAARRGGBB
        let red: u32 = 0xFF_FF_00_00;   // (255, 0, 0)
        let green: u32 = 0xFF_00_FF_00; // (0, 255, 0)
        let default_color: u32 = 0xFF_C0_C5_CE; // default gray (192, 197, 206)

        let lines = vec![
            HighlightedLine {
                line_number: 1,
                raw_text: "fn main()\n".to_string(),
                spans: vec![
                    HighlightedSpan {
                        text: "fn ".to_string(),
                        color: red,
                        bold: false,
                        italic: false,
                    },
                    HighlightedSpan {
                        text: "main".to_string(),
                        color: green,
                        bold: false,
                        italic: false,
                    },
                    HighlightedSpan {
                        text: "()\n".to_string(),
                        color: default_color,
                        bold: false,
                        italic: false,
                    },
                ],
            },
            HighlightedLine {
                line_number: 2,
                raw_text: "}\n".to_string(),
                spans: vec![HighlightedSpan {
                    text: "}\n".to_string(),
                    color: default_color,
                    bold: false,
                    italic: false,
                }],
            },
        ];

        let result = flatten_code_lines(&lines);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].line_number, 1);
        assert_eq!(result[0].segments.len(), 3);
        assert_eq!(result[0].segments[0].text, "fn ");
        assert_eq!(result[0].segments[0].color, (255, 0, 0));
        assert_eq!(result[0].segments[1].text, "main");
        assert_eq!(result[0].segments[1].color, (0, 255, 0));
        assert_eq!(result[0].segments[2].text, "()"); // newline stripped
        assert_eq!(result[1].line_number, 2);
        assert_eq!(result[1].segments[0].text, "}"); // newline stripped
    }

    // ========================================================================
    // FilePreviewData tests
    // ========================================================================

    #[test]
    fn test_file_preview_data_new() {
        let preview = FilePreviewData::new("example.rs", "/path/to/example.rs");

        assert_eq!(preview.file_name, "example.rs");
        assert_eq!(preview.file_path, "/path/to/example.rs");
        assert_eq!(preview.preview_type, PreviewType::None);
        assert!(preview.language.is_empty());
        assert_eq!(preview.line_count, 0);
        assert_eq!(preview.page_count, 0);
        assert_eq!(preview.current_page, 1);
        assert!(preview.error_message.is_empty());
    }

    #[test]
    fn test_file_preview_data_with_code() {
        let preview = FilePreviewData::new("main.rs", "/src/main.rs").with_code("rust", 150);

        assert_eq!(preview.preview_type, PreviewType::Code);
        assert_eq!(preview.language, "rust");
        assert_eq!(preview.line_count, 150);
        assert_eq!(preview.preview_type_str(), "code");
    }

    #[test]
    fn test_file_preview_data_with_pdf() {
        let preview = FilePreviewData::new("document.pdf", "/docs/document.pdf").with_pdf(25);

        assert_eq!(preview.preview_type, PreviewType::Pdf);
        assert_eq!(preview.page_count, 25);
        assert_eq!(preview.current_page, 1);
        assert_eq!(preview.preview_type_str(), "pdf");
    }

    #[test]
    fn test_file_preview_data_with_current_page() {
        let preview = FilePreviewData::new("doc.pdf", "/docs/doc.pdf")
            .with_pdf(10)
            .with_current_page(5);

        assert_eq!(preview.current_page, 5);
    }

    #[test]
    fn test_file_preview_data_with_current_page_clamped() {
        let preview = FilePreviewData::new("doc.pdf", "/docs/doc.pdf")
            .with_pdf(10)
            .with_current_page(-5);

        assert_eq!(preview.current_page, 1);
    }

    #[test]
    fn test_file_preview_data_with_image() {
        let preview = FilePreviewData::new("photo.jpg", "/images/photo.jpg").with_image();

        assert_eq!(preview.preview_type, PreviewType::Image);
        assert_eq!(preview.preview_type_str(), "image");
    }

    #[test]
    fn test_file_preview_data_with_video() {
        let preview = FilePreviewData::new("video.mp4", "/videos/video.mp4").with_video();

        assert_eq!(preview.preview_type, PreviewType::Video);
        assert_eq!(preview.preview_type_str(), "video");
    }

    #[test]
    fn test_file_preview_data_with_audio() {
        let preview = FilePreviewData::new("song.mp3", "/music/song.mp3").with_audio();

        assert_eq!(preview.preview_type, PreviewType::Audio);
        assert_eq!(preview.preview_type_str(), "audio");
    }

    #[test]
    fn test_file_preview_data_with_error() {
        let preview = FilePreviewData::new("broken.rs", "/path/broken.rs")
            .with_error("Failed to read file");

        assert!(preview.has_error());
        assert_eq!(preview.error_message, "Failed to read file");
    }

    #[test]
    fn test_file_preview_data_no_error() {
        let preview = FilePreviewData::new("good.rs", "/path/good.rs").with_code("rust", 50);

        assert!(!preview.has_error());
    }

    #[test]
    fn test_preview_type_from_str() {
        assert_eq!(PreviewType::from("code"), PreviewType::Code);
        assert_eq!(PreviewType::from("pdf"), PreviewType::Pdf);
        assert_eq!(PreviewType::from("image"), PreviewType::Image);
        assert_eq!(PreviewType::from("video"), PreviewType::Video);
        assert_eq!(PreviewType::from("audio"), PreviewType::Audio);
        assert_eq!(PreviewType::from("unknown"), PreviewType::None);
        assert_eq!(PreviewType::from("CODE"), PreviewType::Code); // Case insensitive
    }
}
