//! Shared utilities for inbox event handling.

/// Update a reaction summary string when a reaction is added or removed.
///
/// The summary format is "👍 3  ❤️ 2" (emoji space count, double-space separated).
pub fn update_reaction_summary(current: &str, emoji: &str, remove: bool) -> String {
    use std::collections::HashMap;

    // Parse current summary into map of emoji -> count
    let mut reactions: HashMap<String, i32> = HashMap::new();

    for part in current.split("  ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Format is "emoji count" or just "emoji" (count=1)
        let mut chars = part.chars();
        let mut emoji_chars = String::new();
        let mut count_str = String::new();

        // Collect emoji (may be multiple chars for complex emoji)
        for c in chars.by_ref() {
            if c.is_ascii_digit() || c == ' ' {
                if c.is_ascii_digit() {
                    count_str.push(c);
                }
                break;
            }
            emoji_chars.push(c);
        }
        // Collect remaining digits
        for c in chars {
            if c.is_ascii_digit() {
                count_str.push(c);
            }
        }

        let count = count_str.parse::<i32>().unwrap_or(1);
        if !emoji_chars.is_empty() {
            reactions.insert(emoji_chars, count);
        }
    }

    // Update the count
    if remove {
        if let Some(count) = reactions.get_mut(emoji) {
            *count -= 1;
            if *count <= 0 {
                reactions.remove(emoji);
            }
        }
    } else {
        *reactions.entry(emoji.to_string()).or_insert(0) += 1;
    }

    // Rebuild the summary string
    if reactions.is_empty() {
        return String::new();
    }

    reactions
        .iter()
        .map(|(e, c)| {
            if *c == 1 {
                e.clone()
            } else {
                format!("{} {}", e, c)
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Format a nanosecond timestamp for display.
pub fn format_timestamp(timestamp_nanos: u64) -> String {
    use chrono::{Local, TimeZone};

    if timestamp_nanos == 0 {
        return "".to_string();
    }

    let secs = (timestamp_nanos / 1_000_000_000) as i64;
    let nsecs = (timestamp_nanos % 1_000_000_000) as u32;

    match Local.timestamp_opt(secs, nsecs).single() {
        Some(dt) => {
            let now = Local::now();
            if dt.date_naive() == now.date_naive() {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%b %d, %H:%M").to_string()
            }
        }
        None => "".to_string(),
    }
}
