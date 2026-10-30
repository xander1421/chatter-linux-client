//! Emoji-only message bodies ("jumbomoji").

use unicode_segmentation::UnicodeSegmentation;

/// How many emoji a body is made of when it is nothing but one to three of
/// them (whitespace aside), else 0. Signal and Telegram draw such a message
/// large; anything with a letter, digit or punctuation stays normal text.
pub fn jumbo_count(text: &str) -> usize {
    let mut count = 0;
    for grapheme in text.graphemes(true) {
        if grapheme.chars().all(char::is_whitespace) {
            continue;
        }
        if !is_emoji_cluster(grapheme) {
            return 0;
        }
        count += 1;
        if count > 3 {
            return 0;
        }
    }
    count
}

fn is_emoji_cluster(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else { return false };
    // Keycaps: 0-9, # or * + (VS16) + U+20E3.
    if matches!(first, '0'..='9' | '#' | '*') {
        return cluster.contains('\u{20E3}');
    }
    matches!(first as u32,
        0x1F000..=0x1FAFF   // pictographs, emoticons, transport, flags, symbols
        | 0x2600..=0x27BF   // misc symbols, dingbats
        | 0x2300..=0x23FF   // ⌚ ⏰ …
        | 0x2B00..=0x2BFF   // ⭐ ⬆ …
        | 0x2190..=0x21FF   // arrows (emoji with VS16)
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x00A9 | 0x00AE | 0x2122 | 0x2139 | 0x203C | 0x2049)
}

#[cfg(test)]
mod tests {
    use super::jumbo_count;

    #[test]
    fn counts_only_short_emoji_bodies() {
        assert_eq!(jumbo_count("😂"), 1);
        assert_eq!(jumbo_count(" 👍🏽 ❤️ "), 2);
        assert_eq!(jumbo_count("👨‍👩‍👧"), 1);
        assert_eq!(jumbo_count("🇩🇪🎉3️⃣"), 3);
        assert_eq!(jumbo_count("😂😂😂😂"), 0);
        assert_eq!(jumbo_count("ok 😂"), 0);
        assert_eq!(jumbo_count("1"), 0);
        assert_eq!(jumbo_count(""), 0);
    }
}
