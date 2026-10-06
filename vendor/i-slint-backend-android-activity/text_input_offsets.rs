// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! GameTextInput strings are decoded to UTF-8 by android-activity, but its
//! selection and composing spans remain Java UTF-16 offsets. Keep all cached
//! backend/core spans in UTF-8 and convert only at the GameActivity boundary.

use i_slint_common::unicode_utils::{
    byte_offset_to_utf16_offset, utf16_offset_to_byte_offset_clamped,
};

pub(crate) fn utf16_span_to_utf8(text: &str, span: (usize, usize)) -> (usize, usize) {
    (
        utf16_offset_to_byte_offset_clamped(text, span.0),
        utf16_offset_to_byte_offset_clamped(text, span.1),
    )
}

pub(crate) fn utf8_span_to_utf16(text: &str, span: (usize, usize)) -> (usize, usize) {
    (
        byte_offset_to_utf16_offset(text, span.0),
        byte_offset_to_utf16_offset(text, span.1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyrillic_native_typing_and_backspace_keep_the_caret_at_the_end() {
        // Actual native reports for three typed Cyrillic letters, then three
        // deletes. Treating the first UTF-16 caret (1) as a byte offset put it
        // inside а; passing the resulting byte caret (2) back unchanged then
        // gave Java an offset beyond its one-character string.
        for (text, java_cursor, byte_cursor) in [
            ("а", 1, 2),
            ("ап", 2, 4),
            ("апр", 3, 6),
            ("ап", 2, 4),
            ("а", 1, 2),
            ("", 0, 0),
        ] {
            assert_eq!(
                utf16_span_to_utf8(text, (java_cursor, java_cursor)),
                (byte_cursor, byte_cursor)
            );
            assert_eq!(
                utf8_span_to_utf16(text, (byte_cursor, byte_cursor)),
                (java_cursor, java_cursor)
            );
        }
    }

    #[test]
    fn mixed_unicode_selection_direction_and_mid_text_composition_are_preserved() {
        let text = "а😀e\u{301}中z";
        // UTF-16 boundaries: 0,1,3,4,5,6,7; UTF-8: 0,2,6,7,9,12,13.
        assert_eq!(utf16_span_to_utf8(text, (6, 1)), (12, 2));
        assert_eq!(utf8_span_to_utf16(text, (12, 2)), (6, 1));
        assert_eq!(utf16_span_to_utf8(text, (1, 5)), (2, 9));
        assert_eq!(utf8_span_to_utf16(text, (2, 9)), (1, 5));
        assert_eq!(&text[2..9], "😀e\u{301}");
    }

    #[test]
    fn incoming_surrogate_interiors_and_out_of_range_offsets_are_clamped() {
        assert_eq!(utf16_span_to_utf8("а😀z", (2, usize::MAX)), (6, 7));
        assert_eq!(utf16_span_to_utf8("", (1, usize::MAX)), (0, 0));
        assert_eq!(utf16_span_to_utf8("abc", (3, 1)), (3, 1));
    }
}
