//! File-category icons for attachment display.

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
}
