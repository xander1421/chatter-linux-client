//! Message gateway module.
//!
//! Central message routing and validation pipeline.
//! Decrypts sealed sender messages, parses DataMessage proto, and stores to repository.

pub mod block_filter;
pub mod builders;
mod decryption;
pub mod inbound;
pub mod outgoing;
mod routing;
pub mod shared;

use std::sync::Arc;

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{ConversationType, Message, MessageContent};
use crate::proto::content_v1::DataMessage;
use crate::services::SignalService;
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::MessageRepository;

// Re-export inbound types (moved from inline definitions)
use crate::util::time::TimestampNanos;
pub use inbound::{DecryptResult, DmContent, RouteResult, SealedSenderResult, content_type_name};

// Re-export all content builders
pub use builders::{
    PADDING_GROUP_CONTROL,
    PADDING_LARGE,
    PADDING_MESSAGE,
    // Call hangup
    build_call_hangup_content,
    build_call_key_answer,
    // DM call key exchange (ephemeral keys for PFS)
    build_call_key_offer,
    build_call_self_sync,
    build_data_message_content,
    build_data_message_content_with_quote,
    build_delete_for_me_content,
    build_delivery_receipt_content,
    build_delivery_receipt_content_with_channel,
    build_edit_content,
    build_edit_content_with_channel,
    // Group call key distribution (pairwise sealed sender)
    build_group_call_signal_content,
    build_pin_content,
    build_pin_content_with_channel,
    build_reaction_content,
    build_reaction_content_with_channel,
    build_read_receipt_content,
    build_read_receipt_content_with_channel,
    build_reply_content,
    build_saved_message_sync_content,
    // DataMessage builders
    build_targeted_delete_content,
    build_text_content,
    // Control message builders
    build_typing_content,
    build_typing_content_with_channel,
    // Padding
    generate_padding,
};

// Re-export outgoing router
pub use outgoing::{
    OutgoingIntent, OutgoingServices, OwnedOutgoingServices, QuoteIntent, SendError, SendResult,
    route_outgoing, route_outgoing_owned,
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
    /// One of OUR other devices answered this call (own-user sync only).
    AcceptedElsewhere,
    /// One of OUR other devices declined this call (own-user sync only).
    DeclinedElsewhere,
}

impl std::fmt::Display for CallHangupReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
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
    /// Local disappearing-message TTL tracking. Set on the production gateway
    /// so an INBOUND message that arrives with an armed `ephemeral_ttl`
    /// (V29 wire marker) tracks the RECIPIENT's local copy for reaping. `None`
    /// on gateways built without it (the plain `new` constructor / some tests)
    /// — arming is simply skipped there.
    pub(crate) ephemeral_repo:
        Option<Arc<crate::storage::repositories::ephemeral_repo::EphemeralRepository>>,
    /// Shared with `MessageService`: the group resends this device asked for.
    pub(crate) solicited_resends: crate::services::message::SolicitedResends,
}

impl MessageGateway {
    /// Create a MessageGateway without disappearing-message timers (tests).
    #[cfg(test)]
    pub(crate) fn new(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<crate::storage::repositories::channel_repo::ChannelRepository>,
        user_repo: Arc<crate::storage::repositories::UserRepository>,
        settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        solicited_resends: crate::services::message::SolicitedResends,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            channel_repo,
            user_repo,
            settings_repo,
            ephemeral_repo: None,
            solicited_resends,
        }
    }

    /// Create the production MessageGateway, which also arms disappearing
    /// timers on inbound messages.
    pub(crate) fn with_ephemeral(
        signal_service: SignalService,
        message_repo: Arc<MessageRepository>,
        channel_repo: Arc<crate::storage::repositories::channel_repo::ChannelRepository>,
        user_repo: Arc<crate::storage::repositories::UserRepository>,
        settings_repo: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        ephemeral_repo: Arc<crate::storage::repositories::ephemeral_repo::EphemeralRepository>,
        solicited_resends: crate::services::message::SolicitedResends,
    ) -> Self {
        Self {
            signal_service,
            message_repo,
            user_repo,
            settings_repo,
            channel_repo,
            ephemeral_repo: Some(ephemeral_repo),
            solicited_resends,
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
        // Set when this message brought a profile key we did not have (or a
        // rotated one): the caller then refreshes the sender's profile, as the
        // pure handshake below does, so their real name replaces the fallback
        // without a restart.
        let mut profile_key_changed = false;
        let (channel_id, conversation_type): (ChannelId, ConversationType) = if data_message
            .channel_id
            .is_empty()
        {
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
                        let key_array: [u8; 32] =
                            key.as_slice().try_into().expect("len checked == 32");
                        match self
                            .user_repo
                            .set_friend_profile_key(&current_user, &sid, &key_array)
                        {
                            Ok(changed) => profile_key_changed = changed,
                            Err(e) => {
                                tracing::debug!(sender_id = %sid, error = %e, "DM: failed to store sender profile key");
                            }
                        }
                    }
                }
            }

            // Disappearing-messages timer (Signal's expireTimer): a timer
            // update announces it, and every visible message restates it
            // with its version; adopt it by Signal's version rule
            // (`services::disappearing`). An update message has
            // nothing else to show: surface the notice in its place.
            let timer_update = crate::services::disappearing::is_timer_update(&data_message);
            let visible = !data_message.body.is_empty() || !data_message.attachments.is_empty();
            if data_message.delete.is_none() && (timer_update || visible) {
                let notice = self.adopt_server_dm_timer(
                    &sid,
                    &sid,
                    crate::services::disappearing::stated_timer(&data_message),
                    data_message.timestamp,
                );
                if timer_update {
                    return Ok(match notice {
                        Some(notice) => SealedSenderResult::DataMessageReceived {
                            sender_id: sid,
                            channel_id: notice.conversation_id,
                            message_id: notice.id,
                            body: String::new(),
                            timestamp: data_message.timestamp,
                            attachments: vec![],
                            profile_key_changed,
                        },
                        // Already our timer (a redelivery): nothing new.
                        None => SealedSenderResult::MessageStored,
                    });
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
            let cid: ChannelId = data_message
                .channel_id
                .parse()
                .map_err(|e| ServiceError::Validation(format!("Invalid channel_id: {}", e)))?;
            // A server channel (sender-key or not) is filed as Channel;
            // anon groups never arrive on this path (they use the mailbox).
            let conversation_type =
                crate::services::message::conversion::conversation_type_for_channel(
                    &self.channel_repo,
                    &cid,
                );
            (cid, conversation_type)
        };

        // A sender-key resend (`rebuild_group_content`) carries the server id
        // the other members hold, so thread replies and later references by
        // id reach the recovered message. The id is honoured only for a resend
        // this device asked THIS sender for, at this sent_at (the request is
        // consumed): otherwise any member could squat another member's id
        // before its real copy lands. Everything else gets a local id. A
        // claimed id already in use fails the insert below rather than
        // replacing that row.
        let solicited = conversation_type != ConversationType::ServerDm
            && sender_id.is_some_and(|sid| {
                self.solicited_resends
                    .take(&(channel_id, sid, data_message.timestamp / 1_000_000))
            });
        let message_id = solicited
            .then(|| data_message.message_id.parse().ok())
            .flatten()
            .unwrap_or_else(MessageId::new);

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
            // Thread root rides on the wire (DataMessage.thread_id). Mailbox
            // delivery never sees the server's parent_id, so without this a
            // thread reply would be filed into the main channel.
            thread_id: data_message.thread_id.parse().ok(),
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
            failure_reason: None,
        };

        // Reply/quote context (shared with the mailbox inbound path).
        crate::services::gateway::inbound::apply_inbound_quote(
            &mut message,
            data_message.quote.as_ref(),
            &self.message_repo,
        );

        // Already held (a sender-key resend answering a second DecryptionError,
        // e.g. after a restart, or a re-delivery): the unique
        // (conversation, sender, sent_at) row exists, so the insert would fail
        // and surface as a delivery issue. Nothing new to show.
        if let Some(sid) = sender_id {
            if let Ok(Some(existing)) = self
                .message_repo
                .get_by_sender_timestamp(&sid, message.sent_at)
            {
                if existing.conversation_id == message.conversation_id {
                    return Ok(SealedSenderResult::MessageStored);
                }
            }
            // Held once and deleted here since (for me, expiry, the chat, or
            // for everyone): a second copy of it stays deleted. A storage
            // error is not swallowed into storing it again.
            if self.message_repo.is_dm_receipt_tombstoned(
                &message.conversation_id,
                &sid,
                message.sent_at,
            )? {
                return Ok(SealedSenderResult::MessageStored);
            }
        }

        // Store in repository. What is announced is what was stored: an edit
        // that arrived first is already applied, and a message its author
        // deleted for everyone before it got here is not stored at all.
        let Some(stored) = self.message_repo.insert_received(&message)? else {
            tracing::info!(message_id = %message.id, "Inbound message was deleted for everyone before it arrived");
            return Ok(SealedSenderResult::MessageStored);
        };

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
            body: announced_body(&stored, data_message.body),
            timestamp: data_message.timestamp,
            attachments: data_message.attachments,
            profile_key_changed,
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

        // NOT `Validation`. This is a settings-DB read: a failure here is our
        // disk or a login that has not happened yet, and the inbox ack policy
        // reads `Validation` as "the bytes are bad, delete it". Laundering the
        // cause into a string deleted a linked device's sync copy whenever the
        // database was momentarily unreadable.
        let current_user: UserId = self.settings_repo.current_user_id()?;
        if sealed_sender_id != current_user {
            return Err(ServiceError::Validation(format!(
                "SyncSent sealed-sender identity {sealed_sender_id} != current user \
                 {current_user}: a self-sync must be sent by us — rejecting"
            )));
        }

        // A channel send is already delivered to linked devices by the
        // sender-key stream. Its own-device SyncSent carries only the original
        // receipt targets; never re-file it as a DM or display a second bubble.
        if let Some(message) = sync.message.as_ref().filter(|message| !message.channel_id.is_empty()) {
            if !sync.destination_user_id.is_empty() || message.timestamp != sync.timestamp || sync.timestamp == 0 {
                return Err(ServiceError::Validation("invalid channel receipt transcript".into()));
            }
            let channel: ChannelId = message.channel_id.parse().map_err(|_| {
                ServiceError::Validation("invalid channel receipt transcript scope".into())
            })?;
            let targets = sync.receipt_targets.iter().map(|target| {
                let user: UserId = target.user_id.parse().map_err(|_| {
                    ServiceError::Validation("invalid channel receipt target".into())
                })?;
                if user == current_user || target.device_id == 0 || target.device_id > 127 {
                    return Err(ServiceError::Validation("invalid channel receipt target".into()));
                }
                Ok((user, crate::domain::ids::DeviceId(target.device_id)))
            }).collect::<Result<Vec<_>, ServiceError>>()?;
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.settings_repo.database().clone(),
            ).snapshot_for_send(
                channel,
                ConversationType::Channel,
                current_user,
                sync.timestamp,
                &targets,
            )?;
            return Ok(SealedSenderResult::ChannelReceiptTargetsSynced {
                channel_id: channel,
                timestamp: sync.timestamp,
            });
        }

        let destination: UserId = sync.destination_user_id.parse().map_err(|e| {
            ServiceError::Validation(format!("SyncSent has invalid destination_user_id: {e}"))
        })?;

        let targets = sync
            .receipt_targets
            .iter()
            .map(|target| {
                let user: UserId = target.user_id.parse().map_err(|_| {
                    ServiceError::Validation("SyncSent has invalid receipt recipient".into())
                })?;
                // `device_id` in 1..=127 is a content.proto rule, enforced at decode.
                if user != destination || user == current_user {
                    return Err(ServiceError::Validation(
                        "SyncSent has invalid receipt target".into(),
                    ));
                }
                Ok((user, crate::domain::ids::DeviceId(target.device_id)))
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;

        let data_message = sync
            .message
            .ok_or_else(|| ServiceError::Validation("SyncSent carried no inner message".into()))?;

        // Our own timer change (or a message restating our timer) made on
        // another device: adopt it for the conversation with the recipient,
        // attributed to us. A timer update has nothing else to show.
        let timer_update = crate::services::disappearing::is_timer_update(&data_message);
        let notice = self.adopt_server_dm_timer(
            &destination,
            &current_user,
            crate::services::disappearing::stated_timer(&data_message),
            data_message.timestamp,
        );
        if timer_update {
            return Ok(match notice {
                Some(notice) => SealedSenderResult::DataMessageReceived {
                    sender_id: current_user,
                    channel_id: notice.conversation_id,
                    message_id: notice.id,
                    body: String::new(),
                    timestamp: data_message.timestamp,
                    attachments: vec![],
                    profile_key_changed: false,
                },
                None => SealedSenderResult::MessageStored,
            });
        }

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
            // Thread root rides on the wire (DataMessage.thread_id). Mailbox
            // delivery never sees the server's parent_id, so without this a
            // thread reply would be filed into the main channel.
            thread_id: data_message.thread_id.parse().ok(),
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
            delivery_status: "sent".into(),
            failure_reason: None,
        };

        crate::services::gateway::inbound::apply_inbound_quote(
            &mut message,
            data_message.quote.as_ref(),
            &self.message_repo,
        );

        // Already held: a resend after our DecryptionErrorMessage, or a sync a
        // device-set retry queued twice. The unique (conversation, sender,
        // sent_at) row exists, so the insert would fail into a delivery issue.
        if let Ok(Some(existing)) = self
            .message_repo
            .get_by_sender_timestamp(&current_user, message.sent_at)
        {
            if existing.conversation_id == message.conversation_id {
                return Ok(SealedSenderResult::MessageStored);
            }
        }
        // Our own copy, deleted here since: a second sync of it stays deleted.
        if self.message_repo.is_dm_receipt_tombstoned(
            &message.conversation_id,
            &current_user,
            message.sent_at,
        )? {
            return Ok(SealedSenderResult::MessageStored);
        }

        crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
            self.settings_repo.database().clone(),
        )
        .snapshot_for_send(
            channel_id,
            ConversationType::ServerDm,
            current_user,
            data_message.timestamp,
            &targets,
        )?;

        // As for an inbound message: announce what was stored.
        let Some(stored) = self.message_repo.insert_received(&message)? else {
            tracing::info!(message_id = %message.id, "Synced sent message was deleted for everyone before it arrived");
            return Ok(SealedSenderResult::MessageStored);
        };

        // Our copy on this device disappears on the same timer as the one we
        // sent from.
        let ttl = crate::services::disappearing::message_ttl(&data_message);
        if ttl > 0 {
            self.arm_disappearing(&message.id, ttl);
        }

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
            body: announced_body(&stored, data_message.body),
            timestamp: data_message.timestamp,
            attachments: data_message.attachments,
            profile_key_changed: false,
        })
    }

    /// Adopt a friend DM's disappearing timer as stated by a received message
    /// (see `services::disappearing`). Returns the notice when the timer
    /// changed. Best-effort: a storage failure is logged, never fatal to the
    /// message that carried it.
    pub(crate) fn adopt_server_dm_timer(
        &self,
        peer_id: &UserId,
        author: &UserId,
        stated: Option<crate::services::disappearing::DmTimer>,
        sent_at: u64,
    ) -> Option<Message> {
        // A message stating no timer (version 0) says nothing about it.
        let stated = stated?;
        match crate::services::disappearing::adopt_server_dm_timer(
            &self.settings_repo,
            &self.message_repo,
            peer_id,
            author,
            stated,
            sent_at,
        ) {
            Ok(notice) => {
                if notice.is_some() {
                    tracing::info!(
                        peer_id = %peer_id,
                        seconds = stated.seconds,
                        version = stated.version,
                        "adopted a new disappearing timer"
                    );
                }
                notice
            }
            Err(e) => {
                tracing::warn!(peer_id = %peer_id, error = %e, "failed to adopt disappearing timer");
                None
            }
        }
    }

    /// Track a received message's local copy for reaping `ttl_seconds` from
    /// now. Best-effort, like the other arming paths.
    pub(crate) fn arm_disappearing(&self, message_id: &MessageId, ttl_seconds: u32) {
        let Some(ephemeral_repo) = self.ephemeral_repo.as_ref() else {
            return;
        };
        if let Err(e) = ephemeral_repo.insert_armed(message_id, ttl_seconds) {
            tracing::warn!(
                message_id = %message_id,
                error = %e,
                "failed to arm local disappearing timer for inbound message"
            );
        }
    }

    /// Open an NDSS'21 initial contact message sealed to our identity key.
    /// The open runs inside the Signal actor; no identity secret crosses here.
    pub async fn open_anon_dm_init(
        &self,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, crate::error::ServiceError> {
        self.signal_service
            .open_identity_hpke(
                crate::crypto::ecies::IdentityHpkeUse::AnonDmInit,
                ciphertext.to_vec(),
            )
            .await
    }
}

/// The body to announce for a message just stored: the stored text (an early
/// edit already applied), else what was decoded.
fn announced_body(stored: &crate::domain::message::Message, decoded: String) -> String {
    match &stored.content {
        crate::domain::message::MessageContent::Text(text) => text.clone(),
        _ => decoded,
    }
}

pub(crate) mod submission;
