//! Chat display helpers for Slint UI bindings.
//!
//! Timestamps, day chips, last-seen lines, message text, and initials.

use crate::domain::message::MessageContent;
use crate::domain::user::PresenceStatus;
use chrono::{DateTime, Datelike, Duration, Local, Utc};

// ============================================================================
// Helper Functions
// ============================================================================

/// Format a timestamp to a relative string for display.
///
/// Returns:
/// - "Just now" for < 1 minute ago
/// - "N min ago" for < 1 hour ago
/// - "HH:MM AM/PM" for today
/// - "Yesterday HH:MM AM/PM" for yesterday
/// - "Mon HH:MM AM/PM" for this week
/// - "Jan 1" for this year
/// - "Jan 1, 2024" for older
#[must_use]
pub fn format_timestamp(datetime: DateTime<Utc>) -> String {
    let local = datetime.with_timezone(&Local);
    let now = Local::now();
    let diff = now.signed_duration_since(local);

    if diff < Duration::minutes(1) {
        return "Just now".to_string();
    }

    if diff < Duration::hours(1) {
        let mins = diff.num_minutes();
        return format!("{} min ago", mins);
    }

    let today = now.date_naive();
    let msg_date = local.date_naive();

    if msg_date == today {
        return local.format("%-I:%M %p").to_string();
    }

    let yesterday = today - chrono::Duration::days(1);
    if msg_date == yesterday {
        return format!("Yesterday {}", local.format("%-I:%M %p"));
    }

    // Within the last week
    if diff < Duration::days(7) {
        return local.format("%a %-I:%M %p").to_string();
    }

    // This year
    if local.year() == now.year() {
        return local.format("%b %-d").to_string();
    }

    // Older
    local.format("%b %-d, %Y").to_string()
}

/// A sidebar row's right-hand time, in local time: "14:03" today,
/// "Sep 24, 14:03" before. The live update and the rebuild both go through
/// here — the live path used the sender's clock and a different format, so a
/// row's time changed shape on the next rebuild. The Calls tab uses it too:
/// "Yesterday 10:07 PM" left no room for the outcome on a phone row.
pub fn sidebar_time_at(
    at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    // Signal's chat-list rule: a time only for today; older rows get the
    // day, which is what the eye scans for there.
    use chrono::Datelike;
    let local = at.with_timezone(&chrono::Local);
    let days = (now.date_naive() - local.date_naive()).num_days();
    match days {
        ..=0 => local.format("%H:%M").to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => local.format("%a").to_string(),
        _ if local.year() == now.year() => local.format("%b %-d").to_string(),
        _ => local.format("%b %-d, %Y").to_string(),
    }
}

/// The date chip between messages from different days: "Today",
/// "Yesterday", a weekday within the last week, then "Sep 25" this year and
/// "Sep 25, 2025" before it. Each label names exactly one local day in any
/// run of messages, so the list draws a chip wherever the label changes.
#[must_use]
pub fn day_label(datetime: DateTime<Utc>) -> String {
    day_label_at(
        datetime.with_timezone(&Local).date_naive(),
        Local::now().date_naive(),
    )
}

fn day_label_at(day: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    let days_ago = (today - day).num_days();
    match days_ago {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => day.format("%A").to_string(),
        _ if day.year() == today.year() => day.format("%b %-d").to_string(),
        _ => day.format("%b %-d, %Y").to_string(),
    }
}

/// The line under a contact's name on the profile screen.
///
/// A live presence outranks any stamp — someone who is online now is not
/// "last seen 3 minutes ago". Offline falls back to the stored
/// `profiles.last_seen_at`, and to "last seen recently" when we have never
/// received a presence timestamp for them, which is the honest answer: we
/// know they exist, not when they were here.
#[must_use]
pub fn last_seen_label(presence: PresenceStatus, last_seen_at: Option<i64>) -> String {
    match presence {
        PresenceStatus::Online => "online".to_string(),
        PresenceStatus::Away => "away".to_string(),
        PresenceStatus::DoNotDisturb => "do not disturb".to_string(),
        PresenceStatus::Offline => match last_seen_at {
            Some(secs) => match DateTime::<Utc>::from_timestamp(secs, 0) {
                Some(dt) => format!("last seen {}", format_timestamp(dt).to_lowercase()),
                None => "last seen recently".to_string(),
            },
            None => "last seen recently".to_string(),
        },
    }
}

/// Extract display content from a message.
///
/// Returns the text to display for a message, handling different content types.
#[must_use]
pub fn extract_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Mixed(mixed) => mixed.text.clone().unwrap_or_default(),
        MessageContent::AI(ai) => ai.text.clone(),
        MessageContent::System(sys) => sys.description(),
        MessageContent::Call(call) => match call.call_type {
            crate::domain::message::CallMessageType::Started => {
                format!("{} started a call", call.initiator_name)
            }
            crate::domain::message::CallMessageType::Ended => {
                let duration = call.duration_display().unwrap_or_default();
                if duration.is_empty() {
                    "Call ended".to_string()
                } else {
                    format!("Call ended ({duration})")
                }
            }
            crate::domain::message::CallMessageType::Missed => {
                format!("Missed call from {}", call.initiator_name)
            }
            crate::domain::message::CallMessageType::Declined => {
                format!("Declined call from {}", call.initiator_name)
            }
        },
        MessageContent::Encrypted(_) => "[Encrypted message]".to_string(),
        MessageContent::SealedSender(_) => "[Encrypted message]".to_string(),
    }
}

/// View-ready call-participant data resolved in core from local state. The UI
/// decodes `avatar_bytes` to an image and renders; no resolution logic in the UI.
#[derive(Clone, Debug, Default)]
pub struct CallParticipantInfo {
    pub display_name: String,
    pub initials: String,
    pub avatar_bytes: Option<Vec<u8>>,
}

/// Generate initials from a display name.
///
/// Takes up to 2 characters from the first two words.
#[must_use]
pub fn initials_from_name(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .collect::<String>()
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SystemMessage;

    #[test]
    fn day_label_names_one_day() {
        let d = |y, m, dd| chrono::NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        let today = d(2026, 9, 26);
        assert_eq!(day_label_at(d(2026, 9, 26), today), "Today");
        assert_eq!(day_label_at(d(2026, 9, 25), today), "Yesterday");
        assert_eq!(day_label_at(d(2026, 9, 21), today), "Monday");
        // A week back is a date, never a weekday that repeats.
        assert_eq!(day_label_at(d(2026, 9, 19), today), "Sep 19");
        assert_eq!(day_label_at(d(2025, 12, 31), today), "Dec 31, 2025");
    }

    #[test]
    fn test_extract_content_text() {
        // Text content
        let text = MessageContent::Text("Hello".to_string());
        assert_eq!(extract_content_text(&text), "Hello");

        // System message
        let system = MessageContent::System(SystemMessage::ChannelCreated);
        assert_eq!(extract_content_text(&system), "created the channel");

        // Encrypted
        let encrypted = MessageContent::Encrypted(crate::domain::message::EncryptedContent::new(
            vec![1, 2, 3],
            crate::domain::message::EncryptedMessageType::Signal,
        ));
        assert_eq!(extract_content_text(&encrypted), "[Encrypted message]");
    }

    #[test]
    fn test_initials_from_name() {
        assert_eq!(initials_from_name("Alice"), "A");
        assert_eq!(initials_from_name("Alice Smith"), "AS");
        assert_eq!(initials_from_name("Bob Charles Jones"), "BC");
        assert_eq!(initials_from_name(""), "");
    }
}
