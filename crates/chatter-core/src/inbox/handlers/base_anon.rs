//! Shared handler logic for anonymous DM and group content.
//!
//! Both `AnonDmHandler` and `AnonGroupHandler` handle the same set of
//! "standard content" types (typing, reactions, receipts, edits, deletes,
//! data messages) with identical logic. This module extracts that shared
//! logic so fixes and features only need to be implemented once.

use std::str::FromStr;

use crate::domain::ids::ChannelId;
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

    /// Build the domain event for a received message.
    fn message_received_event(
        &self,
        id: Self::Id,
        sender_id: UserId,
        message_id: String,
        inner_content: Content,
        sent_at: u64,
    ) -> DomainEvent;

    /// Build the domain event for an edited message.
    fn message_edited_event(
        &self,
        id: Self::Id,
        message_timestamp: u64,
        new_text: String,
    ) -> DomainEvent;

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
            ctx.emit(svc.message_edited_event(id, target_timestamp, new_text));
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

    let message_id = svc.store_message(&id, &sender_id, content_type, &content_json, sent_at)
        .map_err(|e| HandlerError::parse_failed(format!("storage error: {}", e)))?;

    tracing::debug!(
        "Received and stored {} message {} from {} in {}",
        svc.label(), message_id, sender_id, id
    );

    ctx.emit(svc.message_received_event(id, sender_id, message_id, inner_content.clone(), sent_at));

    Ok(HandlerOutcome::Processed)
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
