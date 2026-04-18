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
use crate::inbox::events::InboxEvent;
use crate::mailbox::group::{GroupCoordinator, GroupFanout};
use crate::proto::chat_v1::SealedSenderMessage;
use crate::services::attachment::AttachmentService;
use crate::services::message_gateway::{MessageGateway, SealedSenderResult};
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

                // Collect message IDs for acknowledgment
                let message_ids = poll_result.message_ids();

                for msg in poll_result.messages {
                    total_messages += 1;

                    // Per NDSS '21: Group mailbox messages are sealed sender encrypted.
                    // The sender identity is revealed during SSDecVer (decryption).
                    process_sealed_sender_group_message(
                        &msg.encrypted_envelope,
                        &msg.message_id,
                        group_id,
                        message_gateway,
                        group_coordinator,
                        event_tx,
                        channel_repo,
                        attachment_repo,
                    )
                    .await;
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
) {
    // Create SealedSenderMessage from raw envelope
    let sealed_sender_msg = SealedSenderMessage {
        sealed_body: envelope.to_vec(),
        server_key_id: 0,
        timestamp: 0,
    };

    // Process via message gateway - SSDecVer reveals sender identity
    match message_gateway
        .process_incoming_message(sealed_sender_msg)
        .await
    {
        Ok(SealedSenderResult::MessageStored) => {
            tracing::debug!(
                group_id = %group_id,
                message_id = %message_id,
                "Group mailbox message stored (no UI event)"
            );
        }
        Ok(SealedSenderResult::DataMessageReceived {
            sender_id,
            channel_id,
            message_id: msg_id,
            body,
            timestamp,
            attachments,
        }) => {
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
        Ok(SealedSenderResult::KeyExchange {
            sender_id,
            conversation_id,
            ephemeral_public_key: _,
            timestamp: _,
        }) => {
            // KeyExchange in group context is unexpected but log it
            tracing::warn!(
                group_id = %group_id,
                sender_id = %sender_id,
                conversation_id = %conversation_id,
                "Unexpected KeyExchange in group mailbox"
            );
        }
        Ok(SealedSenderResult::SenderKeyRequested {
            requester_id,
            channel_id,
        }) => {
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
        Ok(SealedSenderResult::MailboxAnnouncement {
            sender_id,
            channel_id,
            announcement,
        }) => {
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
        Ok(SealedSenderResult::AnonGroupMessageReceived {
            sender_id,
            group_id: anon_group_id,
            inner_content,
        }) => {
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
        Ok(SealedSenderResult::AnonGroupJoinAckReceived {
            sender_id,
            group_id: anon_group_id,
            joiner_mailbox_pk,
            timestamp,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
        }) => {
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
        Ok(SealedSenderResult::ControlContent { .. }) => {
            // Group-mailbox transport no longer carries channel control
            // content — that now rides the device inbox. Ignore if it sneaks
            // through via a peer still running the old path.
            tracing::debug!(
                group_id = %group_id,
                message_id = %message_id,
                "Ignoring channel control content delivered via group mailbox (legacy path)"
            );
        }
        Err(e) => {
            tracing::error!(
                group_id = %group_id,
                message_id = %message_id,
                error = %e,
                "Failed to process group mailbox message"
            );
        }
    }
}

