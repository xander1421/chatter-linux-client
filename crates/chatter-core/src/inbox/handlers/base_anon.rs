//! Shared handler logic for anonymous DM and group content.
//!
//! Both `AnonDmHandler` and `AnonGroupHandler` handle the same set of
//! "standard content" types (typing, reactions, receipts, edits, deletes,
//! data messages) with identical logic. This module extracts that shared
//! logic so fixes and features only need to be implemented once.

use std::str::FromStr;

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::domain::UserId;
use crate::proto::content_v1::{self, typing_message, receipt_message, Content};

use super::{DomainEvent, HandlerContext, HandlerOutcome, HandlerResult, HandlerError};

// ============================================================================
// Service trait for operations that differ between DM and Group
// ============================================================================

/// Abstraction over `AnonDmService` and `AnonGroupService` for the operations
/// that the shared content handlers need.
pub(super) trait AnonContentService {
    /// The conversation/group ID type (AnonDmId or AnonGroupId).
    type Id: std::fmt::Display + Into<ChannelId> + Copy;

    /// Delete a message by sender timestamp. Returns true if found and deleted.
    fn delete_message_by_timestamp(
        &self,
        id: &Self::Id,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> Result<bool, String>;

    /// Update message text by sender (edit). Returns true if found and updated.
    fn update_message_text_by_sender(
        &self,
        id: &Self::Id,
        sender_id: &UserId,
        sent_timestamp: u64,
        new_text: &str,
    ) -> Result<bool, String>;

    /// Store a data message. Returns the message ID as a string.
    fn store_message(
        &self,
        id: &Self::Id,
        sender_id: &UserId,
        content_type: i32,
        content_json: &str,
        sent_at: u64,
    ) -> Result<String, String>;

    /// The conversation type for this handler (AnonDm or AnonGroup).
    fn conversation_type(&self) -> ConversationType;

    /// Label for log messages ("anon DM" or "anon group").
    fn label(&self) -> &'static str;
}

// ============================================================================
// Shared handler functions
// ============================================================================

/// Handle a typing indicator (shared between DM and Group).
pub(super) fn handle_typing<S: AnonContentService>(
    id: S::Id,
    sender_id: UserId,
    typing: &content_v1::TypingMessage,
    ctx: &HandlerContext,
    svc: &S,
) -> HandlerResult {
    let channel_id: ChannelId = id.into();

    let event = if typing.action == typing_message::Action::Started as i32 {
        DomainEvent::TypingStarted {
            channel_id,
            user_id: sender_id,
        }
    } else {
        DomainEvent::TypingStopped {
            channel_id,
            user_id: sender_id,
        }
    };
    ctx.emit(event);

    tracing::debug!(
        "Received typing indicator from {} in {} {}",
        sender_id, svc.label(), id
    );

    Ok(HandlerOutcome::Processed)
}

/// Handle a reaction (shared between DM and Group).
pub(super) fn handle_reaction<S: AnonContentService>(
    id: S::Id,
    sender_id: UserId,
    reaction: &content_v1::Reaction,
    ctx: &HandlerContext,
    svc: &S,
) -> HandlerResult {
    let message_id = crate::domain::MessageId::from_timestamp(reaction.target_sent_timestamp);
    let author_id = UserId::from_str(&reaction.target_author_aci)
        .unwrap_or(sender_id);

    let event = if reaction.remove {
        DomainEvent::ReactionRemoved {
            message_id,
            author_id,
            reactor_id: sender_id,
            emoji: reaction.emoji.clone(),
            timestamp: reaction.timestamp,
        }
    } else {
        DomainEvent::ReactionAdded {
            message_id,
            author_id,
            reactor_id: sender_id,
            emoji: reaction.emoji.clone(),
            timestamp: reaction.timestamp,
        }
    };
    ctx.emit(event);

    tracing::debug!(
        "Received reaction {} from {} in {} {}",
        reaction.emoji, sender_id, svc.label(), id
    );

    Ok(HandlerOutcome::Processed)
}

/// Handle a read/delivery receipt (shared between DM and Group).
pub(super) fn handle_receipt<S: AnonContentService>(
    id: S::Id,
    sender_id: UserId,
    receipt: &content_v1::ReceiptMessage,
    ctx: &HandlerContext,
    svc: &S,
) -> HandlerResult {
    if receipt.r#type == receipt_message::Type::Read as i32 {
        let channel_id: ChannelId = id.into();
        ctx.emit(DomainEvent::MessagesRead {
            reader_id: sender_id,
            message_timestamps: receipt.timestamps.clone(),
            receipt_timestamp: receipt.timestamp,
            channel_id: Some(channel_id),
        });

        tracing::debug!(
            "Received read receipt for {} messages from {} in {} {}",
            receipt.timestamps.len(), sender_id, svc.label(), id
        );
    } else if receipt.r#type == receipt_message::Type::Delivery as i32 {
        ctx.emit(DomainEvent::MessagesDelivered {
            recipient_id: sender_id,
            message_timestamps: receipt.timestamps.clone(),
            receipt_timestamp: receipt.timestamp,
        });

        tracing::debug!(
            "Received delivery receipt for {} messages from {} in {} {}",
            receipt.timestamps.len(), sender_id, svc.label(), id
        );
    }

    Ok(HandlerOutcome::Processed)
}

/// Handle a delete-for-everyone message (shared between DM and Group).
pub(super) fn handle_delete<S: AnonContentService>(
    id: S::Id,
    sender_id: UserId,
    target_sent_timestamp: u64,
    ctx: &HandlerContext,
    svc: &S,
) -> HandlerResult {
    match svc.delete_message_by_timestamp(&id, &sender_id, target_sent_timestamp) {
        Ok(true) => {
            tracing::info!(
                "Deleted message at timestamp {} from {} in {} {}",
                target_sent_timestamp, sender_id, svc.label(), id
            );
        }
        Ok(false) => {
            tracing::debug!(
                "Message at timestamp {} not found for deletion in {} {}",
                target_sent_timestamp, svc.label(), id
            );
        }
        Err(e) => {
            tracing::warn!(
                "Failed to delete message at timestamp {} in {} {}: {}",
                target_sent_timestamp, svc.label(), id, e
            );
        }
    }

    let message_id = crate::domain::MessageId::from_timestamp(target_sent_timestamp);
    ctx.emit(DomainEvent::MessageDeleted {
        message_id,
        author_id: sender_id,
        channel_id: Some(id.into()),
    });

    Ok(HandlerOutcome::Processed)
}

/// Handle an edit message (shared between DM and Group).
pub(super) fn handle_edit<S: AnonContentService>(
    id: S::Id,
    sender_id: UserId,
    edit: &content_v1::EditMessage,
    ctx: &HandlerContext,
    svc: &S,
) -> HandlerResult {
    let target_timestamp = edit.target_sent_timestamp;
    let new_text = edit
        .data_message
        .as_ref()
        .map(|dm| dm.body.clone())
        .unwrap_or_default();

    match svc.update_message_text_by_sender(&id, &sender_id, target_timestamp, &new_text) {
        Ok(true) => {
            tracing::info!(
                sender_id = %sender_id,
                id = %id,
                target_timestamp,
                "Updated message text (edit from peer)"
            );
            ctx.emit(DomainEvent::MessageEdited {
                message_id: MessageId::from_timestamp(target_timestamp),
                new_content: MessageContent::Text(new_text),
                channel_id: id.into(),
                conversation_type: svc.conversation_type(),
                target_sent_timestamp: target_timestamp,
            });
        }
        Ok(false) => {
            tracing::debug!(
                sender_id = %sender_id,
                id = %id,
                target_timestamp,
                "Edit ignored: message not found or sender not author"
            );
        }
        Err(e) => {
            tracing::warn!(
                sender_id = %sender_id,
                id = %id,
                target_timestamp,
                error = %e,
                "Failed to update message text"
            );
        }
    }

    Ok(HandlerOutcome::Processed)
}

/// Handle a regular data message (shared between DM and Group).
pub(super) fn handle_data_message<S: AnonContentService>(
    id: S::Id,
    sender_id: UserId,
    inner_content: &Content,
    ctx: &HandlerContext,
    svc: &S,
) -> HandlerResult {
    let (content_type, content_json) = content_to_storage(inner_content);
    let sent_at = inner_content.timestamp;

    let message_id_str = svc.store_message(&id, &sender_id, content_type, &content_json, sent_at)
        .map_err(|e| HandlerError::parse_failed(format!("storage error: {}", e)))?;

    tracing::debug!(
        "Received and stored {} message {} from {} in {}",
        svc.label(), message_id_str, sender_id, id
    );

    let message = build_message_from_content(
        &message_id_str,
        id.into(),
        svc.conversation_type(),
        sender_id,
        inner_content,
        sent_at,
    );
    ctx.emit(DomainEvent::MessageReceived { message });

    Ok(HandlerOutcome::Processed)
}

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

    // Extract quote/reply and attachment fields from DataMessage proto
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

/// Convert Content proto to storage format (content_type, content_json).
///
/// Shared between DM and Group handlers (identical logic).
pub(super) fn content_to_storage(content: &Content) -> (i32, String) {
    match &content.content {
        Some(content_v1::content::Content::DataMessage(data_msg)) => {
            (1, serde_json::json!({ "text": data_msg.body }).to_string())
        }
        Some(content_v1::content::Content::Attachment(att)) => {
            (2, serde_json::json!({
                "caption": att.caption,
                "channel_id": att.channel_id,
            }).to_string())
        }
        Some(content_v1::content::Content::Reaction(reaction)) => {
            (3, serde_json::json!({
                "emoji": reaction.emoji,
                "target_sent_timestamp": reaction.target_sent_timestamp,
                "remove": reaction.remove,
            }).to_string())
        }
        _ => {
            (0, "{}".to_string())
        }
    }
}
