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

use crate::domain::anon_dm::AnonDmId;
use crate::domain::UserId;
use crate::inbox::events::InboxEvent;
use crate::mailbox::anon_dm::AnonDmPollResult;
use crate::network::validation::MessageDeduplicator;
use crate::proto::content_v1::{self, Content};
use crate::services::pairwise::PairwiseService;

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
) -> ProcessedAnonDmMessages {
    let mut processed: ProcessedAnonDmMessages = HashMap::new();

    for result in results {
        let conversation_id = result.conversation_id;

        for msg in result.messages {
            let message_id = msg.message_id.clone();

            // Check if we've already processed this message ID.
            // This prevents "old counter" errors when acknowledgment fails
            // and the server re-delivers the same message.
            if !deduplicator.mark_processed(&message_id) {
                tracing::debug!(
                    conversation_id = %conversation_id,
                    message_id = %message_id,
                    "Anon DM message already processed, skipping decryption"
                );
                // Still add to processed list so we acknowledge it again
                processed.entry(conversation_id).or_default().push(message_id);
                continue;
            }

            // Decrypt using pairwise service (sealed sender)
            let (sender_uuid, plaintext) = match pairwise_service
                .decrypt_sealed_sender(&msg.encrypted_envelope)
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        conversation_id = %conversation_id,
                        message_id = %msg.message_id,
                        error = %e,
                        "Failed to decrypt anonymous DM message"
                    );
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
                    continue;
                }
            };

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
                    continue;
                }
            };

            // Route based on content type
            let was_processed = match content.content {
                Some(content_v1::content::Content::AnonDmAccept(accept)) => {
                    tracing::info!(
                        sender_id = %sender_id,
                        conversation_id = %conversation_id,
                        "Received anonymous DM acceptance from mailbox poll"
                    );

                    let _ = event_tx
                        .send(InboxEvent::AnonDmAccept {
                            peer_id: sender_id,
                            conversation_id: conversation_id.as_bytes().to_vec(),
                            responder_mailbox_pk: accept.responder_mailbox_pk,
                            profile_key: accept.profile_key,
                            encrypted_display_name: accept.encrypted_display_name,
                            encrypted_avatar: accept.encrypted_avatar,
                            timestamp: accept.timestamp,
                        })
                        .await;
                    true
                }
                Some(content_v1::content::Content::AnonDmMessage(dm_msg)) => {
                    tracing::info!(
                        sender_id = %sender_id,
                        conversation_id = %conversation_id,
                        has_inner = dm_msg.inner_content.is_some(),
                        "Received anonymous DM message from mailbox poll"
                    );

                    if let Some(inner) = dm_msg.inner_content {
                        // Check if the inner content is an anon group management message
                        
                        match inner.content.as_ref() {
                            Some(content_v1::content::Content::AnonGroupInvite(invite)) => {
                                tracing::info!(
                                    sender_id = %sender_id,
                                    group_id = %hex::encode(&invite.group_id),
                                    "Received anonymous group invite inside anon DM"
                                );
                                let _ = event_tx
                                    .send(InboxEvent::AnonGroupInvite {
                                        peer_id: sender_id,
                                        group_id: invite.group_id.clone(),
                                        group_name: invite.group_name.clone(),
                                        inviter_mailbox_pk: invite.inviter_mailbox_pk.clone(),
                                        timestamp: invite.timestamp,
                                    })
                                    .await;
                                true
                            }
                            Some(content_v1::content::Content::AnonGroupJoinAck(ack)) => {
                                tracing::info!(
                                    sender_id = %sender_id,
                                    group_id = %hex::encode(&ack.group_id),
                                    "Received anonymous group join ack inside anon DM"
                                );
                                let _ = event_tx
                                    .send(InboxEvent::AnonGroupJoinAck {
                                        peer_id: sender_id,
                                        group_id: ack.group_id.clone(),
                                        joiner_mailbox_pk: ack.joiner_mailbox_pk.clone(),
                                        timestamp: ack.timestamp,
                                    })
                                    .await;
                                true
                            }
                            Some(content_v1::content::Content::AnonGroupMemberList(list)) => {
                                tracing::info!(
                                    sender_id = %sender_id,
                                    group_id = %hex::encode(&list.group_id),
                                    "Received anonymous group member list inside anon DM"
                                );
                                let _ = event_tx
                                    .send(InboxEvent::AnonGroupMemberList {
                                        peer_id: sender_id,
                                        group_id: list.group_id.clone(),
                                        members: list.members.clone(),
                                        timestamp: list.timestamp,
                                    })
                                    .await;
                                true
                            }
                            _ => {
                                // Default handling for other messages
                                let _ = event_tx
                                    .send(InboxEvent::AnonDmMessage {
                                        peer_id: sender_id,
                                        conversation_id: conversation_id.as_bytes().to_vec(),
                                        inner_content: inner,
                                    })
                                    .await;
                                true
                            }
                        }
                    } else {
                        false
                    }
                }
                other => {
                    let content_type = match &other {
                        Some(content_v1::content::Content::DataMessage(_)) => "DataMessage",
                        Some(content_v1::content::Content::Typing(_)) => "Typing",
                        Some(content_v1::content::Content::Reaction(_)) => "Reaction",
                        Some(content_v1::content::Content::Receipt(_)) => "Receipt",
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
                    // Still acknowledge to avoid re-processing
                    true
                }
            };

            // Track processed message for acknowledgment
            if was_processed {
                processed
                    .entry(conversation_id)
                    .or_default()
                    .push(message_id);
            }
        }
    }

    processed
}
