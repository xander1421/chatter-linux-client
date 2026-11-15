//! Disappearing-message values shared by domain, storage and services: the
//! secret-chat default timer, the timer-notice presentation filter and the
//! timer's human label. Timer state and its policy live in
//! `services::disappearing`.

use crate::domain::message::{Message, MessageContent, SystemMessage};

pub const SECRET_CHAT_DEFAULT_SECONDS: u32 = 86_400;

/// Typed presentation filter for internal timer change notices. Keep storage
/// intact: these rows still drive receive-side timer notifications. Secret
/// chats (anonymous DMs and private groups) show their notices as rows.
#[must_use]
pub fn is_timer_notice(message: &Message) -> bool {
    !message.conversation_type.is_secret_chat()
        && matches!(
            message.content,
            MessageContent::System(SystemMessage::DisappearingTimerChanged { .. })
        )
}

/// Human label for a timer value: "Off", "30 seconds", "1 hour", "4 weeks"…
/// Any value (a peer may send one that is not a preset) is named by the
/// largest unit that divides it exactly.
#[must_use]
pub fn timer_label(seconds: u32) -> String {
    if seconds == 0 {
        return chatter_i18n::tr("Off");
    }
    const UNITS: [(u32, &str); 5] = [
        (604_800, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
        (1, "second"),
    ];
    let (size, name) = UNITS
        .iter()
        .copied()
        .find(|(size, _)| seconds % size == 0)
        .unwrap_or((1, "second"));
    let n = u64::from(seconds / size);
    match name {
        "week" => chatter_i18n::trn("{n} week", "{n} weeks", n, &[]),
        "day" => chatter_i18n::trn("{n} day", "{n} days", n, &[]),
        "hour" => chatter_i18n::trn("{n} hour", "{n} hours", n, &[]),
        "minute" => chatter_i18n::trn("{n} minute", "{n} minutes", n, &[]),
        _ => chatter_i18n::trn("{n} second", "{n} seconds", n, &[]),
    }
}
