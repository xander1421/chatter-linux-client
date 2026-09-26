//! Chat data structures for Slint UI bindings.
//!
//! Provides Rust structs that map to Slint struct definitions for
//! chat components like message items and headers.

use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{Message, MessageContent, ReactionCount};
use crate::domain::user::PresenceStatus;
use chrono::{DateTime, Datelike, Duration, Local, Utc};

// ============================================================================
// Chat Header Data
// ============================================================================

/// Chat header data for Slint model binding.
///
/// Maps to the properties of `ChatHeader` component in Slint.
#[derive(Clone, Debug, Default)]
pub struct ChatHeaderData {
    /// Channel ID.
    pub channel_id: String,
    /// Channel name.
    pub channel_name: String,
    /// Channel type: "public", "private", or "dm".
    pub channel_type: String,
    /// Number of members in the channel.
    pub member_count: u32,
    /// Channel topic/description.
    pub topic: String,
    /// Whether the channel is archived.
    pub is_archived: bool,
    /// Typing indicator text (e.g., "Alice is typing..." or empty).
    pub typing_indicator: String,
}

impl ChatHeaderData {
    /// Create a new chat header data entry.
    #[must_use]
    pub fn new(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            channel_type: "public".to_string(),
            ..Default::default()
        }
    }

    /// Set channel name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.channel_name = name.into();
        self
    }

    /// Set channel type.
    #[must_use]
    pub fn with_type(mut self, channel_type: impl Into<String>) -> Self {
        self.channel_type = channel_type.into();
        self
    }

    /// Set member count.
    #[must_use]
    pub fn with_member_count(mut self, count: u32) -> Self {
        self.member_count = count;
        self
    }

    /// Set channel topic.
    #[must_use]
    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = topic.into();
        self
    }

    /// Set archived status.
    #[must_use]
    pub fn with_archived(mut self, is_archived: bool) -> Self {
        self.is_archived = is_archived;
        self
    }

    /// Set typing indicator text.
    #[must_use]
    pub fn with_typing_indicator(mut self, indicator: impl Into<String>) -> Self {
        self.typing_indicator = indicator.into();
        self
    }
}

impl From<&Channel> for ChatHeaderData {
    fn from(channel: &Channel) -> Self {
        let channel_type_str = match channel.channel_type {
            ChannelType::Public => "public",
            ChannelType::Private => "private",
            ChannelType::DirectMessage => "dm",
            ChannelType::SavedMessages => "saved",
        };

        Self {
            channel_id: channel.id.to_string(),
            channel_name: channel.name.clone(),
            channel_type: channel_type_str.to_string(),
            member_count: 0, // Must be populated externally
            topic: channel.description.clone().unwrap_or_default(),
            is_archived: channel.archived,
            typing_indicator: String::new(),
        }
    }
}

// ============================================================================
// Pinned Bar Data
// ============================================================================

/// Pinned bar data for Slint model binding.
///
/// Maps to the properties of `PinnedBar` component in Slint.
/// Provides Telegram-style navigation through pinned messages.
#[derive(Clone, Debug, Default)]
pub struct PinnedBarData {
    /// Truncated content of the current pinned message.
    pub message_preview: String,
    /// Current position (1-based for display: "Pinned message 2 of 5").
    pub current_index: i32,
    /// Total number of pinned messages.
    pub total_count: i32,
    /// Whether there are multiple pinned messages (show navigation controls).
    pub has_multiple: bool,
}

impl PinnedBarData {
    /// Create a new pinned bar data entry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the message preview text.
    #[must_use]
    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.message_preview = preview.into();
        self
    }

    /// Set the current position (1-based index and total count).
    #[must_use]
    pub fn with_position(mut self, current: i32, total: i32) -> Self {
        self.current_index = current;
        self.total_count = total;
        self.has_multiple = total >= 2;
        self
    }
}

// ============================================================================
// Thread Panel Data
// ============================================================================

/// Thread panel data for Slint model binding.
///
/// Maps to the properties of `ThreadPanel` component in Slint.
/// Provides display data for thread replies to a root message.
#[derive(Clone, Debug, Default)]
pub struct ThreadPanelData {
    /// Root message ID.
    pub root_message_id: String,
    /// Sender name of the root message.
    pub root_sender_name: String,
    /// Truncated content preview of the root message.
    pub root_content_preview: String,
    /// Number of replies in the thread.
    pub reply_count: i32,
    /// Channel name where the thread exists.
    pub channel_name: String,
}

impl ThreadPanelData {
    /// Create a new thread panel data entry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the root message information.
    #[must_use]
    pub fn with_root_message(
        mut self,
        message_id: impl Into<String>,
        sender_name: impl Into<String>,
        content_preview: impl Into<String>,
    ) -> Self {
        self.root_message_id = message_id.into();
        self.root_sender_name = sender_name.into();
        self.root_content_preview = content_preview.into();
        self
    }

    /// Set the reply count.
    #[must_use]
    pub fn with_reply_count(mut self, count: i32) -> Self {
        self.reply_count = count;
        self
    }

    /// Set the channel name.
    #[must_use]
    pub fn with_channel(mut self, channel_name: impl Into<String>) -> Self {
        self.channel_name = channel_name.into();
        self
    }
}

// ============================================================================
// Message Item Data
// ============================================================================

/// Message item data for Slint model binding.
///
/// Maps to the properties of `MessageItem` component in Slint.
#[derive(Clone, Debug, Default)]
pub struct MessageItemData {
    /// Message ID for identification.
    pub message_id: String,
    /// Sender user ID.
    pub sender_id: String,
    /// Sender display name.
    pub sender_name: String,
    /// Sender initials for avatar fallback.
    pub sender_initials: String,
    /// Plain text content to display.
    pub content: String,
    /// Formatted timestamp (e.g., "2:34 PM", "Yesterday").
    pub timestamp: String,
    /// Raw nanosecond timestamp as string for event matching (reactions, edits).
    pub timestamp_ns: String,
    /// Whether this is the current user's message.
    pub is_own: bool,
    /// Whether the message has been edited.
    pub is_edited: bool,
    /// Whether the message has been deleted.
    pub is_deleted: bool,
    /// Whether this is a system message (join/leave/etc).
    pub is_system: bool,
    /// Whether this is an AI-generated message.
    pub is_ai: bool,
    /// Whether the message has attachments.
    pub has_attachments: bool,
    /// Number of attachments.
    pub attachment_count: u32,
    /// Whether the message has reactions.
    pub has_reactions: bool,
    /// Reaction summary string (e.g., "👍 3  ❤️ 2").
    pub reaction_summary: String,
    /// Thread reply count (0 if not a thread root).
    pub reply_count: u32,
    /// Whether this is a reply in a thread.
    pub is_thread_reply: bool,
    /// Whether to show sender info (false for consecutive same-sender).
    pub show_sender: bool,
    /// Delivery status for own messages: "sent", "delivered", "read", or "" for non-own.
    pub delivery_status: String,
}

impl MessageItemData {
    /// Create a new message item data entry.
    #[must_use]
    pub fn new(message_id: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            show_sender: true,
            ..Default::default()
        }
    }

    /// Set sender information.
    #[must_use]
    pub fn with_sender(
        mut self,
        sender_id: impl Into<String>,
        sender_name: impl Into<String>,
        sender_initials: impl Into<String>,
    ) -> Self {
        self.sender_id = sender_id.into();
        self.sender_name = sender_name.into();
        self.sender_initials = sender_initials.into();
        self
    }

    /// Set message content.
    #[must_use]
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }

    /// Set formatted timestamp.
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: impl Into<String>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    /// Set raw nanosecond timestamp for event matching.
    #[must_use]
    pub fn with_timestamp_ns(mut self, timestamp_ns: u64) -> Self {
        self.timestamp_ns = timestamp_ns.to_string();
        self
    }

    /// Set whether this is the current user's message.
    #[must_use]
    pub fn with_own(mut self, is_own: bool) -> Self {
        self.is_own = is_own;
        self
    }

    /// Set edited status.
    #[must_use]
    pub fn with_edited(mut self, is_edited: bool) -> Self {
        self.is_edited = is_edited;
        self
    }

    /// Mark as deleted.
    #[must_use]
    pub fn with_deleted(mut self, is_deleted: bool) -> Self {
        self.is_deleted = is_deleted;
        self
    }

    /// Mark as system message.
    #[must_use]
    pub fn with_system(mut self, is_system: bool) -> Self {
        self.is_system = is_system;
        self
    }

    /// Mark as AI message.
    #[must_use]
    pub fn with_ai(mut self, is_ai: bool) -> Self {
        self.is_ai = is_ai;
        self
    }

    /// Set attachment information.
    #[must_use]
    pub fn with_attachments(mut self, count: u32) -> Self {
        self.has_attachments = count > 0;
        self.attachment_count = count;
        self
    }

    /// Set reaction information.
    #[must_use]
    pub fn with_reactions(mut self, summary: impl Into<String>) -> Self {
        let summary = summary.into();
        self.has_reactions = !summary.is_empty();
        self.reaction_summary = summary;
        self
    }

    /// Set thread information.
    #[must_use]
    pub fn with_thread(mut self, reply_count: u32, is_thread_reply: bool) -> Self {
        self.reply_count = reply_count;
        self.is_thread_reply = is_thread_reply;
        self
    }

    /// Set whether to show sender info.
    #[must_use]
    pub fn with_show_sender(mut self, show: bool) -> Self {
        self.show_sender = show;
        self
    }

    /// Set delivery status for own messages.
    #[must_use]
    pub fn with_delivery_status(mut self, status: impl Into<String>) -> Self {
        self.delivery_status = status.into();
        self
    }
}

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

/// The date chip between messages from different days: "Today",
/// "Yesterday", a weekday within the last week, then "Sep 25" this year and
/// "Sep 25, 2025" before it. Each label names exactly one local day in any
/// run of messages, so the list draws a chip wherever the label changes.
#[must_use]
pub fn day_label(datetime: DateTime<Utc>) -> String {
    day_label_at(datetime.with_timezone(&Local).date_naive(), Local::now().date_naive())
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

/// Format reaction counts to a summary string.
///
/// Returns a string like "👍 3  ❤️ 2  😂 1"
#[must_use]
pub fn reaction_summary_string(reactions: &[ReactionCount]) -> String {
    if reactions.is_empty() {
        return String::new();
    }

    reactions
        .iter()
        .map(|r| format!("{} {}", r.emoji, r.count))
        .collect::<Vec<_>>()
        .join("  ")
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
        MessageContent::Call(call) => {
            match call.call_type {
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
            }
        }
        MessageContent::Encrypted(_) => "[Encrypted message]".to_string(),
        MessageContent::SealedSender(_) => "[Encrypted message]".to_string(),
    }
}

/// Convert a domain Message to MessageItemData.
///
/// # Arguments
/// * `message` - The message to convert
/// * `current_user_id` - The current user's ID (for is_own flag)
/// * `sender_name` - The sender's display name (must be provided externally)
/// * `sender_initials` - The sender's initials (must be provided externally)
/// * `reply_count` - Number of thread replies (must be provided externally)
/// * `show_sender` - Whether to show sender info
#[must_use]
pub fn message_to_item_data(
    message: &Message,
    current_user_id: Option<&str>,
    sender_name: &str,
    sender_initials: &str,
    reply_count: u32,
    show_sender: bool,
) -> MessageItemData {
    let sender_id = message
        .sender_id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_default();

    let is_own = current_user_id
        .map(|uid| message.sender_id.as_ref().is_some_and(|sid| sid.to_string() == uid))
        .unwrap_or(false);

    let is_system = message.content.is_system();
    let is_ai = matches!(message.content, MessageContent::AI(_));

    MessageItemData::new(message.id.to_string())
        .with_sender(sender_id, sender_name, sender_initials)
        .with_content(extract_content_text(&message.content))
        .with_timestamp(format_timestamp(message.created_at))
        .with_timestamp_ns(message.sent_at)
        .with_own(is_own)
        .with_edited(message.is_edited)
        .with_deleted(message.is_deleted)
        .with_system(is_system)
        .with_ai(is_ai)
        .with_attachments(message.attachments.len() as u32)
        .with_reactions(reaction_summary_string(&message.reaction_counts))
        .with_thread(reply_count, message.is_thread_reply())
        .with_show_sender(show_sender)
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
    use crate::domain::ids::{ChannelId, UserId};

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
    use crate::domain::message::{ConversationType, Message, MessageContent, ReactionCount};
    use crate::domain::SystemMessage;

    #[test]
    fn test_message_item_data_new() {
        let item = MessageItemData::new("msg-123")
            .with_sender("user-1", "Alice", "AL")
            .with_content("Hello, world!")
            .with_timestamp("2:34 PM")
            .with_own(true);

        assert_eq!(item.message_id, "msg-123");
        assert_eq!(item.sender_id, "user-1");
        assert_eq!(item.sender_name, "Alice");
        assert_eq!(item.content, "Hello, world!");
        assert!(item.is_own);
        assert!(item.show_sender);
    }

    #[test]
    fn test_message_item_with_reactions() {
        let item = MessageItemData::new("msg-456")
            .with_reactions("👍 3  ❤️ 2");

        assert!(item.has_reactions);
        assert_eq!(item.reaction_summary, "👍 3  ❤️ 2");
    }

    #[test]
    fn test_message_item_with_thread() {
        let item = MessageItemData::new("msg-789")
            .with_thread(5, false);

        assert_eq!(item.reply_count, 5);
        assert!(!item.is_thread_reply);
    }

    #[test]
    fn test_reaction_summary_string() {
        let reactions = vec![
            ReactionCount::new("👍".to_string(), 3),
            ReactionCount::new("❤️".to_string(), 2),
        ];

        let summary = reaction_summary_string(&reactions);
        assert_eq!(summary, "👍 3  ❤️ 2");
    }

    #[test]
    fn test_reaction_summary_string_empty() {
        let reactions: Vec<ReactionCount> = vec![];
        let summary = reaction_summary_string(&reactions);
        assert!(summary.is_empty());
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

    #[test]
    fn test_message_to_item_data() {
        let channel_id = ChannelId::new();
        let sender_id = UserId::new();
        let msg = Message::new_text(channel_id, ConversationType::Channel, Some(sender_id), "Test message".to_string(), 0);

        let item = message_to_item_data(
            &msg,
            Some(&sender_id.to_string()),
            "Bob",
            "B",
            0,
            true,
        );

        assert_eq!(item.content, "Test message");
        assert_eq!(item.sender_name, "Bob");
        assert!(item.is_own);
        assert!(!item.is_system);
        assert!(!item.is_edited);
    }

    #[test]
    fn test_chat_header_data_new() {
        let header = ChatHeaderData::new("channel-123")
            .with_name("general")
            .with_type("public")
            .with_member_count(42)
            .with_topic("General discussion");

        assert_eq!(header.channel_id, "channel-123");
        assert_eq!(header.channel_name, "general");
        assert_eq!(header.channel_type, "public");
        assert_eq!(header.member_count, 42);
        assert_eq!(header.topic, "General discussion");
        assert!(!header.is_archived);
        assert!(header.typing_indicator.is_empty());
    }

    #[test]
    fn test_chat_header_data_from_channel() {
        let owner_id = UserId::new();
        let channel = Channel::new(
            ChannelId::new(),
            "dev-chat".to_string(),
            ChannelType::Private,
            owner_id,
        );

        let header = ChatHeaderData::from(&channel);

        assert_eq!(header.channel_name, "dev-chat");
        assert_eq!(header.channel_type, "private");
        assert!(!header.is_archived);
    }

    #[test]
    fn test_chat_header_data_with_typing() {
        let header = ChatHeaderData::new("channel-456")
            .with_typing_indicator("Alice is typing...");

        assert_eq!(header.typing_indicator, "Alice is typing...");
    }
}
