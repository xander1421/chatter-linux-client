//! Format-text callback: wrap composer text with markdown markers.

use slint::{ComponentHandle, SharedString};

use crate::MainWindow;

/// Apply markdown formatting to the composer text.
fn apply_format(text: &str, format_type: &str) -> String {
    if text.is_empty() {
        // Insert empty markers so the user can type inside them.
        return match format_type {
            "bold" => "****".to_string(),
            "italic" => "**".to_string(),
            "strikethrough" => "~~~~".to_string(),
            "code" => "``".to_string(),
            "codeblock" => "```\n\n```".to_string(),
            _ => text.to_string(),
        };
    }

    match format_type {
        "bold" => format!("**{text}**"),
        "italic" => format!("*{text}*"),
        "strikethrough" => format!("~~{text}~~"),
        "code" => format!("`{text}`"),
        "codeblock" => format!("```\n{text}\n```"),
        _ => text.to_string(),
    }
}

pub(crate) fn wire_format_text(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_format_text(move |format_type: SharedString| {
        let weak = weak_window.clone();
        let fmt = format_type.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let current_text = w.get_composer_text().to_string();
                let formatted = apply_format(&current_text, &fmt);
                w.set_composer_text(formatted.into());
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_wraps_text() {
        assert_eq!(apply_format("hello", "bold"), "**hello**");
    }

    #[test]
    fn italic_wraps_text() {
        assert_eq!(apply_format("hello", "italic"), "*hello*");
    }

    #[test]
    fn strikethrough_wraps_text() {
        assert_eq!(apply_format("hello", "strikethrough"), "~~hello~~");
    }

    #[test]
    fn code_wraps_text() {
        assert_eq!(apply_format("hello", "code"), "`hello`");
    }

    #[test]
    fn codeblock_wraps_text() {
        assert_eq!(apply_format("hello", "codeblock"), "```\nhello\n```");
    }

    #[test]
    fn empty_text_inserts_markers() {
        assert_eq!(apply_format("", "bold"), "****");
        assert_eq!(apply_format("", "italic"), "**");
        assert_eq!(apply_format("", "code"), "``");
    }

    #[test]
    fn unknown_format_returns_text() {
        assert_eq!(apply_format("hello", "unknown"), "hello");
    }
}
