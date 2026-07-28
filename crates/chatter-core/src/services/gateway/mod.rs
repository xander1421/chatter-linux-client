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
use crate::services::current_user::CurrentUserProvider;
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
    build_data_message_content_with_quote,
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
    pub(crate) channel_repo: Arc<crate::storage::repositories::channel_repo::ChannelRepository>,
    /// For storing a friend's profile key piggybacked on an incoming DM
    /// DataMessage (Signal-style), so their encrypted name/avatar can be
    /// decrypted later.
    pub(crate) user_repo: Arc<crate::storage::repositories::UserRepository>,
    pub(crate) settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
    pub(crate) sesame_manager: Option<Arc<SesameManager<SqliteSesameStore>>>,
    /// Local disappearing-message TTL tracking. Set on the production gateway
    /// so an INBOUND message that arrives with an armed `ephemeral_ttl`
    /// (V29 wire marker) tracks the RECIPIENT's local copy for reaping. `None`
    /// on gateways built without it (the plain `new` constructor / some tests)
    /// — arming is simply skipped there.
    pub(crate) ephemeral_repo:
        Option<Arc<crate::storage::repositories::ephemeral_repo::EphemeralRepository>>,
}

impl MessageGateway {
    /// Create a new MessageGateway.
    pub fn new(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<crate::storage::repositories::channel_repo::ChannelRepository>,
        user_repo: Arc<crate::storage::repositories::UserRepository>,
        settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            channel_repo,
            user_repo,
            settings_repo,
            sesame_manager: None,
            ephemeral_repo: None,
        }
    }

    /// Create a new MessageGateway with Sesame session management.
    pub fn with_sesame(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<crate::storage::repositories::channel_repo::ChannelRepository>,
        user_repo: Arc<crate::storage::repositories::UserRepository>,
        settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        sesame_manager: Arc<SesameManager<SqliteSesameStore>>,
        ephemeral_repo: Arc<crate::storage::repositories::ephemeral_repo::EphemeralRepository>,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            user_repo,
            settings_repo,
            channel_repo,
            sesame_manager: Some(sesame_manager),
            ephemeral_repo: Some(ephemeral_repo),
        }
    }

    /// Store a DataMessage to the repository.
    pub(crate) async fn store_data_message(
        &self,
        data_message: DataMessage,
        sender_id: Option<UserId>,
    ) -> Result<SealedSenderResult, crate::error::ServiceError> {
        use crate::error::ServiceError;

        // An empty channel_id means this is a channel-free DM (Signal-style
        // — no server DM channel). Bucket it by the sender's user_id so the
        // receiver's local conversation_id matches "the other party", same
        // as the sender's own outgoing row (keyed by the recipient peer_id).
        // A non-empty channel_id is a group/sender-key message.
        let (channel_id, conversation_type): (ChannelId, ConversationType) =
            if data_message.channel_id.is_empty() {
                let sid = sender_id.ok_or_else(|| {
                    ServiceError::Validation(
                        "DM DataMessage has no sender_id — cannot bucket a channel-free DM".into(),
                    )
                })?;
                // Signal-style: the sender piggybacks their profile key on the
                // DataMessage. Persist it so their encrypted display name +
                // avatar can be decrypted (list_friends_with_profiles). Without
                // this the peer only ever renders the "User <id>" fallback.
                if let Some(key) = data_message.profile_key.as_ref() {
                    if key.len() == 32 {
                        if let Ok(current_user) = self.settings_repo.current_user_id() {
                            let key_array: [u8; 32] = key.as_slice().try_into().expect("len checked == 32");
                            if let Err(e) =
                                self.user_repo.set_friend_profile_key(&current_user, &sid, &key_array)
                            {
                                tracing::debug!(sender_id = %sid, error = %e, "DM: failed to store sender profile key");
                            }
                        }
                    }
                }

                // Pure profile-key handshake carrier (`send_profile_key_handshake`,
                // fired at friend-accept time): no text, no attachments, and
                // not a delete-for-everyone (which also rides an empty-body
                // DataMessage — `data_message.delete` — on the ServerDm path).
                // The profile key above was the whole point. Don't create a
                // message row or surface a bubble; the caller refreshes the
                // sender's profile instead.
                if data_message.body.is_empty()
                    && data_message.attachments.is_empty()
                    && data_message.delete.is_none()
                {
                    return Ok(SealedSenderResult::ProfileKeyReceived { sender_id: sid });
                }

                (ChannelId::from(sid), ConversationType::ServerDm)
            } else {
                let cid: ChannelId = data_message.channel_id.parse().map_err(|e| {
                    ServiceError::Validation(format!("Invalid channel_id: {}", e))
                })?;
                (cid, ConversationType::AnonGroup)
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

        // Create domain message with sender's timestamp for Signal-style dedup.
        // Reply fields are filled by the shared inbound-quote helper below.
        let mut message = Message {
            id: message_id,
            conversation_id: channel_id,
            conversation_type,
            sender_id,
            device_id: None,
            content,
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
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

        // Reply/quote context (shared with the MailboxDm inbound path).
        crate::services::gateway::inbound::apply_inbound_quote(
            &mut message,
            data_message.quote.as_ref(),
            &self.message_repo,
        );

        // Store in repository
        self.message_repo.insert(&message)?;

        // Disappearing messages — RECIPIENT local half. When the inbound
        // DataMessage carries an armed ephemeral marker (V29 wire fields set by
        // the sender), track the RECIPIENT's local copy so the periodic reaper
        // removes it once the timer elapses — Signal's whole point is that the
        // message vanishes from *both* devices, not just the server. Timer
        // starts at receive time (send-time-start semantics; network latency is
        // negligible against a disappearing window). Best-effort: a tracking
        // failure must not drop an otherwise-good message.
        if data_message.ephemeral && data_message.ephemeral_ttl_seconds > 0 {
            if let Some(ephemeral_repo) = self.ephemeral_repo.as_ref() {
                if let Err(e) =
                    ephemeral_repo.insert_armed(&message.id, data_message.ephemeral_ttl_seconds)
                {
                    tracing::warn!(
                        message_id = %message.id,
                        error = %e,
                        "failed to arm local disappearing timer for inbound message"
                    );
                } else {
                    tracing::info!(
                        message_id = %message.id,
                        ttl_seconds = data_message.ephemeral_ttl_seconds,
                        "armed local disappearing timer for inbound message"
                    );
                }
            }
        }

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

    /// Store a self-synced `SyncSent` (a copy of a message WE sent, delivered to
    /// our own other devices) as an OUTGOING message in the conversation with the
    /// real recipient.
    ///
    /// This is the receive-side half of the F8 fix. Unlike `store_data_message`,
    /// which buckets a channel-free DM by the *sender's* id, this buckets by the
    /// `SyncSent.destination_user_id` and attributes the message to *us* (the
    /// current user) — so the message shows up on our linked device as our own
    /// outgoing message to the recipient, not as an inbound note-to-self.
    ///
    /// `sealed_sender_id` is the identity the sealed-sender certificate carried;
    /// a self-sync is by definition sent by us, so it MUST equal the current
    /// user. Anything else is a forged/misrouted sync and is rejected — no other
    /// account can legitimately place an outgoing message in our transcript.
    pub(crate) async fn store_synced_sent_message(
        &self,
        sync: crate::proto::content_v1::SyncSent,
        sealed_sender_id: UserId,
    ) -> Result<SealedSenderResult, crate::error::ServiceError> {
        use crate::error::ServiceError;

        let current_user = self.settings_repo.current_user_id().map_err(|e| {
            ServiceError::Validation(format!("SyncSent received but no current user: {e}"))
        })?;
        if sealed_sender_id != current_user {
            return Err(ServiceError::Validation(format!(
                "SyncSent sealed-sender identity {sealed_sender_id} != current user \
                 {current_user}: a self-sync must be sent by us — rejecting"
            )));
        }

        let destination: UserId = sync.destination_user_id.parse().map_err(|e| {
            ServiceError::Validation(format!("SyncSent has invalid destination_user_id: {e}"))
        })?;

        let data_message = sync.message.ok_or_else(|| {
            ServiceError::Validation("SyncSent carried no inner message".into())
        })?;

        // Empty-body carriers (profile-key handshake, delete-for-everyone) are not
        // wrapped on the send side, so a wrapped copy is always a real message.
        // Guard defensively anyway: nothing outgoing to surface.
        if data_message.body.is_empty() && data_message.attachments.is_empty() {
            return Ok(SealedSenderResult::MessageStored);
        }

        let channel_id = ChannelId::from(destination);
        let message_id = MessageId::new();

        let created_at = if data_message.timestamp > 0 {
            TimestampNanos::from_nanos(data_message.timestamp)
                .to_datetime()
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };

        let attachments: Vec<crate::domain::ids::AttachmentId> = data_message
            .attachments
            .iter()
            .filter(|ptr| !ptr.cdn_key.is_empty())
            .map(|ptr| crate::domain::ids::AttachmentId::from_cdn_key(&ptr.cdn_key))
            .collect();

        let mut message = Message {
            id: message_id,
            conversation_id: channel_id,
            conversation_type: ConversationType::ServerDm,
            // Attributed to US — this is our outgoing message, synced from our
            // other device.
            sender_id: Some(current_user),
            device_id: None,
            content: MessageContent::Text(data_message.body.clone()),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
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

        crate::services::gateway::inbound::apply_inbound_quote(
            &mut message,
            data_message.quote.as_ref(),
            &self.message_repo,
        );

        self.message_repo.insert(&message)?;

        tracing::info!(
            message_id = %message.id,
            channel_id = %message.conversation_id,
            destination = %destination,
            "Stored self-synced outgoing message (SyncSent) in conversation with recipient"
        );

        Ok(SealedSenderResult::DataMessageReceived {
            sender_id: current_user,
            channel_id,
            message_id,
            body: data_message.body,
            timestamp: data_message.timestamp,
            attachments: data_message.attachments,
        })
    }

    /// Get the local identity private key for ECIES decryption.
    ///
    /// Returns the 32-byte X25519 private scalar used to decrypt
    /// NDSS'21 initial contact messages.
    pub async fn get_identity_key(&self) -> Result<[u8; 32], crate::error::ServiceError> {
        self.signal_service.get_identity_private_key().await
    }
}
