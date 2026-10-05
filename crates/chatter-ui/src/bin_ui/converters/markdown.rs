//! Simple markdown parser for chat message rendering.
//!
//! Handles the subset of markdown produced by the composer formatting toolbar:
//! `**bold**`, `*italic*`, `~~strikethrough~~`, `` `code` ``, and fenced code blocks.
//!
//! [`parse_blocks`] turns a body into what the bubble lays out: the inline
//! spans between fenced code blocks become one Slint `StyledText` each, so
//! mixed styles flow and wrap together in one paragraph, and each fenced code
//! block stays its own monospace box. The `StyledText` is built straight from
//! these spans (`StyledText::from_paragraphs`, a patch in vendor/i-slint-core),
//! never through Slint's own markdown parser, which rejects headings, quotes,
//! fenced code and HTML that users type.

use i_slint_core::styled_text::{FormattedSpan, Style, StyledTextParagraph};

/// A parsed inline span.
///
/// Flags are mutually exclusive in practice (a span won't be both bold and code).
#[derive(Clone, Debug, Default)]
struct MarkdownSpan {
    text: String,
    bold: bool,
    italic: bool,
    strikethrough: bool,
    code: bool,
}

/// One block of a message body as the bubble lays it out.
#[derive(Debug, PartialEq)]
pub(crate) enum MarkdownBlock {
    /// Inline text, one paragraph per line, its styles as byte ranges.
    Inline(Vec<StyledTextParagraph>),
    /// A fenced code block's content, whitespace preserved.
    Code(String),
}

/// Parse a message body into the blocks the bubble draws: inline runs (split
/// into lines) and fenced code blocks. Newlines that only separate a code
/// block from its neighbours are dropped, so a code block does not gain a
/// blank line above and below it.
pub(crate) fn parse_blocks(input: &str) -> Vec<MarkdownBlock> {
    split_code_blocks(input.trim())
        .into_iter()
        .filter_map(|block| {
            if block.is_code_block {
                return Some(MarkdownBlock::Code(block.text));
            }
            let text = block.text.trim_matches(|c| c == '\n' || c == '\r');
            (!text.is_empty()).then(|| MarkdownBlock::Inline(paragraphs(&parse_inline(text))))
        })
        .collect()
}

/// Lay inline spans out as paragraphs, one per line. A span that crosses a
/// line break is split into one formatting range per line.
fn paragraphs(spans: &[MarkdownSpan]) -> Vec<StyledTextParagraph> {
    let mut out = Vec::new();
    let mut para = StyledTextParagraph { text: String::new(), formatting: Vec::new(), links: Vec::new() };
    for span in spans {
        let style = if span.bold {
            Some(Style::Strong)
        } else if span.italic {
            Some(Style::Emphasis)
        } else if span.strikethrough {
            Some(Style::Strikethrough)
        } else if span.code {
            Some(Style::Code)
        } else {
            None
        };
        for (i, line) in span.text.split('\n').enumerate() {
            if i > 0 {
                out.push(std::mem::replace(
                    &mut para,
                    StyledTextParagraph { text: String::new(), formatting: Vec::new(), links: Vec::new() },
                ));
            }
            // Parley treats a lone CR as a line break of its own.
            let line = line.strip_suffix('\r').unwrap_or(line);
            let start = para.text.len();
            para.text.push_str(line);
            if let (Some(style), false) = (&style, line.is_empty()) {
                para.formatting.push(FormattedSpan { range: start..para.text.len(), style: style.clone() });
            }
        }
    }
    out.push(para);
    out
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

    /// The single inline paragraph of `input`, as (text, [(styled text, style)]).
    fn one_paragraph(input: &str) -> (String, Vec<(String, Style)>) {
        let blocks = parse_blocks(input);
        let [MarkdownBlock::Inline(paras)] = blocks.as_slice() else {
            panic!("{input:?}: expected one inline block, got {blocks:?}");
        };
        let [p] = paras.as_slice() else { panic!("{input:?}: expected one line, got {paras:?}") };
        let styled = p.formatting.iter().map(|f| (p.text[f.range.clone()].to_string(), f.style.clone())).collect();
        (p.text.clone(), styled)
    }

    #[test]
    fn plain_text_has_no_formatting() {
        assert_eq!(one_paragraph("hello world"), ("hello world".into(), vec![]));
    }

    #[test]
    fn each_marker_maps_to_its_style() {
        for (input, style) in [
            ("**hello world**", Style::Strong),
            ("*hello world*", Style::Emphasis),
            ("~~hello world~~", Style::Strikethrough),
            ("`hello world`", Style::Code),
        ] {
            assert_eq!(one_paragraph(input), ("hello world".into(), vec![("hello world".into(), style)]), "{input}");
        }
    }

    /// The bug this layout fixes: mixed styles were one block each, so each
    /// sat on its own line. Now they are ranges in ONE paragraph.
    #[test]
    fn mixed_styles_share_one_paragraph() {
        assert_eq!(
            one_paragraph("**bold** *italic* ~~strike~~ `code` end"),
            (
                "bold italic strike code end".into(),
                vec![
                    ("bold".into(), Style::Strong),
                    ("italic".into(), Style::Emphasis),
                    ("strike".into(), Style::Strikethrough),
                    ("code".into(), Style::Code),
                ]
            )
        );
    }

    #[test]
    fn ranges_are_bytes_not_chars() {
        assert_eq!(one_paragraph("héllo **wörld** 🙂"), ("héllo wörld 🙂".into(), vec![("wörld".into(), Style::Strong)]));
    }

    #[test]
    fn lines_become_paragraphs_and_a_style_crossing_one_is_split() {
        let blocks = parse_blocks("a **b\r\nc** d");
        let [MarkdownBlock::Inline(paras)] = blocks.as_slice() else { panic!("{blocks:?}") };
        let lines: Vec<_> = paras.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(lines, ["a b", "c d"]);
        assert_eq!(paras[0].formatting, [FormattedSpan { range: 2..3, style: Style::Strong }]);
        assert_eq!(paras[1].formatting, [FormattedSpan { range: 0..1, style: Style::Strong }]);
    }

    #[test]
    fn code_blocks_stand_alone_without_padding_lines() {
        let blocks = parse_blocks("before\n```rust\nlet x = 1;\n```\nafter");
        let texts: Vec<_> = blocks
            .iter()
            .map(|b| match b {
                MarkdownBlock::Inline(p) => format!("inline:{}", p.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("|")),
                MarkdownBlock::Code(c) => format!("code:{c}"),
            })
            .collect();
        assert_eq!(texts, ["inline:before", "code:let x = 1;", "inline:after"]);
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
        assert_eq!(one_paragraph("hello * world"), ("hello * world".into(), vec![]));
    }
}
