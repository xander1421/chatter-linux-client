//! Markdown wrapping for the composer formatting toolbar.
//!
//! Wraps the selected byte range (or inserts empty markers at the cursor)
//! with markdown. Lives in Rust because Slint has no byte-slice op; the
//! composer's `TextFormat.wrap` global calls in here.

use slint::{ComponentHandle, SharedString};

use crate::{MainWindow, TextFormat};

/// Round `i` down to the nearest UTF-8 char boundary at or below it.
fn floor_boundary(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Wrap `text[start..end]` with the markdown for `fmt`. When `start == end`
/// (no selection) the markers are inserted empty at that point.
fn wrap_selection(text: &str, fmt: &str, start: usize, end: usize) -> String {
    let (prefix, suffix) = match fmt {
        "bold" => ("**", "**"),
        "italic" => ("*", "*"),
        "strikethrough" => ("~~", "~~"),
        "code" => ("`", "`"),
        "codeblock" => ("```\n", "\n```"),
        _ => return text.to_string(),
    };

    let s = floor_boundary(text, start.min(end));
    let e = floor_boundary(text, start.max(end));

    let mut out = String::with_capacity(text.len() + prefix.len() + suffix.len());
    out.push_str(&text[..s]);
    out.push_str(prefix);
    out.push_str(&text[s..e]);
    out.push_str(suffix);
    out.push_str(&text[e..]);
    out
}

fn replace_selection(text: &str, replacement: &str, start: usize, end: usize) -> String {
    let s = floor_boundary(text, start.min(end));
    let e = floor_boundary(text, start.max(end));
    format!("{}{}{}", &text[..s], replacement, &text[e..])
}

pub(crate) fn wire_format_text(window: &MainWindow) {
    window
        .global::<TextFormat>()
        .on_replace(|text, replacement, start, end| {
            replace_selection(
                &text,
                &replacement,
                start.max(0) as usize,
                end.max(0) as usize,
            )
            .into()
        });
    window
        .global::<TextFormat>()
        .on_byte_length(|text| text.len() as i32);
    window.global::<TextFormat>().on_wrap(
        |text: SharedString, fmt: SharedString, start: i32, end: i32| {
            wrap_selection(&text, &fmt, start.max(0) as usize, end.max(0) as usize).into()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_insertion_preserves_middle_and_unicode_boundaries() {
        assert_eq!(replace_selection("abXYZcd", "😀", 2, 5), "ab😀cd");
        assert_eq!(replace_selection("abXYZcd", "😀", 5, 2), "ab😀cd");
        assert_eq!(replace_selection("café!", "😀", 4, 4), "caf😀é!");
        assert_eq!(replace_selection("abc", "😀", 999, 999), "abc😀");
        assert_eq!(replace_selection("ab", "preedit", 1, 1), "apreeditb");
    }

    #[test]
    fn wraps_selection_only() {
        // "hello world", select "world" (bytes 6..11)
        assert_eq!(
            wrap_selection("hello world", "bold", 6, 11),
            "hello **world**"
        );
    }

    #[test]
    fn wraps_middle_selection() {
        assert_eq!(
            wrap_selection("abcXYZdef", "strikethrough", 3, 6),
            "abc~~XYZ~~def"
        );
    }

    #[test]
    fn no_selection_inserts_empty_markers_at_cursor() {
        assert_eq!(wrap_selection("abcdef", "bold", 3, 3), "abc****def");
        assert_eq!(wrap_selection("", "italic", 0, 0), "**");
        assert_eq!(wrap_selection("", "codeblock", 0, 0), "```\n\n```");
    }

    #[test]
    fn reversed_offsets_normalized() {
        // anchor after cursor — same result as forward selection
        assert_eq!(
            wrap_selection("hello world", "code", 11, 6),
            "hello `world`"
        );
    }

    #[test]
    fn multibyte_boundary_safe() {
        // "café world", select "world". 'é' is 2 bytes → "world" at bytes 6..11.
        assert_eq!(
            wrap_selection("café world", "bold", 6, 11),
            "café **world**"
        );
    }

    #[test]
    fn unknown_format_unchanged() {
        assert_eq!(wrap_selection("hello", "unknown", 0, 5), "hello");
    }
}
