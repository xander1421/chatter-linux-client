//! Shared utilities for anonymous DM and group content processing.
//!
//! Services (`AnonDmService`, `AnonGroupService`) use these helpers to build
//! unified `Message` instances and extract reply/quote metadata from inbound
//! `Content` protos.

use std::str::FromStr;

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::domain::UserId;
use crate::proto::content_v1::{self, Content};

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
        if reply_to.is_none() && reply_to_author_id.is_none() && reply_to_text.is_none() {
            return None;
        }
        Some(Self {
            reply_to,
            reply_to_author_id,
            reply_to_text,
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
