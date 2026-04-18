//! Shared utilities for anonymous DM and group content processing.
//!
//! Services (`AnonDmService`, `AnonGroupService`) use these helpers to build
//! unified `Message` instances, extract reply/quote metadata, and dispatch the
//! content types that behave identically across DM and Group conversations
//! (data messages, reactions, edits, pins, typing, receipts).

use std::fmt;
use std::str::FromStr;

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::domain::UserId;
use crate::proto::content_v1::{self, AttachmentPointer, Content, receipt_message, typing_message};

use super::DomainEvent;

// ============================================================================
// Quote / reply context (extracted from the inbound Content proto)
// ============================================================================

/// Reply/quote metadata persisted alongside an anonymous data message so
/// reply bubbles survive a channel reload on the receiver side.
#[derive(Debug, Clone)]
pub struct QuoteContext {
    pub reply_to: Option<MessageId>,
    pub reply_to_author_id: Option<UserId>,
    pub reply_to_text: Option<String>,
    /// JPEG/PNG thumbnail bytes of the first quoted attachment.
    pub reply_to_thumbnail: Option<Vec<u8>>,
    /// MIME type of the quoted thumbnail (e.g. `image/jpeg`).
    pub reply_to_thumbnail_mime: Option<String>,
}

impl QuoteContext {
    /// Extract reply context from a Content proto's DataMessage quote.
    pub fn from_content(inner_content: &Content) -> Option<Self> {
        let dm = match &inner_content.content {
            Some(content_v1::content::Content::DataMessage(dm)) => dm,
            _ => return None,
        };
        let quote = dm.quote.as_ref()?;
        let reply_to = Some(MessageId::from_timestamp(quote.id));
        let reply_to_author_id = if quote.author_aci.is_empty() {
            None
        } else {
            UserId::from_str(&quote.author_aci).ok()
        };
        let reply_to_text = if quote.text.is_empty() {
            None
        } else {
            Some(quote.text.clone())
        };
        let (reply_to_thumbnail, reply_to_thumbnail_mime) = quote
            .attachments
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));
        if reply_to.is_none()
            && reply_to_author_id.is_none()
            && reply_to_text.is_none()
            && reply_to_thumbnail.is_none()
        {
            return None;
        }
        Some(Self {
            reply_to,
            reply_to_author_id,
            reply_to_text,
            reply_to_thumbnail,
            reply_to_thumbnail_mime,
        })
    }
}

// ============================================================================
// Message construction
// ============================================================================

/// Build a unified `Message` from anonymous content handler data.
pub fn build_message_from_content(
    message_id_str: &str,
    conversation_id: ChannelId,
    conversation_type: ConversationType,
    sender_id: UserId,
    inner_content: &Content,
    sent_at: u64,
) -> Message {
    let msg_id = message_id_str
        .parse::<uuid::Uuid>()
        .map(MessageId::from_uuid)
        .unwrap_or_else(|_| MessageId::new());

    let content = content_to_message_content(inner_content);
    let created_at = crate::util::time::TimestampNanos::from_nanos(sent_at)
        .to_datetime()
        .unwrap_or_else(chrono::Utc::now);

    let data_msg = match &inner_content.content {
        Some(content_v1::content::Content::DataMessage(dm)) => Some(dm),
        _ => None,
    };

    let reply_to = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .map(|q| MessageId::from_timestamp(q.id));

    let reply_to_author_id = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .and_then(|q| {
            if q.author_aci.is_empty() { None } else { UserId::from_str(&q.author_aci).ok() }
        });

    let reply_to_text = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .map(|q| q.text.clone())
        .filter(|t| !t.is_empty());

    let (reply_to_thumbnail, reply_to_thumbnail_mime) = data_msg
        .and_then(|dm| dm.quote.as_ref())
        .and_then(|q| q.attachments.first())
        .map(|qa| {
            let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
            let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
            (thumb, mime)
        })
        .unwrap_or((None, None));

    let attachments: Vec<AttachmentId> = data_msg
        .map(|dm| {
            dm.attachments.iter()
                .filter(|ptr| !ptr.cdn_key.is_empty())
                .map(|ptr| AttachmentId::from_cdn_key(&ptr.cdn_key))
                .collect()
        })
        .unwrap_or_default();

    Message {
        id: msg_id,
        conversation_id,
        conversation_type,
        sender_id: Some(sender_id),
        device_id: None,
        content,
        encrypted_content: None,
        thread_id: None,
        reply_to,
        reply_to_author_id,
        reply_to_text,
        reply_to_thumbnail,
        reply_to_thumbnail_mime,
        attachments,
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
    }
}

/// Convert Content proto to MessageContent for the unified Message struct.
pub fn content_to_message_content(content: &Content) -> MessageContent {
    match &content.content {
        Some(content_v1::content::Content::DataMessage(data_msg)) => {
            MessageContent::Text(data_msg.body.clone())
        }
        _ => MessageContent::Text(String::new()),
    }
}

// ============================================================================
// Shared backend trait
// ============================================================================

/// Persistence + blocking ops used by the common content dispatcher.
///
/// Implemented for both `AnonDmService` and `AnonGroupService` so the
/// dispatcher can persist + emit events without caring which conversation
/// flavor it's operating on.
pub trait AnonInboxBackend {
    /// The conversation/group ID type (`AnonDmId` or `AnonGroupId`).
    type Id: Copy + fmt::Display + Into<ChannelId>;

    fn conversation_type(&self) -> ConversationType;
    fn label(&self) -> &'static str;

    /// Default: false. DM overrides to check its block list.
    fn is_sender_blocked(&self, _sender_id: &UserId) -> bool { false }

    /// Default: false. DM overrides to check per-conversation blocking.
    fn is_conversation_blocked(&self, _id: &Self::Id) -> bool { false }

    fn create_attachment_records(&self, pointers: &[AttachmentPointer]) -> Vec<AttachmentId>;

    fn store_data_message(
        &self,
        id: &Self::Id,
        sender: &UserId,
        text: &str,
        attachments: &[AttachmentId],
        sent_at: u64,
        quote: Option<QuoteContext>,
    ) -> Result<String, String>;

    fn delete_message(&self, id: &Self::Id, sender: &UserId, ts: u64) -> Result<bool, String>;

    fn update_message_text(&self, id: &Self::Id, sender: &UserId, ts: u64, text: &str) -> Result<bool, String>;

    fn add_reaction(
        &self, id: &Self::Id, target_sender: &UserId, target_ts_ns: u64,
        reactor: &UserId, emoji: &str,
    ) -> Result<(), String>;

    fn remove_reaction(
        &self, id: &Self::Id, target_sender: &UserId, target_ts_ns: u64,
        reactor: &UserId, emoji: &str,
    ) -> Result<(), String>;

    fn set_pinned(&self, id: &Self::Id, ts: u64, pinned: bool) -> Result<(), String>;

    fn get_message_id_by_timestamp(&self, id: &Self::Id, ts: u64) -> Option<MessageId>;

    /// Default: no-op. DM overrides to persist read/delivered status to its table.
    fn persist_delivery_status(&self, _id: &Self::Id, _timestamps: &[u64], _status: &str) {}

    /// Whether receipts should emit `MessagesRead` / `MessagesDelivered` events.
    /// Default: true. Group overrides to suppress (receipts are debug-only there).
    fn emit_receipts(&self) -> bool { true }
}

// ============================================================================
// Shared dispatcher
// ============================================================================

/// Outcome of dispatching a content proto through the common handler.
pub enum CommonDispatchOutcome {
    /// Content handled by common dispatcher. Caller emits these events.
    Handled(Vec<DomainEvent>),
    /// Content type not handled here. Caller must dispatch per-service logic
    /// (e.g. calls, profile updates, group management).
    NotHandled,
    /// Content intentionally dropped (blocked sender, blocked conversation).
    Dropped,
}

/// Dispatch the content types that behave identically across DM and Group.
///
/// Handled: DataMessage (store + delete), Typing, Reaction, Receipt, Pin, Edit.
/// Everything else returns `NotHandled` so the caller can dispatch per-service
/// content (calls, profile updates, group management, anon-group invites, etc.).
pub fn dispatch_common_content<B: AnonInboxBackend>(
    id: &B::Id,
    sender_id: &UserId,
    inner_content: &Content,
    backend: &B,
) -> CommonDispatchOutcome {
    use content_v1::content::Content as ContentKind;

    if backend.is_sender_blocked(sender_id) {
        tracing::info!(
            sender_id = %sender_id,
            id = %id,
            label = backend.label(),
            "Dropped content from blocked sender"
        );
        return CommonDispatchOutcome::Dropped;
    }
    if backend.is_conversation_blocked(id) {
        tracing::info!(
            id = %id,
            label = backend.label(),
            "Dropped content for blocked conversation"
        );
        return CommonDispatchOutcome::Dropped;
    }

    let channel_id: ChannelId = (*id).into();
    let mut events = Vec::new();

    match &inner_content.content {
        Some(ContentKind::DataMessage(data_msg)) => {
            // Delete-for-everyone wrapped inside DataMessage
            if let Some(delete) = &data_msg.delete {
                match backend.delete_message(id, sender_id, delete.target_sent_timestamp) {
                    Ok(true) => tracing::info!(
                        timestamp = delete.target_sent_timestamp,
                        sender_id = %sender_id,
                        id = %id,
                        "Deleted message (delete-for-everyone)"
                    ),
                    Ok(false) => tracing::debug!(
                        timestamp = delete.target_sent_timestamp,
                        "Delete target not found"
                    ),
                    Err(e) => tracing::warn!(error = %e, "Failed to delete message"),
                }
                events.push(DomainEvent::MessageDeleted {
                    message_id: MessageId::from_timestamp(delete.target_sent_timestamp),
                    author_id: *sender_id,
                    channel_id: Some(channel_id),
                    target_sent_timestamp: delete.target_sent_timestamp,
                });
                return CommonDispatchOutcome::Handled(events);
            }

            let has_text = !data_msg.body.is_empty();
            let has_attachments = !data_msg.attachments.is_empty();
            if !has_text && !has_attachments {
                return CommonDispatchOutcome::Handled(events);
            }

            let attachment_ids = if has_attachments {
                backend.create_attachment_records(&data_msg.attachments)
            } else {
                Vec::new()
            };

            let sent_at = inner_content.timestamp;
            let quote = QuoteContext::from_content(inner_content);

            match backend.store_data_message(
                id, sender_id, &data_msg.body, &attachment_ids, sent_at, quote,
            ) {
                Ok(message_id_str) => {
                    tracing::info!(
                        id = %id,
                        message_id = %message_id_str,
                        sent_at = sent_at,
                        has_text = has_text,
                        attachment_count = attachment_ids.len(),
                        label = backend.label(),
                        "Stored data message"
                    );
                    let message = build_message_from_content(
                        &message_id_str,
                        channel_id,
                        backend.conversation_type(),
                        *sender_id,
                        inner_content,
                        sent_at,
                    );
                    events.push(DomainEvent::MessageReceived { message });
                    // Clear typing indicator when message arrives — prevents stuck indicators.
                    events.push(DomainEvent::TypingStopped {
                        channel_id,
                        user_id: *sender_id,
                    });
                }
                Err(e) => {
                    tracing::error!(
                        id = %id,
                        error = %e,
                        label = backend.label(),
                        "Failed to store data message"
                    );
                }
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Typing(typing)) => {
            let started = typing.action == typing_message::Action::Started as i32;
            let event = if started {
                DomainEvent::TypingStarted { channel_id, user_id: *sender_id }
            } else {
                DomainEvent::TypingStopped { channel_id, user_id: *sender_id }
            };
            events.push(event);
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Reaction(reaction)) => {
            let target_sender: UserId = reaction.target_author_aci.parse().unwrap_or(*sender_id);
            let sent_at_ns = reaction.target_sent_timestamp;

            if reaction.remove {
                if let Err(e) = backend.remove_reaction(id, &target_sender, sent_at_ns, sender_id, &reaction.emoji) {
                    tracing::warn!(id = %id, error = %e, "Failed to remove reaction");
                }
                events.push(DomainEvent::ReactionRemoved {
                    sender_id: target_sender,
                    sent_at_ns,
                    reactor_id: *sender_id,
                    emoji: reaction.emoji.clone(),
                    timestamp: reaction.target_sent_timestamp,
                });
            } else {
                if let Err(e) = backend.add_reaction(id, &target_sender, sent_at_ns, sender_id, &reaction.emoji) {
                    tracing::warn!(id = %id, error = %e, "Failed to add reaction");
                }
                events.push(DomainEvent::ReactionAdded {
                    sender_id: target_sender,
                    sent_at_ns,
                    reactor_id: *sender_id,
                    emoji: reaction.emoji.clone(),
                    timestamp: reaction.target_sent_timestamp,
                });
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Receipt(receipt)) => {
            if receipt.r#type == receipt_message::Type::Read as i32 {
                backend.persist_delivery_status(id, &receipt.timestamps, "read");
                if backend.emit_receipts() {
                    events.push(DomainEvent::MessagesRead {
                        reader_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                        channel_id: Some(channel_id),
                    });
                }
            } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
                backend.persist_delivery_status(id, &receipt.timestamps, "delivered");
                if backend.emit_receipts() {
                    events.push(DomainEvent::MessagesDelivered {
                        recipient_id: *sender_id,
                        message_timestamps: receipt.timestamps.clone(),
                        receipt_timestamp: receipt.timestamp,
                    });
                }
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Pin(pin)) => {
            let pinned = !pin.unpin;
            if let Err(e) = backend.set_pinned(id, pin.target_sent_timestamp, pinned) {
                tracing::warn!(id = %id, error = %e, "Failed to persist pin state");
            }
            let message_id = backend
                .get_message_id_by_timestamp(id, pin.target_sent_timestamp)
                .unwrap_or_else(|| MessageId::from_timestamp(pin.target_sent_timestamp));

            if pin.unpin {
                events.push(DomainEvent::MessageUnpinned {
                    message_id,
                    unpinner_id: *sender_id,
                    channel_id,
                    target_sent_timestamp: pin.target_sent_timestamp,
                });
            } else {
                events.push(DomainEvent::MessagePinned {
                    message_id,
                    pinner_id: *sender_id,
                    channel_id,
                    target_sent_timestamp: pin.target_sent_timestamp,
                });
            }
            CommonDispatchOutcome::Handled(events)
        }

        Some(ContentKind::Edit(edit)) => {
            let target_timestamp = edit.target_sent_timestamp;
            let new_text = edit.data_message.as_ref().map(|dm| dm.body.clone()).unwrap_or_default();
            match backend.update_message_text(id, sender_id, target_timestamp, &new_text) {
                Ok(true) => {
                    events.push(DomainEvent::MessageEdited {
                        message_id: MessageId::from_timestamp(target_timestamp),
                        new_content: MessageContent::Text(new_text),
                        channel_id,
                        conversation_type: backend.conversation_type(),
                        target_sent_timestamp: target_timestamp,
                    });
                }
                Ok(false) => tracing::debug!(
                    id = %id,
                    target_timestamp,
                    "Edit ignored: message not found or sender not author"
                ),
                Err(e) => tracing::warn!(id = %id, error = %e, "Failed to update message text"),
            }
            CommonDispatchOutcome::Handled(events)
        }

        _ => CommonDispatchOutcome::NotHandled,
    }
}
