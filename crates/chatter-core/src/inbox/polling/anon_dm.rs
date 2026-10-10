//! Anonymous DM mailbox polling.
//!
//! Processes messages from anonymous DM mailboxes and emits InboxEvents.
//! Anonymous DM mailboxes are used for stranger messaging - they're polled
//! separately from the device inbox.
//!
//! Message flow:
//! 1. Request: Sent to device inbox → processed via dm.rs → InboxEvent::AnonDmRequest
//! 2. Accept: Sent to requester's anon mailbox → processed here → InboxEvent::AnonDmAccept
//! 3. Messages: Sent to peer's anon mailbox → processed here → InboxEvent::AnonDmMessage
//!
//! Note: This module handles messages from anonymous DM mailboxes (polled via AnonDmPoller).
//! The initial request/accept flow goes through the device inbox and is handled by dm.rs.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use prost::Message as ProstMessage;
use tokio::sync::mpsc;

use crate::domain::UserId;
use crate::domain::anon_dm::AnonDmId;
use crate::domain::ids::ChannelId;
use crate::domain::message::ConversationType;
use crate::inbox::ack_policy::{self, AckDecision, MarkerTarget};
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::anon_dm::AnonDmPollResult;
use crate::network::validation::MessageDeduplicator;
use crate::proto::content_v1::{self, Content};
use crate::services::pairwise::PairwiseService;
use crate::storage::MessageRepository;

/// Decide what to do with an anonymous-DM envelope whose decrypt or decode
/// failed.
///
/// This loop's bug is the mirror image of the other three: it `continue`s
/// without adding the id to the ack batch, so a peer on the old v2 envelope
/// format redelivers something that can never decrypt, forever.
pub(crate) fn anon_dm_failure_decision(
    conversation_id: AnonDmId,
    envelope_ts_ns: u64,
    reason: &str,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
) -> AckDecision {
    // Every failure this loop can reach is deterministic in the bytes: the
    // sealed-sender open failed, the sender uuid would not parse, the Content
    // proto would not decode, or an `AnonDmMessage` arrived with no
    // `inner_content`. None of them depend on local state that a later poll
    // could supply, so all of them are permanent — which is why retaining them
    // produced the infinite redelivery.
    ack_policy::decide_after_malformed(
        reason,
        MarkerTarget {
            conversation_id: ChannelId::from(conversation_id),
            conversation_type: ConversationType::AnonDm,
            // Sealed sender failed, so nobody was authenticated. The entry is
            // written anyway; it just cannot name a sender.
            peer_id: None,
            envelope_ts_ns,
        },
        message_repo,
        domain_tx,
    )
}

/// What the loop must do with a decrypted, decoded anon-DM `Content`.
///
/// Extracted from the routing match so the decision is testable without a
/// `PairwiseService`, a mailbox client, or a real sealed-sender envelope.
#[derive(Debug)]
pub(crate) enum AnonDmRoute {
    /// Emit these events, then drain the envelope.
    Deliver(Vec<InboxEvent>),
    /// Nothing to deliver, and nothing a later poll can change: the failure is
    /// a pure function of the bytes. Marker, then drain.
    Malformed(&'static str),
}

/// Route a decoded anon-DM `Content` to the events it produces.
///
/// Pure: takes the decrypted content and the authenticated sender, returns the
/// events the loop should emit. No I/O, so the whole routing table can be
/// tested directly.
pub(crate) fn route_anon_dm_content(
    content: Content,
    sender_id: UserId,
    // The sealed-sender-authenticated device it came from.
    sender_device_id: u32,
    conversation_id: AnonDmId,
) -> AnonDmRoute {
    // Kept for the envelopes that are re-wrapped into a fresh `Content` below.
    let outer_timestamp = content.timestamp;
    let outer_ephemeral_reply_to = content.ephemeral_reply_to;
    let outer_padding = content.padding;
    let conversation_bytes = conversation_id.as_bytes().to_vec();

    let rewrap = |inner: content_v1::content::Content| InboxEvent::AnonDmMessage {
        peer_id: sender_id,
        conversation_id: conversation_bytes.clone(),
        inner_content: Box::new(Content {
            content: Some(inner),
            timestamp: outer_timestamp,
            ephemeral_reply_to: outer_ephemeral_reply_to,
            padding: outer_padding.clone(),
        }),
    };

    match content.content {
        Some(content_v1::content::Content::AnonDmAccept(accept)) => {
            tracing::info!(%conversation_id, "Received secret chat acceptance");

            AnonDmRoute::Deliver(vec![InboxEvent::AnonDmAccept {
                peer_id: sender_id,
                sender_device_id,
                conversation_id: conversation_bytes,
                responder_mailbox_pk: accept.responder_mailbox_pk,
                profile_key: accept.profile_key,
                encrypted_display_name: accept.encrypted_display_name,
                encrypted_avatar: accept.encrypted_avatar,
                timestamp: accept.timestamp,
            }])
        }
        Some(content_v1::content::Content::AnonDmMessage(dm_msg)) => {
            tracing::info!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                has_inner = dm_msg.inner_content.is_some(),
                "Received anonymous DM message from mailbox poll"
            );

            // No inner content is not "wait for more" — the field is simply
            // absent from the bytes, so every redelivery reproduces it. The
            // pre-fix loop returned `was_processed = false` here: no ack, no
            // marker, no dedup entry, which meant a single crafted envelope
            // was re-fetched and re-decrypted on every poll forever. Anon-DM
            // mailboxes take mail from strangers, so that loop was one
            // unauthenticated message away.
            let Some(inner) = dm_msg.inner_content else {
                return AnonDmRoute::Malformed("AnonDmMessage carried no inner_content");
            };

            // Check if the inner content is an anon group management message
            match inner.content.as_ref() {
                Some(content_v1::content::Content::AnonGroupInvite(invite)) => {
                    tracing::info!("Received secret group invite inside a secret chat");
                    AnonDmRoute::Deliver(vec![InboxEvent::AnonGroupInvite {
                        peer_id: sender_id,
                        sender_device_id,
                        group_id: invite.group_id.clone(),
                        group_name: invite.group_name.clone(),
                        inviter_mailbox_pk: invite.inviter_mailbox_pk.clone(),
                        timestamp: invite.timestamp,
                        profile_key: invite.profile_key.clone(),
                        encrypted_display_name: invite.encrypted_display_name.clone(),
                        encrypted_avatar: invite.encrypted_avatar.clone(),
                    }])
                }
                Some(content_v1::content::Content::AnonGroupJoinAck(ack)) => {
                    tracing::info!(
                        has_profile = ack.profile_key.is_some(),
                        "Received secret group join ack inside a secret chat"
                    );
                    AnonDmRoute::Deliver(vec![InboxEvent::AnonGroupJoinAck {
                        peer_id: sender_id,
                        sender_device_id,
                        group_id: ack.group_id.clone(),
                        joiner_mailbox_pk: ack.joiner_mailbox_pk.clone(),
                        timestamp: ack.timestamp,
                        profile_key: ack.profile_key.clone(),
                        encrypted_display_name: ack.encrypted_display_name.clone(),
                        encrypted_avatar: ack.encrypted_avatar.clone(),
                    }])
                }
                Some(content_v1::content::Content::AnonGroupMemberList(list)) => {
                    tracing::info!(
                        sender_id = %sender_id,
                        group_id = %hex::encode(&list.group_id),
                        "Received anonymous group member list inside anon DM"
                    );
                    AnonDmRoute::Deliver(vec![InboxEvent::AnonGroupMemberList {
                        peer_id: sender_id,
                        group_id: list.group_id.clone(),
                        members: list.members.clone(),
                        timestamp: list.timestamp,
                    }])
                }
                _ => {
                    // Default handling for other messages
                    AnonDmRoute::Deliver(vec![InboxEvent::AnonDmMessage {
                        peer_id: sender_id,
                        conversation_id: conversation_bytes,
                        inner_content: inner,
                    }])
                }
            }
        }
        Some(content_v1::content::Content::Receipt(receipt)) => {
            // Handle read/delivery receipts
            tracing::info!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                receipt_type = receipt.r#type,
                timestamp_count = receipt.timestamps.len(),
                timestamps = ?receipt.timestamps,
                "[ANON_DM_RECEIPT] Received receipt from peer"
            );
            AnonDmRoute::Deliver(vec![rewrap(content_v1::content::Content::Receipt(receipt))])
        }
        Some(content_v1::content::Content::Typing(typing)) => {
            // Handle typing indicators
            tracing::debug!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                "[ANON_DM] Received typing indicator from peer"
            );
            AnonDmRoute::Deliver(vec![rewrap(content_v1::content::Content::Typing(typing))])
        }
        Some(content_v1::content::Content::AnonDmPresence(presence)) => {
            // Handle presence updates
            tracing::debug!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                status = ?presence.status,
                "[ANON_DM] Received presence update from peer"
            );
            AnonDmRoute::Deliver(vec![rewrap(content_v1::content::Content::AnonDmPresence(
                presence,
            ))])
        }
        Some(content_v1::content::Content::AnonDmProfileUpdate(profile_update)) => {
            // Handle profile updates
            tracing::info!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                has_encrypted_display_name = profile_update.encrypted_display_name.is_some(),
                has_avatar = profile_update.encrypted_avatar.is_some(),
                has_avatar_url = profile_update.avatar_url.is_some(),
                has_bio = profile_update.bio.is_some(),
                "[ANON_DM] Received profile update from peer"
            );
            AnonDmRoute::Deliver(vec![rewrap(
                content_v1::content::Content::AnonDmProfileUpdate(profile_update),
            )])
        }
        other => {
            let content_type = match &other {
                Some(content_v1::content::Content::DataMessage(_)) => "DataMessage",
                Some(content_v1::content::Content::Reaction(_)) => "Reaction",
                Some(content_v1::content::Content::AnonDmRequest(_)) => "AnonDmRequest",
                None => "None",
                _ => "Other",
            };
            tracing::warn!(
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                content_type = content_type,
                "[ANON_DM] Unexpected content type in anonymous DM mailbox"
            );
            // Nothing to emit, but nothing a later poll changes either — drain.
            AnonDmRoute::Deliver(Vec::new())
        }
    }
}

/// Result of processing anonymous DM poll results.
/// Maps conversation_id -> list of processed message IDs for acknowledgment.
pub type ProcessedAnonDmMessages = HashMap<AnonDmId, Vec<String>>;

/// Process anonymous DM poll results and emit events.
///
/// Takes the raw poll results from AnonDmPoller, decrypts each message
/// using the pairwise service, and emits the appropriate InboxEvent.
///
/// Returns a map of conversation_id -> processed message IDs for acknowledgment.
///
/// # Arguments
///
/// * `results` - Poll results from AnonDmPoller
/// * `pairwise_service` - Service for sealed sender decryption
/// * `event_tx` - Channel to send InboxEvents
/// * `deduplicator` - Deduplicator to prevent re-processing messages when ack fails
pub async fn process_anon_dm_results(
    results: Vec<AnonDmPollResult>,
    pairwise_service: &Arc<PairwiseService>,
    event_tx: &mpsc::Sender<InboxEvent>,
    deduplicator: &MessageDeduplicator,
    current_user_id: Option<&UserId>,
    is_blocked: &(dyn Fn(&UserId, &AnonDmId) -> bool + Send + Sync),
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
) -> ProcessedAnonDmMessages {
    let mut processed: ProcessedAnonDmMessages = HashMap::new();
    crate::inbox::reaction_handoff::replay(message_repo, domain_tx);

    for result in results {
        let conversation_id = result.conversation_id;

        for msg in result.messages {
            let message_id = msg.message_id.clone();

            // Check if we've already processed this message ID.
            // This prevents "old counter" errors when acknowledgment fails
            // and the server re-delivers the same message.
            // Read-only: marking here would mean a message we chose to retain
            // is skipped on its redelivery and never retried.
            if deduplicator.is_duplicate(&message_id) {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    message_id = %message_id,
                    "Anon DM message already processed, skipping decryption"
                );
                // Still add to processed list so we acknowledge it again
                processed
                    .entry(conversation_id)
                    .or_default()
                    .push(message_id);
                continue;
            }

            let key = match message_repo.reaction_receive_key(
                "anon_dm",
                conversation_id.into(),
                &message_id,
                msg.server_timestamp,
            ) {
                Ok(key) => key,
                Err(_) => {
                    tracing::warn!("Anonymous receive binding unavailable; envelope retained");
                    continue;
                }
            };
            // Decrypt using pairwise service (sealed sender)
            let (sender_uuid, sender_device_id, plaintext) = match pairwise_service
                .decrypt_reaction_durable(&msg.encrypted_envelope, &key)
                .await
            {
                Ok(received) => {
                    if let Some(artifact) = received.artifact {
                        if crate::inbox::reaction_handoff::apply(message_repo, &artifact, domain_tx)
                            .is_ok()
                        {
                            deduplicator.mark_processed(&message_id);
                            processed
                                .entry(conversation_id)
                                .or_default()
                                .push(message_id);
                        } else {
                            tracing::warn!(
                                "Anonymous reaction application failed; envelope retained"
                            );
                        }
                        continue;
                    }
                    (
                        received.address.name().to_string(),
                        u32::from(received.address.device_id()),
                        received.content,
                    )
                }
                Err(crate::error::ServiceError::Storage(_))
                | Err(crate::error::ServiceError::Crypto(crate::error::CryptoError::Storage(_))) => {
                    tracing::warn!("Atomic anonymous receive storage failed; envelope retained");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        message_id = %msg.message_id,
                        error = %e,
                        "Failed to decrypt anonymous DM message"
                    );
                    // Was: `continue` with no ack, so an old-build peer on the
                    // v2 envelope format redelivered this forever.
                    if anon_dm_failure_decision(
                        conversation_id,
                        msg.server_timestamp,
                        &format!("sealed sender decrypt failed: {e}"),
                        message_repo,
                        domain_tx,
                    )
                    .should_ack()
                    {
                        deduplicator.mark_processed(&message_id);
                        processed
                            .entry(conversation_id)
                            .or_default()
                            .push(message_id);
                    }
                    continue;
                }
            };

            // Parse sender ID
            let sender_id = match UserId::from_str(&sender_uuid) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        sender_uuid = %sender_uuid,
                        "Invalid sender UUID in anonymous DM"
                    );
                    if anon_dm_failure_decision(
                        conversation_id,
                        msg.server_timestamp,
                        &format!("unparseable sender id: {e}"),
                        message_repo,
                        domain_tx,
                    )
                    .should_ack()
                    {
                        deduplicator.mark_processed(&message_id);
                        processed
                            .entry(conversation_id)
                            .or_default()
                            .push(message_id);
                    }
                    continue;
                }
            };

            // Skip messages from ourselves — already displayed optimistically at send time.
            // The server may echo our own message back to our mailbox; processing it would
            // create a duplicate bubble in the UI.
            if current_user_id.is_some_and(|uid| *uid == sender_id) {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    message_id = %message_id,
                    "Skipping own message echoed back from anon DM mailbox poll"
                );
                deduplicator.mark_processed(&message_id);
                processed
                    .entry(conversation_id)
                    .or_default()
                    .push(message_id);
                continue;
            }

            // Drop all content from blocked users/conversations — messages,
            // calls, typing, presence, profile updates. Acknowledge to prevent
            // re-delivery but don't process.
            if is_blocked(&sender_id, &conversation_id) {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    sender_id = %sender_id,
                    "Dropping message from blocked sender/conversation"
                );
                deduplicator.mark_processed(&message_id);
                processed
                    .entry(conversation_id)
                    .or_default()
                    .push(message_id);
                continue;
            }

            tracing::info!(
                conversation_id = %conversation_id,
                sender_id = %sender_id,
                plaintext_len = plaintext.len(),
                "[ANON_DM] Decrypted message, parsing Content proto"
            );

            // Decode Content proto
            let content: Content = match ProstMessage::decode(plaintext.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        conversation_id = %conversation_id,
                        "Failed to decode Content proto in anonymous DM"
                    );
                    // Deterministic in the bytes — this is the v2/v3 format
                    // change, and retaining it is the infinite redelivery.
                    if anon_dm_failure_decision(
                        conversation_id,
                        msg.server_timestamp,
                        &format!("Content proto would not decode: {e}"),
                        message_repo,
                        domain_tx,
                    )
                    .should_ack()
                    {
                        deduplicator.mark_processed(&message_id);
                        processed
                            .entry(conversation_id)
                            .or_default()
                            .push(message_id);
                    }
                    continue;
                }
            };

            // Route based on content type. The routing table itself is
            // `route_anon_dm_content`, so every outcome is one value the ack
            // policy can be applied to instead of a bare bool.
            let decision = match route_anon_dm_content(content, sender_id, sender_device_id, conversation_id) {
                AnonDmRoute::Deliver(events) => {
                    for event in events {
                        if event_tx.send(event).await.is_err()
                        {
                            return processed;
                        }
                    }
                    AckDecision::Ack
                }
                AnonDmRoute::Malformed(reason) => anon_dm_failure_decision(
                    conversation_id,
                    msg.server_timestamp,
                    reason,
                    message_repo,
                    domain_tx,
                ),
            };

            // Track processed message for acknowledgment
            if decision.should_ack() {
                deduplicator.mark_processed(&message_id);
                processed
                    .entry(conversation_id)
                    .or_default()
                    .push(message_id);
            }
        }
    }

    processed
}
