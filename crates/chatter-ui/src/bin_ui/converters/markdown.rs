//! Simple markdown parser for chat message rendering.
//!
//! Handles the subset of markdown produced by the composer formatting toolbar:
//! `**bold**`, `*italic*`, `~~strikethrough~~`, `` `code` ``, and fenced code blocks.
//!
//! Produces a flat list of styled spans suitable for Slint's block-level rendering.
//! Since Slint doesn't support inline rich text (mixed styles in a single wrapped
//! text flow), each span is rendered as a separate element.

/// A parsed rich text span for Slint rendering.
///
/// Each span maps to one Slint `RichTextSpan` struct element.
/// Flags are mutually exclusive in practice (a span won't be both bold and code).
#[derive(Clone, Debug, Default)]
pub(crate) struct MarkdownSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub code_block: bool,
}

/// Parse markdown text into styled spans.
///
/// Code block content preserves original whitespace.
pub(crate) fn parse_markdown(input: &str) -> Vec<MarkdownSpan> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return vec![MarkdownSpan {
            text: String::new(),
            ..Default::default()
        }];
    }

    let mut result = Vec::new();

    // Split on fenced code block boundaries first
    let blocks = split_code_blocks(trimmed);

    for block in blocks {
        if block.is_code_block {
            // Code blocks: preserve whitespace
            result.push(MarkdownSpan {
                text: block.text,
                code_block: true,
                ..Default::default()
            });
        } else if !block.text.is_empty() {
            // Parse inline formatting within text blocks
            let spans = parse_inline(&block.text);
            result.extend(spans);
        }
    }

    if result.is_empty() {
        result.push(MarkdownSpan {
            text: trimmed.to_string(),
            ..Default::default()
        });
    }

    result
}

/// Strip all markdown markers from text, returning clean plaintext.
///
/// Used for the `content` field (reply previews, empty checks, system messages).
pub(crate) fn strip_markdown(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let blocks = split_code_blocks(trimmed);
    let mut result = String::new();

    for block in blocks {
        if block.is_code_block {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&block.text);
        } else {
            let spans = parse_inline(&block.text);
            for span in spans {
                result.push_str(&span.text);
            }
        }
    }

    result
}

// --- Internal types and helpers ---

struct TextBlock {
    text: String,
    is_code_block: bool,
}

/// Split text on fenced code block boundaries (``` ... ```).
fn split_code_blocks(input: &str) -> Vec<TextBlock> {
    let mut blocks = Vec::new();
    let mut remaining = input;

    loop {
        // Find opening ```
        let Some(open_pos) = remaining.find("```") else {
            // No more code blocks — rest is plain text
            if !remaining.is_empty() {
                blocks.push(TextBlock {
                    text: remaining.to_string(),
                    is_code_block: false,
                });
            }
            break;
        };

        // Push text before the code block
        let before = &remaining[..open_pos];
        if !before.is_empty() {
            blocks.push(TextBlock {
                text: before.to_string(),
                is_code_block: false,
            });
        }

        // Skip the opening ``` and any language identifier on the same line
        let after_open = &remaining[open_pos + 3..];
        let content_start = after_open.find('\n').map(|p| p + 1).unwrap_or(0);
        let code_content = &after_open[content_start..];

        // Find closing ```
        if let Some(close_pos) = code_content.find("```") {
            let code_text = code_content[..close_pos].trim_end_matches('\n');
            blocks.push(TextBlock {
                text: code_text.to_string(),
                is_code_block: true,
            });
            // Advance past closing ```
            let skip = open_pos + 3 + content_start + close_pos + 3;
            remaining = &remaining[skip..];
        } else {
            // No closing ``` — treat rest as code block
            let code_text = code_content.trim_end_matches('\n');
            blocks.push(TextBlock {
                text: code_text.to_string(),
                is_code_block: true,
            });
            break;
        }
    }

    blocks
}

/// Parse inline markdown formatting within a text block.
///
/// Handles: `**bold**`, `*italic*`, `~~strikethrough~~`, `` `code` ``
fn parse_inline(text: &str) -> Vec<MarkdownSpan> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < len {
        // ** bold **
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if let Some(end) = find_closing_pair(&chars, i + 2, '*', '*') {
                flush_normal(&mut current, &mut spans);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(MarkdownSpan {
                    text: inner,
                    bold: true,
                    ..Default::default()
                });
                i = end + 2;
                continue;
            }
        }

        // ~~ strikethrough ~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_closing_pair(&chars, i + 2, '~', '~') {
                flush_normal(&mut current, &mut spans);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(MarkdownSpan {
                    text: inner,
                    strikethrough: true,
                    ..Default::default()
                });
                i = end + 2;
                continue;
            }
        }

        // * italic * (single asterisk, not preceded by another *)
        if chars[i] == '*' && (i + 1 >= len || chars[i + 1] != '*') {
            if let Some(end) = find_closing_single(&chars, i + 1, '*') {
                // Make sure the closing * isn't part of **
                if end + 1 >= len || chars[end + 1] != '*' {
                    flush_normal(&mut current, &mut spans);
                    let inner: String = chars[i + 1..end].iter().collect();
                    spans.push(MarkdownSpan {
                        text: inner,
                        italic: true,
                        ..Default::default()
                    });
                    i = end + 1;
                    continue;
                }
            }
        }

        // ` inline code `
        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                flush_normal(&mut current, &mut spans);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(MarkdownSpan {
                    text: inner,
                    code: true,
                    ..Default::default()
                });
                i = end + 1;
                continue;
            }
        }

        current.push(chars[i]);
        i += 1;
    }

    flush_normal(&mut current, &mut spans);
    spans
}

/// Flush accumulated normal text into a span.
fn flush_normal(buf: &mut String, spans: &mut Vec<MarkdownSpan>) {
    if !buf.is_empty() {
        spans.push(MarkdownSpan {
            text: buf.clone(),
            ..Default::default()
        });
        buf.clear();
    }
}

/// Find a closing pair of identical characters (e.g., `**`) starting at `from`.
/// Returns the index of the first character of the closing pair, or None.
fn find_closing_pair(chars: &[char], from: usize, c1: char, c2: char) -> Option<usize> {
    let len = chars.len();
    let mut i = from;
    while i + 1 < len {
        if chars[i] == c1 && chars[i + 1] == c2 {
            // Don't match empty content
            if i > from {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Find a closing single character starting at `from`.
/// Returns the index of the closing character, or None.
fn find_closing_single(chars: &[char], from: usize, c: char) -> Option<usize> {
    for i in from..chars.len() {
        if chars[i] == c && i > from {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        let spans = parse_markdown("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert!(!spans[0].bold);
        assert!(!spans[0].italic);
    }

    #[test]
    fn bold_whole_message() {
        let spans = parse_markdown("**hello world**");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert!(spans[0].bold);
    }

    #[test]
    fn italic_whole_message() {
        let spans = parse_markdown("*hello world*");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert!(spans[0].italic);
    }

    #[test]
    fn strikethrough_whole_message() {
        let spans = parse_markdown("~~hello world~~");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert!(spans[0].strikethrough);
    }

    #[test]
    fn inline_code() {
        let spans = parse_markdown("`hello world`");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert!(spans[0].code);
    }

    #[test]
    fn mixed_inline() {
        let spans = parse_markdown("hello **world** how are you");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "hello ");
        assert!(!spans[0].bold);
        assert_eq!(spans[1].text, "world");
        assert!(spans[1].bold);
        assert_eq!(spans[2].text, " how are you");
        assert!(!spans[2].bold);
    }

    #[test]
    fn code_block() {
        let spans = parse_markdown("before\n```\nlet x = 1;\n```\nafter");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "before\n");
        assert!(!spans[0].code_block);
        assert_eq!(spans[1].text, "let x = 1;");
        assert!(spans[1].code_block);
        assert_eq!(spans[2].text, "\nafter");
        assert!(!spans[2].code_block);
    }

    #[test]
    fn strip_markdown_basic() {
        assert_eq!(strip_markdown("**bold**"), "bold");
        assert_eq!(strip_markdown("*italic*"), "italic");
        assert_eq!(strip_markdown("~~strike~~"), "strike");
        assert_eq!(strip_markdown("`code`"), "code");
        assert_eq!(strip_markdown("hello **world**"), "hello world");
    }

    #[test]
    fn unmatched_markers_pass_through() {
        let spans = parse_markdown("hello * world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello * world");
        assert!(!spans[0].italic);
    }
}
