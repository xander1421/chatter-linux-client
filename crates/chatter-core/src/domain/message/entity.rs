use super::{CallMessageType, ConversationType, MessageContent, ReactionCount};
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Message
// ============================================================================

/// Represents a chat message in the unified message pipeline.
///
/// All message types (channel, DM, anon DM, anon group) use this struct.
/// The `conversation_type` field discriminates what kind of conversation
/// the message belongs to, and `conversation_id` holds the conversation UUID
/// (a ChannelId for channels, or a converted AnonDmId/AnonGroupId for
/// anonymous conversations — they share the same UUID representation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    /// The conversation this message belongs to.
    /// For channels: the channel UUID. For anon DM/group: the AnonDmId/AnonGroupId
    /// converted to ChannelId (they share the same UUID representation).
    pub conversation_id: ChannelId,
    /// What type of conversation this message is in.
    pub conversation_type: ConversationType,
    /// None for sealed sender messages (server doesn't know sender).
    pub sender_id: Option<UserId>,
    /// Signal device ID of the sender (for multi-device).
    pub device_id: Option<u32>,
    pub content: MessageContent,
    /// Raw encrypted content blob (separate from content_json for efficient storage).
    pub encrypted_content: Option<Vec<u8>>,
    /// Parent message ID for threads.
    pub thread_id: Option<MessageId>,
    /// Message being replied to (quote).
    pub reply_to: Option<MessageId>,
    /// Quote context: author of the quoted message.
    #[serde(default)]
    pub reply_to_author_id: Option<UserId>,
    /// Quote context: preview text of the quoted message.
    #[serde(default)]
    pub reply_to_text: Option<String>,
    /// Quote context: JPEG/PNG thumbnail bytes of the first image/video
    /// attachment in the quoted message. Carried over the wire inside
    /// `Quote.attachments[0].thumbnail` (Signal-compatible).
    #[serde(default)]
    pub reply_to_thumbnail: Option<Vec<u8>>,
    /// Quote context: MIME type of the quoted attachment, e.g. "image/jpeg".
    #[serde(default)]
    pub reply_to_thumbnail_mime: Option<String>,
    pub attachments: Vec<AttachmentId>,
    pub is_pinned: bool,
    pub is_edited: bool,
    pub is_deleted: bool,
    /// Whether this message has been read by the local user.
    pub is_read: bool,
    /// Sender's timestamp (used for Signal-style deduplication with sender_id).
    /// This is when the sender created the message, not when we received it.
    pub sent_at: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    /// Reaction counts by emoji.
    pub reaction_counts: Vec<ReactionCount>,
    /// Current user's reactions (for UI state).
    pub current_user_reactions: Vec<String>,
    /// AI integrations mentioned in message.
    pub mentioned_integrations: Vec<IntegrationId>,
    pub targets_ai_bot: bool,
    /// Delivery status: "", "sent", "delivered", "read", "sending", "failed".
    pub delivery_status: String,
    /// Short label for a send that did not land. Absent when the failure is
    /// transient (the bubble reads "not sent · retry"). A label does not block
    /// resend — see [`Message::can_retry_send`].
    pub failure_reason: Option<String>,
}

/// Signal's window for edit and delete-for-everyone, measured from `sent_at`.
pub const REMOTE_ACTION_WINDOW_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// Which message-menu actions a bubble may offer.
///
/// Decided here, from send state, sender, content, and age. The UI renders
/// these flags and does not re-decide them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageActions {
    pub resend: bool,
    pub react: bool,
    pub reply: bool,
    pub edit: bool,
    pub pin: bool,
    pub delete_for_everyone: bool,
    pub delete_for_me: bool,
    pub forward: bool,
    pub copy: bool,
    pub download: bool,
    pub info: bool,
    pub select: bool,
}

/// Menu facts the UI needs beside the action flags: pin state (Pin vs Unpin).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageMenu {
    pub actions: MessageActions,
    pub pinned: bool,
}

impl MessageActions {
    /// `now_ns` and `sent_at` are nanoseconds since the Unix epoch.
    ///
    /// A message has reached the server when it is someone else's, or when our
    /// own copy is no longer `"failed"` or `"sending"` (an empty status counts
    /// as sent). Resend is offered for every failed outgoing bubble, including
    /// one that carries a [`Message::failure_reason`]. Edit needs a non-empty
    /// body, so an attachment-only voice note is not editable. Edit and
    /// channel/group delete-for-everyone also require the remote-action window.
    /// Either DM participant may delete an older delivered message for both.
    ///
    /// `one_to_one_dm`: either authenticated participant may request
    /// delete-for-everyone. Channels and groups stay author-only.
    #[must_use]
    pub fn decide(
        is_own: bool,
        is_deleted: bool,
        delivery_status: &str,
        sent_at: u64,
        now_ns: u64,
        text: Option<&str>,
        has_attachments: bool,
        one_to_one_dm: bool,
    ) -> Self {
        let reached = !is_own || (delivery_status != "failed" && delivery_status != "sending");
        let within_window =
            sent_at > 0 && now_ns.saturating_sub(sent_at) <= REMOTE_ACTION_WINDOW_NS;
        let has_body = text.is_some_and(|t| !t.trim().is_empty());
        let on_server = reached && !is_deleted;
        Self {
            resend: is_own && delivery_status == "failed" && !is_deleted,
            react: on_server,
            reply: on_server,
            edit: is_own && on_server && within_window && has_body,
            pin: on_server,
            delete_for_everyone: on_server
                && sent_at > 0
                && (one_to_one_dm || (is_own && within_window)),
            delete_for_me: !is_deleted,
            forward: !is_deleted,
            copy: !is_deleted,
            download: has_attachments && !is_deleted,
            info: is_own && on_server,
            select: !is_deleted,
        }
    }
}

impl Message {
    /// Create a new text message with the given channel, sender, text, and sender's timestamp.
    ///
    /// The `sent_at` parameter is the sender's timestamp (nanoseconds since epoch),
    /// used for Signal-style deduplication with `(sender_id, sent_at)`.
    /// `created_at` is derived from `sent_at` - they represent the same moment.
    #[must_use]
    pub fn new_text(
        conversation_id: ChannelId,
        conversation_type: ConversationType,
        sender_id: Option<UserId>,
        text: String,
        sent_at: u64,
    ) -> Self {
        // Derive created_at from sent_at (they're the same moment)
        let created_at = crate::util::time::TimestampNanos::from_nanos(sent_at)
            .to_datetime()
            .unwrap_or_else(Utc::now);
        tracing::debug!(
            sent_at_nanos = sent_at,
            created_at = %created_at,
            "Message::new_text - timestamp conversion"
        );
        Self {
            id: MessageId::new(),
            conversation_id,
            conversation_type,
            sender_id,
            device_id: None,
            content: MessageContent::Text(text),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at,
            created_at,
            updated_at: created_at,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
            failure_reason: None,
        }
    }

    /// A failed outgoing bubble can be sent again. The failure label is not a lock.
    #[must_use]
    pub fn can_retry_send(&self) -> bool {
        self.delivery_status == "failed" && !self.is_deleted
    }

    /// Menu actions for this message. `is_own` is the caller's authorship check
    /// (sealed-sender rows do not always carry `sender_id`). `now_ns` is
    /// nanoseconds since the Unix epoch.
    #[must_use]
    pub fn actions(&self, is_own: bool, now_ns: u64) -> MessageActions {
        MessageActions::decide(
            is_own,
            self.is_deleted,
            &self.delivery_status,
            self.sent_at,
            now_ns,
            self.as_text(),
            self.has_attachments(),
            self.conversation_type.is_one_to_one_dm(),
        )
    }

    /// Check if this message is the root of a thread.
    /// A message is a thread root if it has no thread_id (not a reply) but has replies.
    /// Note: This requires external knowledge of reply count.
    #[must_use]
    pub fn is_thread_root(&self, has_replies: bool) -> bool {
        self.thread_id.is_none() && has_replies
    }

    /// Check if this message is a reply in a thread.
    #[must_use]
    pub fn is_thread_reply(&self) -> bool {
        self.thread_id.is_some()
    }

    /// Extract text content if this is a Text or Mixed variant.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(text) => Some(text),
            MessageContent::Mixed(mixed) => mixed.text.as_deref(),
            MessageContent::AI(ai) => Some(&ai.text),
            _ => None,
        }
    }

    /// Check if the message has any attachments.
    #[must_use]
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// Mark the message as edited.
    pub fn mark_edited(&mut self) {
        self.is_edited = true;
        self.edited_at = Some(Utc::now());
        self.updated_at = Utc::now();
    }

    /// Mark the message as deleted (soft delete).
    pub fn mark_deleted(&mut self) {
        self.is_deleted = true;
        self.updated_at = Utc::now();
    }

    /// Check if this is a sealed sender (anonymous) message.
    #[must_use]
    pub fn is_sealed_sender(&self) -> bool {
        matches!(self.content, MessageContent::SealedSender(_))
    }

    /// Get the total reaction count.
    #[must_use]
    pub fn total_reactions(&self) -> u32 {
        self.reaction_counts.iter().map(|r| r.count).sum()
    }

    /// Compose a one-line sidebar preview for this message.
    ///
    /// - Text messages (emoji-only included — an emoji IS the text) are
    ///   truncated to 50 chars.
    /// - A captioned attachment prefixes the caption with the label's icon.
    /// - Empty-body messages take `attachment_label` when the caller resolved
    ///   one ("🖼️ Photo", "📎 report.pdf"), else "📎 N attachment(s)".
    /// - A call reads "📞 Missed/Incoming/Outgoing call"; a system notice reads
    ///   its description.
    /// - Other empty-body messages with no attachments render as "[Message]".
    /// - If `current_user_id` is provided and matches this message's sender,
    ///   the preview is prefixed with "You: " (text and attachments alike).
    /// - Other senders in channels and groups use the first word of the
    ///   resolved bubble sender name, followed by ": ". DMs, calls, and
    ///   system notices do not use this prefix.
    /// - A pinned message carries a 📌 marker, so the row says *why* this is
    ///   the last thing that happened here.
    ///
    /// Attachment names and MIME types live in storage, not on the message, so
    /// the typed label and bubble sender name are passed in by
    /// `ChatterClient::message_sidebar_preview`.
    ///
    /// Lives in core because "how do we describe a message in the sidebar"
    /// is a domain decision shared by every UI surface, not a presentation
    /// concern owned by Slint.
    #[must_use]
    pub fn sidebar_preview(
        &self,
        current_user_id: Option<&crate::domain::ids::UserId>,
        attachment_label: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> String {
        let is_own = matches!(
            (current_user_id, self.sender_id),
            (Some(u), Some(s)) if *u == s
        );
        let pin = if self.is_pinned { "\u{1F4CC} " } else { "" };
        let text = self.as_text().unwrap_or("");
        let att_count = self.attachments.len();

        match &self.content {
            // A call or a notice has no body; say what it was.
            MessageContent::Call(call) => {
                let what = match call.call_type {
                    CallMessageType::Missed => "Missed call",
                    CallMessageType::Declined => "Declined call",
                    _ if current_user_id == Some(&call.initiator_id) => "Outgoing call",
                    _ => "Incoming call",
                };
                return format!("{}\u{1F4DE} {}", pin, what);
            }
            MessageContent::System(system) => {
                return format!("{}{}", pin, system.description_for(&self.conversation_type));
            }
            _ => {}
        }
        let body = if !text.is_empty() {
            // A caption keeps its attachment's icon ("📷 caption"), as Signal
            // does; every label starts with its icon.
            match attachment_label.and_then(|label| label.split_once(' ')) {
                Some((icon, _)) => format!("{} {}", icon, text),
                None => text.to_string(),
            }
        } else if let Some(label) = attachment_label {
            label.to_string()
        } else if att_count > 0 {
            let suffix = if att_count == 1 { "" } else { "s" };
            format!("\u{1F4CE} {} attachment{}", att_count, suffix)
        } else {
            "[Message]".to_string()
        };
        if is_own {
            format!("{}You: {}", pin, body)
        } else if matches!(
            &self.conversation_type,
            ConversationType::Channel | ConversationType::AnonGroup | ConversationType::Mesh
        ) {
            let name = sender_display_name
                .and_then(|name| name.split_whitespace().next())
                .unwrap_or("");
            if name.is_empty() {
                format!("{}{}", pin, body)
            } else {
                format!("{}{}: {}", pin, name, body)
            }
        } else {
            format!("{}{}", pin, body)
        }
    }
}

/// Truncate text for a single-line preview, cutting to `max` chars and
/// appending `...` when the original overflowed. Counts by chars, not bytes,
/// so emoji and multibyte glyphs are counted correctly.
#[must_use]
pub fn truncate_preview_text(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        // Reserve 3 chars for the ellipsis.
        let head_len = max.saturating_sub(3);
        let truncated: String = text.chars().take(head_len).collect();
        format!("{}...", truncated)
    } else {
        text.to_string()
    }
}
