//! Message gateway module.
//!
//! Central message routing and validation pipeline.
//! Decrypts sealed sender messages, parses DataMessage proto, and stores to repository.

pub mod builders;
mod decryption;
pub mod inbound;
pub mod outgoing;
mod routing;
pub mod shared;

use std::sync::Arc;

use crate::crypto::signal::sesame::{SesameManager, SqliteSesameStore};
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::proto::content_v1::DataMessage;
use crate::services::SignalService;
use crate::storage::repositories::MessageRepository;

// Re-export inbound types (moved from inline definitions)
pub use inbound::{
    content_type_name,
    DmContent, DecryptResult, RouteResult, SealedSenderResult,
};
use crate::util::time::TimestampNanos;

// Re-export all content builders
pub use builders::{
    // Padding
    generate_padding, PADDING_GROUP_CONTROL, PADDING_MESSAGE, PADDING_LARGE,
    // DataMessage builders
    build_text_content, build_data_message_content, build_reply_content,
    build_data_message_content_with_quote, build_data_message_content_with_channel,
    // Control message builders
    build_typing_content, build_typing_content_with_channel,
    build_read_receipt_content, build_read_receipt_content_with_channel,
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_reaction_content, build_reaction_content_with_channel,
    build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_saved_message_sync_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
    // DM call key exchange (ephemeral keys for PFS)
    build_call_key_offer, build_call_key_answer,
    // Group call key distribution (pairwise sealed sender)
    build_group_call_key_content,
    // Call hangup
    build_call_hangup_content,
    // Anonymous call signaling (anon DM + anon group)
    generate_anon_call_room,
    build_anon_call_token,
    build_anon_call_offer_content,
    build_anon_call_answer_content,
    build_anon_call_hangup_content,
};

// Re-export outgoing router
pub use outgoing::{
    OutgoingIntent, QuoteIntent, SendResult, SendError,
    OutgoingServices, route_outgoing,
    OwnedOutgoingServices, route_outgoing_owned,
};

// ============================================================================
// Types
// ============================================================================

/// Reason for call hangup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallHangupReason {
    /// User ended call normally
    Normal,
    /// Call was declined
    Declined,
    /// User is busy (on another call)
    Busy,
    /// Call not answered in time
    Timeout,
    /// Network connectivity issue
    NetworkError,
}

// DmContent, DecryptResult, RouteResult, SealedSenderResult are in inbound.rs
// and re-exported above.

// ============================================================================
// MessageGateway
// ============================================================================

/// Message gateway for routing and validation.
pub struct MessageGateway {
    pub(crate) signal_service: SignalService,
    pub(crate) message_repo: Arc<MessageRepository>,
    pub(crate) sesame_manager: Option<Arc<SesameManager<SqliteSesameStore>>>,
}

impl MessageGateway {
    /// Create a new MessageGateway.
    pub fn new(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            sesame_manager: None,
        }
    }

    /// Create a new MessageGateway with Sesame session management.
    pub fn with_sesame(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
        sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            sesame_manager: Some(sesame_manager),
        }
    }

    /// Store a DataMessage to the repository.
    pub(crate) async fn store_data_message(
        &self,
        data_message: DataMessage,
        sender_id: Option<UserId>,
    ) -> Result<SealedSenderResult, crate::error::ServiceError> {
        use crate::error::ServiceError;

        // Parse channel ID (required for group messages)
        let channel_id: ChannelId = if data_message.channel_id.is_empty() {
            // For DMs, derive channel from sender (would need conversation lookup)
            return Err(ServiceError::Validation("Missing channel_id in DataMessage".into()));
        } else {
            data_message.channel_id.parse().map_err(|e| {
                ServiceError::Validation(format!("Invalid channel_id: {}", e))
            })?
        };

        // Generate message ID from timestamp (Signal's approach: timestamp + author = unique)
        let message_id = MessageId::new();

        // Convert timestamp to datetime (nanoseconds since epoch)
        let created_at = if data_message.timestamp > 0 {
            TimestampNanos::from_nanos(data_message.timestamp)
                .to_datetime()
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };

        // Extract content
        let content = MessageContent::Text(data_message.body.clone());

        // Convert attachments: generate deterministic attachment IDs from CDN keys
        // CDN keys are base64 strings, we hash them to generate consistent UUIDs
        // NOTE: Actual Attachment records are created by AttachmentService.create_from_pointers()
        let attachments: Vec<crate::domain::ids::AttachmentId> = data_message
            .attachments
            .iter()
            .filter(|ptr| !ptr.cdn_key.is_empty())
            .map(|ptr| crate::domain::ids::AttachmentId::from_cdn_key(&ptr.cdn_key))
            .collect();

        // Extract quote/reply context from DataMessage so reply bubbles survive reload
        let reply_to = data_message.quote.as_ref()
            .map(|q| MessageId::from_timestamp(q.id));
        let reply_to_author_id = data_message.quote.as_ref()
            .and_then(|q| {
                if q.author_aci.is_empty() { None } else { q.author_aci.parse().ok() }
            });
        let reply_to_text = data_message.quote.as_ref()
            .map(|q| q.text.clone())
            .filter(|t| !t.is_empty());
        let (reply_to_thumbnail, reply_to_thumbnail_mime) = data_message
            .quote
            .as_ref()
            .and_then(|q| q.attachments.first())
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));

        // Create domain message with sender's timestamp for Signal-style dedup
        let message = Message {
            id: message_id,
            conversation_id: channel_id,
            conversation_type: ConversationType::AnonGroup,
            sender_id,
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
            sent_at: data_message.timestamp,
            created_at,
            updated_at: created_at,
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: data_message
                .mentioned_integrations
                .iter()
                .filter_map(|id| id.parse().ok())
                .collect(),
            targets_ai_bot: data_message.targets_ai_bot,
            delivery_status: String::new(),
        };

        // Store in repository
        self.message_repo.insert(&message)?;

        tracing::info!(
            message_id = %message.id,
            channel_id = %message.conversation_id,
            attachment_count = message.attachments.len(),
            attachment_ids = ?message.attachments.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            "Stored incoming message with attachments"
        );

        // Return details so caller can emit UI events
        Ok(SealedSenderResult::DataMessageReceived {
            sender_id: sender_id.unwrap_or_else(|| UserId::from_uuid(uuid::Uuid::nil())),
            channel_id,
            message_id,
            body: data_message.body,
            timestamp: data_message.timestamp,
            attachments: data_message.attachments,
        })
    }

    /// Create a sender key request message for a channel member.
    ///
    /// Call this when you receive an encrypted message but don't have
    /// the sender's key.
    pub fn create_sender_key_request(
        channel_id: &ChannelId,
        target_user_id: &UserId,
    ) -> crate::proto::content_v1::SenderKeyRequest {
        crate::proto::content_v1::SenderKeyRequest {
            channel_id: channel_id.to_string(),
            target_user_id: target_user_id.to_string(),
            timestamp: TimestampNanos::now().as_u64(),
        }
    }

    /// Get the local identity private key for ECIES decryption.
    ///
    /// Returns the 32-byte X25519 private scalar used to decrypt
    /// NDSS'21 initial contact messages.
    pub async fn get_identity_key(&self) -> Result<[u8; 32], crate::error::ServiceError> {
        self.signal_service.get_identity_private_key().await
    }
}
