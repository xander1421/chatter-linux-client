//! Text processing for the composer.

use slint::SharedString;

use crate::MainWindow;

/// Maximum characters in a word before inserting a break point.
const MAX_WORD_LENGTH: usize = 50;

/// Insert zero-width spaces into long sequences without whitespace.
/// This allows Slint's word-wrap to break long strings.
fn insert_word_breaks(text: &str) -> String {
    const ZWSP: char = '\u{200B}';

    let mut result = String::with_capacity(text.len() + text.len() / MAX_WORD_LENGTH);
    let mut word_len = 0;

    for ch in text.chars() {
        if ch.is_whitespace() || ch == ZWSP {
            word_len = 0;
            result.push(ch);
        } else {
            word_len += 1;
            result.push(ch);
            if word_len >= MAX_WORD_LENGTH {
                result.push(ZWSP);
                word_len = 0;
            }
        }
    }

    result
}

/// Check if text needs word break processing.
/// Returns true if there's a sequence of non-whitespace chars longer than MAX_WORD_LENGTH
/// without a zero-width space.
fn needs_word_breaks(text: &str) -> bool {
    const ZWSP: char = '\u{200B}';
    let mut word_len = 0;

    for ch in text.chars() {
        if ch.is_whitespace() || ch == ZWSP {
            word_len = 0;
        } else {
            word_len += 1;
            if word_len > MAX_WORD_LENGTH {
                return true;
            }
        }
    }
    false
}

pub(crate) fn wire_process_text(window: &MainWindow) {
    window.on_process_text(|text: SharedString| -> SharedString {
        let text_str = text.as_str();
        if needs_word_breaks(text_str) {
            insert_word_breaks(text_str).into()
        } else {
            text
        }
    });
}
