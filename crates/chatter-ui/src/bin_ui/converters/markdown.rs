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
use chatter_core::domain::mention::{MentionExtractor, MentionMember, MentionType};

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
    link: Option<String>,
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
pub(crate) fn parse_blocks(
    input: &str,
    members: &[MentionMember],
    broadcast: bool,
) -> Vec<MarkdownBlock> {
    split_code_blocks(input.trim())
        .into_iter()
        .filter_map(|block| {
            if block.is_code_block {
                return Some(MarkdownBlock::Code(block.text));
            }
            let text = block.text.trim_matches(|c| c == '\n' || c == '\r');
            (!text.is_empty()).then(|| {
                let mut paragraphs = paragraphs(&parse_inline(text));
                for paragraph in &mut paragraphs {
                    // Bare web addresses link too, as in Signal and Telegram.
                    for (range, url) in bare_urls(&paragraph.text) {
                        if paragraph.formatting.iter().any(|span| span.style == Style::Code
                            && span.range.start < range.end && range.start < span.range.end)
                            || paragraph.links.iter().any(|(link, _)|
                                link.start < range.end && range.start < link.end)
                        {
                            continue;
                        }
                        paragraph.formatting.push(FormattedSpan { range: range.clone(), style: Style::Link });
                        paragraph.links.push((range, url));
                    }
                    for mention in MentionExtractor::resolve_mentions(&paragraph.text, members, broadcast) {
                        let range = mention.start_offset..mention.end_offset;
                        if paragraph.formatting.iter().any(|span| span.style == Style::Code
                            && span.range.start < range.end && range.start < span.range.end)
                            || paragraph.links.iter().any(|(link, _)|
                                link.start < range.end && range.start < link.end)
                        {
                            continue;
                        }
                        paragraph.formatting.push(FormattedSpan { range: range.clone(), style: Style::Link });
                        paragraph.formatting.push(FormattedSpan { range: range.clone(), style: Style::Strong });
                        if mention.mention_type == MentionType::User {
                            if let Some(id) = mention.target_id {
                                paragraph.links.push((range, format!("chatter-mention:{id}")));
                            }
                        }
                    }
                }
                MarkdownBlock::Inline(paragraphs)
            })
        })
        .collect()
}

/// Lay inline spans out as paragraphs, one per line. A span that crosses a
/// line break is split into one formatting range per line.
fn paragraphs(spans: &[MarkdownSpan]) -> Vec<StyledTextParagraph> {
    let mut out = Vec::new();
    let mut para = StyledTextParagraph {
        text: String::new(),
        formatting: Vec::new(),
        links: Vec::new(),
    };
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
                    StyledTextParagraph {
                        text: String::new(),
                        formatting: Vec::new(),
                        links: Vec::new(),
                    },
                ));
            }
            // Parley treats a lone CR as a line break of its own.
            let line = line.strip_suffix('\r').unwrap_or(line);
            let start = para.text.len();
            para.text.push_str(line);
            if let Some(url) = &span.link {
                if !line.is_empty() {
                    para.links.push((start..para.text.len(), url.clone()));
                    para.formatting.push(FormattedSpan {
                        range: start..para.text.len(),
                        style: Style::Link,
                    });
                }
            }
            if let (Some(style), false) = (&style, line.is_empty()) {
                para.formatting.push(FormattedSpan {
                    range: start..para.text.len(),
                    style: style.clone(),
                });
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
        // Only explicit, validated web links. Code spans are consumed whole
        // below, so their contents never become live links.
        if chars[i] == '[' && (i == 0 || chars[i - 1] != '!') {
            if let Some(label_end) = (i + 1..len).find(|&j| chars[j] == ']') {
                if label_end > i + 1 && chars.get(label_end + 1) == Some(&'(') {
                    if let Some(end) = (label_end + 2..len).find(|&j| chars[j] == ')') {
                        let target: String = chars[label_end + 2..end].iter().collect();
                        if let Some(url) = safe_web_url(&target) {
                            flush_normal(&mut current, &mut spans);
                            let label: String = chars[i + 1..label_end].iter().collect();
                            let mut label_spans = parse_inline(&label);
                            for span in &mut label_spans {
                                span.link = Some(url.clone());
                            }
                            spans.extend(label_spans);
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
        }
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

        // _ italic _ — only at word edges, so snake_case stays text
        // (CommonMark's intraword rule; Signal and Telegram read _x_ too).
        if chars[i] == '_' && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            if let Some(end) = find_closing_single(&chars, i + 1, '_') {
                let edges_ok = end > i + 1
                    && !chars[i + 1].is_whitespace()
                    && !chars[end - 1].is_whitespace()
                    && (end + 1 >= len || !chars[end + 1].is_alphanumeric());
                if edges_ok {
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
    fn mentions_are_styled_and_linked_only_outside_code() {
        let user_id = chatter_core::domain::ids::UserId::new();
        let members = [MentionMember { user_id, names: vec!["Alpha".into()] }];
        let blocks = parse_blocks("hey @Alpha `@Alpha` @here", &members, true);
        let [MarkdownBlock::Inline(paras)] = blocks.as_slice() else { panic!("inline block expected") };
        let [para] = paras.as_slice() else { panic!("one paragraph expected") };
        assert_eq!(para.links.len(), 1);
        assert_eq!(&para.text[para.links[0].0.clone()], "@Alpha");
        assert_eq!(para.links[0].1, format!("chatter-mention:{user_id}"));
        assert_eq!(para.formatting.iter().filter(|span| span.style == Style::Link).count(), 2);
    }

    /// The single inline paragraph of `input`, as (text, [(styled text, style)]).
    fn one_paragraph(input: &str) -> (String, Vec<(String, Style)>) {
        let blocks = parse_blocks(input, &[], false);
        let [MarkdownBlock::Inline(paras)] = blocks.as_slice() else {
            panic!("{input:?}: expected one inline block, got {blocks:?}");
        };
        let [p] = paras.as_slice() else {
            panic!("{input:?}: expected one line, got {paras:?}")
        };
        let styled = p
            .formatting
            .iter()
            .map(|f| (p.text[f.range.clone()].to_string(), f.style.clone()))
            .collect();
        (p.text.clone(), styled)
    }

    #[test]
    fn plain_text_has_no_formatting() {
        assert_eq!(one_paragraph("hello world"), ("hello world".into(), vec![]));
    }

    #[test]
    fn snake_case_is_not_italic() {
        assert_eq!(
            one_paragraph("call some_fn_name now"),
            ("call some_fn_name now".into(), vec![])
        );
    }

    #[test]
    fn each_marker_maps_to_its_style() {
        for (input, style) in [
            ("**hello world**", Style::Strong),
            ("*hello world*", Style::Emphasis),
            ("_hello world_", Style::Emphasis),
            ("~~hello world~~", Style::Strikethrough),
            ("`hello world`", Style::Code),
        ] {
            assert_eq!(
                one_paragraph(input),
                ("hello world".into(), vec![("hello world".into(), style)]),
                "{input}"
            );
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
        assert_eq!(
            one_paragraph("héllo **wörld** 🙂"),
            (
                "héllo wörld 🙂".into(),
                vec![("wörld".into(), Style::Strong)]
            )
        );
    }

    #[test]
    fn lines_become_paragraphs_and_a_style_crossing_one_is_split() {
        let blocks = parse_blocks("a **b\r\nc** d", &[], false);
        let [MarkdownBlock::Inline(paras)] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        let lines: Vec<_> = paras.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(lines, ["a b", "c d"]);
        assert_eq!(
            paras[0].formatting,
            [FormattedSpan {
                range: 2..3,
                style: Style::Strong
            }]
        );
        assert_eq!(
            paras[1].formatting,
            [FormattedSpan {
                range: 0..1,
                style: Style::Strong
            }]
        );
    }

    #[test]
    fn code_blocks_stand_alone_without_padding_lines() {
        let blocks = parse_blocks("before\n```rust\nlet x = 1;\n```\nafter", &[], false);
        let texts: Vec<_> = blocks
            .iter()
            .map(|b| match b {
                MarkdownBlock::Inline(p) => format!(
                    "inline:{}",
                    p.iter()
                        .map(|p| p.text.as_str())
                        .collect::<Vec<_>>()
                        .join("|")
                ),
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
        assert_eq!(
            one_paragraph("hello * world"),
            ("hello * world".into(), vec![])
        );
    }
}

/// Bare `http(s)://` addresses in `text` as byte ranges, without trailing
/// sentence punctuation, each passing [`safe_web_url`].
fn bare_urls(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find("http") {
        let start = from + offset;
        let rest = &text[start..];
        let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = rest[..len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
        let boundary = text[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        if boundary && (token.starts_with("https://") || token.starts_with("http://")) {
            if let Some(url) = safe_web_url(token) {
                found.push((start..start + token.len(), url));
            }
        }
        from = start + len.max(4);
    }
    found
}

/// Browser destinations only; reject credentials, ambiguous separators and
/// whitespace before URL parsing can silently normalize them.
pub(crate) fn safe_web_url(value: &str) -> Option<String> {
    if value
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return None;
    }
    let authority = value.split_once("://")?.1.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(url.to_string())
}

#[cfg(test)]
mod link_tests {
    use super::*;
    #[test]
    fn bare_urls_link_without_trailing_punctuation_or_code() {
        let blocks = parse_blocks("see https://example.org/x27, `http://a.b` ok", &[], false);
        let [MarkdownBlock::Inline(paras)] = blocks.as_slice() else { panic!("one inline block") };
        assert_eq!(paras[0].links.len(), 1);
        let (range, url) = &paras[0].links[0];
        assert_eq!(&paras[0].text[range.clone()], "https://example.org/x27");
        assert_eq!(url, "https://example.org/x27");
    }

    #[test]
    fn unicode_link_ranges_and_style_are_preserved() {
        let paras = paragraphs(&parse_inline(
            "Hi [**é猫**](https://example.org/path?q=1) end",
        ));
        assert_eq!(paras[0].text, "Hi é猫 end");
        assert_eq!(
            paras[0].links,
            vec![(3..8, "https://example.org/path?q=1".into())]
        );
        assert!(
            paras[0]
                .formatting
                .iter()
                .any(|span| span.range == (3..8) && span.style == Style::Strong)
        );
    }
    #[test]
    fn unsafe_and_code_links_stay_inert() {
        for value in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "mailto:a@b",
            "https://a:b@example.org",
            "https://example.org\n/x",
            "https:\\example.org",
            "https:///example.org",
        ] {
            assert!(safe_web_url(value).is_none(), "{value}");
        }
        let paras = paragraphs(&parse_inline("`[x](https://example.org)` [bad](file:///x)"));
        assert!(paras[0].links.is_empty());
        assert!(paras[0].text.contains("[bad](file:///x)"));
    }
    #[test]
    fn links_split_at_newlines_and_fences_stay_code() {
        let paras = paragraphs(&parse_inline("[a\nb](http://example.org)"));
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[0].links[0].0, 0..1);
        assert_eq!(paras[1].links[0].0, 0..1);
        assert!(matches!(
            &parse_blocks("```\n[x](https://example.org)\n```", &[], false)[0],
            MarkdownBlock::Code(_)
        ));
    }
}
