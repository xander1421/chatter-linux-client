//! Plaintext pin summaries; raw message bodies and media captions stay intact.

/// Convert actual message text before bounding it, so truncation cannot cut
/// markdown delimiters in half. Pin summaries occupy a single readable line.
pub(crate) fn text_preview(text: &str) -> String {
    let plain = super::markdown::strip_markdown(text);
    let line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    chatter_core::services::gateway::shared::truncate_preview(&line)
}

/// Only actual text is markdown. Attachment filenames/captions are literal.
pub(crate) fn preview_or_caption(text: Option<&str>, caption: &str) -> String {
    text.filter(|text| !text.trim().is_empty())
        .map(text_preview)
        .unwrap_or_else(|| caption.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatted_preview_preserves_original_body() {
        let body = "**bold** *italic* ~~strike~~ `code`".to_owned();
        assert_eq!(text_preview(&body), "bold italic strike code");
        assert_eq!(body, "**bold** *italic* ~~strike~~ `code`");
        let blocks = super::super::markdown::parse_blocks(&body);
        let [super::super::markdown::MarkdownBlock::Inline(paragraphs)] = blocks.as_slice() else {
            panic!("Original must retain its formatted inline paragraph");
        };
        assert_eq!(paragraphs[0].formatting.len(), 4);
    }

    #[test]
    fn plain_multiline_and_code_previews_are_readable() {
        assert_eq!(text_preview("plain text 🙂"), "plain text 🙂");
        assert_eq!(
            text_preview(" **first**\r\n\n second\tline "),
            "first second line"
        );
        assert_eq!(
            text_preview("```rust\nlet s = \"**literal**\";\nnext();\n```"),
            "let s = \"**literal**\"; next();"
        );
        assert_eq!(text_preview("`**literal**`"), "**literal**");
    }

    #[test]
    fn strip_precedes_unicode_safe_bound() {
        let body = format!("**{}**", "猫".repeat(120));
        let preview = text_preview(&body);
        assert_eq!(preview, format!("{}...", "猫".repeat(97)));
        assert_eq!(preview.chars().count(), 100);
        // The closing delimiter is beyond the old core preview boundary.
        assert_eq!(
            text_preview(&format!("**{}**", "a".repeat(90))),
            "a".repeat(90)
        );
    }

    #[test]
    fn attachment_captions_and_filenames_are_literal() {
        for caption in [
            "📷 Photo",
            "🎬 Video",
            "📎 3 attachments",
            "📄 **draft** *final*.txt",
        ] {
            assert_eq!(preview_or_caption(None, caption), caption);
            assert_eq!(preview_or_caption(Some(""), caption), caption);
            assert_eq!(preview_or_caption(Some(" \n\t"), caption), caption);
        }
        assert_eq!(
            preview_or_caption(Some("**caption**"), "📷 Photo"),
            "caption"
        );
    }
}
