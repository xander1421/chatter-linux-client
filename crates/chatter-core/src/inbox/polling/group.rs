//! Group mailbox polling.
//!
//! Handles polling and processing messages from per-group ephemeral mailboxes
//! (NDSS '21 paper implementation for privacy-preserving group messaging).
//!
//! Per NDSS '21 and Signal Protocol spec: Group messages are pairwise encrypted
//! using Double Ratchet (sealed sender) for each member. The sender identity is
//! cryptographically revealed during SSDecVer (sealed sender decryption), NOT
//! from plaintext metadata.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::ids::ChannelId;
use crate::domain::message::ConversationType;
use crate::inbox::ack_policy::{self, AckDecision, MarkerTarget};
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::storage::MessageRepository;

/// Decide what to do with a group-mailbox envelope whose decrypt failed.
///
/// The marker lands in the GROUP's conversation, not a DM: that is where the
/// user is looking when the gap appears. `peer_id` is `Some` only when the
/// failure authenticated a sender before dying (an identity change or a
/// desync); a sealed-sender envelope that failed to open names nobody.
pub(crate) fn group_failure_decision(
    group_id: ChannelId,
    peer_id: Option<crate::domain::UserId>,
    envelope_ts_ns: u64,
    err: &crate::error::ServiceError,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
) -> AckDecision {
    ack_policy::decide_after_failure(
        err,
        MarkerTarget {
            conversation_id: group_id,
            conversation_type: ConversationType::Channel,
            peer_id,
            envelope_ts_ns,
        },
        message_repo,
        domain_tx,
    )
}
use crate::mailbox::group::{GroupCoordinator, GroupFanout};
use crate::proto::chat_v1::SealedSenderMessage;
use crate::services::attachment::AttachmentService;
use crate::services::gateway::{MessageGateway, SealedSenderResult};
use crate::storage::{AttachmentRepository, ChannelRepository};

/// Poll all group mailboxes for new messages.
///
/// Group messages are received via per-group ephemeral mailboxes (NDSS '21 paper).
/// Messages are pairwise encrypted with sealed sender (Double Ratchet) and routed
/// through anonymous mailboxes to prevent server-side correlation.
///
/// Per Signal Protocol: The sender identity is cryptographically revealed during
/// sealed sender decryption (SSDecVer), NOT from plaintext metadata.
///
/// This method handles:
/// - Sealed sender encrypted messages (sender revealed via SSDecVer)
/// - GroupMailboxAddress announcements (member mailbox exchange)
pub async fn poll_group_mailboxes(
    group_fanout: &Arc<GroupFanout>,
    group_coordinator: &Arc<GroupCoordinator>,
    message_gateway: &Arc<MessageGateway>,
    event_tx: &broadcast::Sender<InboxEvent>,
    channel_repo: &Arc<ChannelRepository>,
    attachment_repo: &Arc<AttachmentRepository>,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    match group_fanout.poll_all_group_mailboxes().await {
        Ok(poll_results) => {
            let mut total_groups = 0u32;
            let mut total_messages = 0u32;

            if poll_results.is_empty() {
                // Only log occasionally to avoid spam
                static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % 60 == 0 {
                    tracing::debug!("[GROUP_POLL] No group mailboxes to poll (count={})", count);
                }
            }

            for (group_id, poll_result) in poll_results {
                total_groups += 1;
                let message_count = poll_result.messages.len();
                tracing::info!(
                    group_id = %group_id,
                    message_count = message_count,
                    "[GROUP_POLL] Polled group mailbox"
                );

                if message_count == 0 {
                    continue;
                }

                // Acking makes the SERVER DELETE the message, so ids go in here
                // per-message, only once the policy has decided this one is
                // safe to destroy. `poll_result.message_ids()` collected them
                // all up front, which acked failures too.
                let mut message_ids: Vec<String> = Vec::with_capacity(message_count);

                for msg in poll_result.messages {
                    total_messages += 1;

                    // Per NDSS '21: Group mailbox messages are sealed sender encrypted.
                    // The sender identity is revealed during SSDecVer (decryption).
                    let decision = process_sealed_sender_group_message(
                        &msg.encrypted_envelope,
                        &msg.message_id,
                        group_id,
                        message_gateway,
                        group_coordinator,
                        event_tx,
                        channel_repo,
                        attachment_repo,
                        message_repo,
                        domain_tx,
                        msg.server_timestamp,
                    )
                    .await;

                    if decision.should_ack() {
                        message_ids.push(msg.message_id.clone());
                    }
                }

                // Acknowledge processed messages using mailbox_id (works for both server-managed and anon groups)
                if !message_ids.is_empty() {
                    if let Err(e) = group_fanout
                        .acknowledge_by_mailbox_id(&poll_result.mailbox_id, message_ids)
                        .await
                    {
                        tracing::warn!(
                            group_id = %group_id,
                            mailbox_id = %poll_result.mailbox_id,
                            error = %e,
                            "Failed to acknowledge group mailbox messages"
                        );
                    }
                }
            }

            if total_messages > 0 || total_groups > 0 {
                let _ = event_tx.send(InboxEvent::GroupMailboxPollCompleted {
                    total_groups,
                    total_messages,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Group mailbox poll failed");
        }
    }
}

/// Process a sealed sender group mailbox message.
///
/// Per NDSS '21 and Signal Protocol: Group messages are pairwise encrypted using
/// sealed sender (Double Ratchet). The sender identity is cryptographically revealed
/// during SSDecVer decryption, NOT from plaintext metadata.
#[allow(clippy::too_many_arguments)]
async fn process_sealed_sender_group_message(
    envelope: &[u8],
    message_id: &str,
    group_id: ChannelId,
    message_gateway: &Arc<MessageGateway>,
    group_coordinator: &Arc<GroupCoordinator>,
    event_tx: &broadcast::Sender<InboxEvent>,
    channel_repo: &Arc<ChannelRepository>,
    attachment_repo: &Arc<AttachmentRepository>,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_ts_ns: u64,
) -> AckDecision {
    // Create SealedSenderMessage from raw envelope
    let sealed_sender_msg = SealedSenderMessage {
        sealed_body: envelope.to_vec(),
        server_key_id: 0,
        timestamp: 0,
    };

    // Process via message gateway - SSDecVer reveals sender identity.
    //
    // The failure is split off first because it is the only outcome that can
    // LOSE the message: everything the match below handles was handled, so the
    // envelope is safe to drain.
    let outcome = match message_gateway
        .process_incoming_message(sealed_sender_msg)
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::error!(
                group_id = %group_id,
                message_id = %message_id,
                error = %e,
                "Failed to process group mailbox message"
            );
            // No authenticated sender: the sealed sender envelope is what
            // failed, so the marker names the group but nobody in it.
            return group_failure_decision(
                group_id,
                None,
                envelope_ts_ns,
                &e,
                message_repo,
                domain_tx,
            );
        }
    };

    match outcome {
        SealedSenderResult::MessageStored => {
            tracing::debug!(
                group_id = %group_id,
                message_id = %message_id,
                "Group mailbox message stored (no UI event)"
            );
        }
        // 1:1 self-heal results are dispatched on the DEVICE inbox path
        // (polling/device.rs), which carries the event/domain senders. The
        // anonymous group mailbox has neither — log and drop. (The sender-side
        // archive in SessionHealed already happened inside the gateway.)
        SealedSenderResult::DecryptionErrorNeedsReport { sender_id, .. } => {
            tracing::warn!(
                group_id = %group_id,
                message_id = %message_id,
                sender_id = %sender_id,
                "Session desync detected on group mailbox path — DEM report not dispatched here"
            );
            // The ciphertext is dead and this path cannot even ship the
            // DecryptionErrorMessage that would heal the session for next
            // time, so this is the one arm below that is a LOSS rather than a
            // handled outcome. Mark it in the group, then drain.
            return ack_policy::record_marker_then_ack(
                message_repo,
                domain_tx,
                MarkerTarget {
                    conversation_id: group_id,
                    conversation_type: ConversationType::Channel,
                    peer_id: Some(sender_id),
                    envelope_ts_ns,
                },
                ack_policy::FailureMarker::DeliveryIssue,
            );
        }
        SealedSenderResult::SessionHealed { peer_id, archived, .. } => {
            tracing::info!(
                group_id = %group_id,
                message_id = %message_id,
                peer_id = %peer_id,
                archived,
                "DecryptionErrorMessage processed via group mailbox path"
            );
        }
        // SKDMs ride the device inbox (sealed sender), not the group mailbox;
        // if one lands here the key is still installed by the gateway. There is
        // no domain_tx on this path to fire SenderKeyReceived, so just
        // acknowledge it (the device-inbox path is where the drain happens).
        SealedSenderResult::SenderKeyProcessed { channel_id, .. } => {
            tracing::debug!(
                group_id = %group_id,
                channel_id = %channel_id,
                "SKDM processed via group mailbox path (key installed)"
            );
        }
        SealedSenderResult::DataMessageReceived {
            sender_id,
            channel_id,
            message_id: msg_id,
            body,
            timestamp,
            attachments,
        } => {
            // Increment unread count
            if let Ok(new_count) = channel_repo.increment_unread_count(&channel_id) {
                tracing::debug!(
                    channel_id = %channel_id,
                    unread_count = new_count,
                    "Incremented group unread count (pairwise sealed sender)"
                );
            }

            // Create attachment records from pointers (service layer responsibility)
            if !attachments.is_empty() {
                let attachment_service = AttachmentService::new_local(attachment_repo.clone());
                let created_ids = attachment_service.create_from_pointers(&attachments);
                tracing::debug!(
                    channel_id = %channel_id,
                    attachment_count = created_ids.len(),
                    "Created attachment records from pointers"
                );
            }

            // Emit UI event so message appears in chat
            let _ = event_tx.send(InboxEvent::ChannelMessageReceived {
                channel_id,
                sender_id,
                message_id: msg_id.to_string(),
                text: body,
                timestamp,
                attachments,
            });

            tracing::info!(
                group_id = %group_id,
                channel_id = %channel_id,
                sender_id = %sender_id,
                "Group message received via pairwise sealed sender, UI notified"
            );
        }
        SealedSenderResult::KeyExchange {
            sender_id,
            conversation_id,
            ephemeral_public_key: _,
            timestamp: _,
        } => {
            // KeyExchange in group context is unexpected but log it
            tracing::warn!(
                group_id = %group_id,
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                "Unexpected KeyExchange in group mailbox"
            );
        }
        SealedSenderResult::SenderKeyRequested {
            requester_id,
            channel_id,
        } => {
            // Note: With pairwise encryption, sender key requests are deprecated.
            // Keep for backwards compatibility during transition.
            tracing::info!(
                group_id = %group_id,
                channel_id = %channel_id,
                requester_id = %requester_id,
                "Sender key request via group mailbox (legacy)"
            );
            let _ = event_tx.send(InboxEvent::SenderKeyNeeded {
                channel_id,
                sender_id: requester_id,
            });
        }
        SealedSenderResult::MailboxAnnouncement {
            sender_id,
            channel_id,
            announcement,
        } => {
            // Member announcing their mailbox address
            if let Err(e) = group_coordinator.process_announcement(sender_id, &announcement) {
                tracing::error!(
                    channel_id = %channel_id,
                    sender_id = %sender_id,
                    error = %e,
                    "Failed to process group mailbox announcement"
                );
            } else {
                tracing::info!(
                    channel_id = %channel_id,
                    sender_id = %sender_id,
                    "Processed group mailbox announcement via sealed sender"
                );
                let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                    channel_id,
                    member_id: sender_id,
                    mailbox_pk: announcement.mailbox_pk.clone(),
                    timestamp: announcement.timestamp,
                });
            }
        }
        SealedSenderResult::AnonGroupMessageReceived {
            sender_id,
            group_id: anon_group_id,
            inner_content,
        } => {
            tracing::info!(
                sender_id = %sender_id,
                group_id_hex = %hex::encode(&anon_group_id),
                "Received anonymous group message, emitting UI event"
            );

            // Emit UI event for the anonymous group message
            let _ = event_tx.send(InboxEvent::AnonGroupMessage {
                peer_id: sender_id,
                group_id: anon_group_id,
                inner_content,
            });
        }
        SealedSenderResult::AnonGroupJoinAckReceived {
            sender_id,
            group_id: anon_group_id,
            joiner_mailbox_pk,
            timestamp,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
        } => {
            tracing::info!(
                sender_id = %sender_id,
                group_id_hex = %hex::encode(&anon_group_id),
                joiner_mailbox_pk_len = joiner_mailbox_pk.len(),
                timestamp = timestamp,
                has_profile = profile_key.is_some(),
                "Received anonymous group join ack, emitting event to update member mailbox"
            );

            // Emit event to update the joiner's mailbox key and profile
            let _ = event_tx.send(InboxEvent::AnonGroupJoinAck {
                peer_id: sender_id,
                group_id: anon_group_id,
                joiner_mailbox_pk,
                timestamp,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
            });
        }
        SealedSenderResult::ControlContent { .. } => {
            // ControlContent rides the device inbox, not the group mailbox.
            // Drop if it lands here — match must stay exhaustive.
        }
        SealedSenderResult::ProfileKeyReceived { .. } => {
            // Friend-DM profile-key handshake rides the device inbox, not
            // the group mailbox. Drop if it lands here — match must stay
            // exhaustive.
        }
    }

    // Every arm above is a handled outcome.
    AckDecision::Ack
}

