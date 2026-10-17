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
///
/// Bounded, because the group mailbox has no cursor at all — nothing else
/// stops a permanently-failing envelope being re-decrypted on every poll.
#[allow(clippy::too_many_arguments)]
pub(crate) fn group_failure_decision(
    group_id: ChannelId,
    peer_id: Option<crate::domain::UserId>,
    envelope_ts_ns: u64,
    err: &crate::error::ServiceError,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&tokio::sync::broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &ack_policy::RetryBudget,
) -> AckDecision {
    ack_policy::decide_after_failure_bounded(
        err,
        MarkerTarget {
            conversation_id: group_id,
            conversation_type: ConversationType::Channel,
            peer_id,
            envelope_ts_ns,
        },
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
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
    retry_budget: &ack_policy::RetryBudget,
    content_handoff: Option<&Arc<crate::inbox::content_handoff::ContentHandoff>>,
) {
    if let Some(handoff) = content_handoff {
        handoff.replay(message_repo).await;
    }
    match group_fanout.poll_all_group_mailboxes().await {
        Ok(batch) => {
            let mut total_groups = 0u32;
            let mut total_messages = 0u32;

            tracing::info!(
                mailbox_count = batch.mailbox_count,
                completed = batch.results.len(),
                timed_out = batch.timed_out,
                failed = batch.failed,
                "[GROUP_POLL] Group mailbox polling cycle completed"
            );

            if batch.mailbox_count == 0 {
                // Only log occasionally to avoid spam
                static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % 60 == 0 {
                    tracing::debug!("[GROUP_POLL] No group mailboxes to poll (count={})", count);
                }
            }

            for (group_id, poll_result) in batch.results {
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
                let mut ack_bindings = Vec::new();

                for msg in poll_result.messages {
                    total_messages += 1;

                    // Per NDSS '21: Group mailbox messages are sealed sender encrypted.
                    // The sender identity is revealed during SSDecVer (decryption).
                    let binding = message_repo
                        .reaction_receive_key(
                            "anon_group",
                            group_id,
                            &msg.message_id,
                            msg.server_timestamp,
                        )
                        .ok()
                        .map(|key| {
                            (
                                key,
                                crate::storage::receive_artifact::envelope_digest(
                                    &msg.encrypted_envelope,
                                ),
                            )
                        });
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
                        retry_budget,
                        content_handoff,
                    )
                    .await;

                    if decision.should_ack() {
                        retry_budget.forget(&msg.message_id);
                        message_ids.push(msg.message_id.clone());
                        if let Some(binding) = binding {
                            ack_bindings.push(binding);
                        }
                    }
                }

                // Acknowledge processed messages using mailbox_id (works for both server-managed and anon groups)
                if !message_ids.is_empty() {
                    let ack_ids_for_log = message_ids.clone();
                    let ack_result = group_fanout
                        .acknowledge_by_mailbox_id(&poll_result.mailbox_id, message_ids)
                        .await;
                    if let Err(e) = &ack_result {
                        tracing::warn!(
                            group_id = %group_id,
                            mailbox_id = %poll_result.mailbox_id,
                            error = %e,
                            "Failed to acknowledge group mailbox messages"
                        );
                    }
                    if ack_result.is_ok() {
                        for id in &ack_ids_for_log {
                            tracing::info!(envelope_id = %id, "secret mailbox ACK succeeded");
                        }
                        for (key, digest) in ack_bindings {
                            if crate::inbox::reaction_handoff::acknowledge(
                                message_repo,
                                &key,
                                &digest,
                                true,
                            )
                            .is_err()
                            {
                                tracing::warn!("Group ACK receipt awaits storage recovery");
                            }
                        }
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
    retry_budget: &ack_policy::RetryBudget,
    content_handoff: Option<&Arc<crate::inbox::content_handoff::ContentHandoff>>,
) -> AckDecision {
    crate::inbox::reaction_handoff::replay(message_repo, domain_tx);
    let key =
        match message_repo.reaction_receive_key("anon_group", group_id, message_id, envelope_ts_ns)
        {
            Ok(key) => key,
            Err(_) => {
                tracing::warn!("Group receive binding unavailable");
                return AckDecision::Retain;
            }
        };
    // Create SealedSenderMessage from raw envelope
    let sealed_sender_msg = SealedSenderMessage {
        sealed_body: envelope.to_vec(),
        server_key_id: 0,
        // What a DecryptionErrorMessage for this envelope names.
        timestamp: envelope_ts_ns,
    };

    // Process via message gateway - SSDecVer reveals sender identity.
    //
    // The failure is split off first because it is the only outcome that can
    // LOSE the message: everything the match below handles was handled, so the
    // envelope is safe to drain.
    let outcome = match message_gateway
        .process_incoming_reaction_durable(sealed_sender_msg, &key)
        .await
    {
        Ok((Some(artifact), _)) => {
            if artifact.kind == crate::storage::receive_artifact::ReceiveKind::Content {
                let Some(handoff) = content_handoff else {
                    tracing::warn!("Group data handoff unavailable; envelope retained");
                    return AckDecision::Retain;
                };
                if let Err(error) = handoff.apply(message_repo, &artifact).await {
                    tracing::warn!(envelope_id = %artifact.key.envelope_id, %error, "Group post awaits replay");
                }
                return AckDecision::Ack;
            }
            return match crate::inbox::reaction_handoff::apply(message_repo, &artifact, domain_tx) {
                Ok(()) => AckDecision::Ack,
                Err(_) => {
                    tracing::warn!("Group reaction application failed; envelope retained");
                    AckDecision::Retain
                }
            };
        }
        Ok((None, Some(outcome))) => outcome,
        Ok((None, None)) => return AckDecision::Retain,
        Err(crate::error::ServiceError::Storage(_))
        | Err(crate::error::ServiceError::Crypto(crate::error::CryptoError::Storage(_))) => {
            tracing::warn!("Atomic group receive storage failed; envelope retained");
            return AckDecision::Retain;
        }
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
                message_id,
                retry_budget,
            );
        }
    };

    match outcome {
        SealedSenderResult::ChannelReceiptTargetsSynced { .. } => {}
        SealedSenderResult::MessageStored => {
            tracing::debug!(
                group_id = %group_id,
                message_id = %message_id,
                "Group mailbox message stored (no UI event)"
            );
        }
        // The pairwise session with this member is desynced. Same self-heal
        // as the device inbox (polling/device.rs): the InboxEventProcessor
        // ships the DecryptionErrorMessage back to the sending device.
        SealedSenderResult::DecryptionErrorNeedsReport {
            sender_id,
            sender_device_id,
            dem_serialized,
            ..
        } => {
            tracing::warn!(
                group_id = %group_id,
                message_id = %message_id,
                sender_id = %sender_id,
                sender_device_id,
                "Session desync on group mailbox path — dispatching DecryptionErrorMessage report to sender"
            );
            let _ = event_tx.send(InboxEvent::DmDecryptionErrorNeedsReport {
                peer_id: sender_id,
                peer_device_id: sender_device_id,
                dem_serialized,
            });
            // The report heals the SESSION; this ciphertext is dead, so it
            // is the one arm below that is a LOSS rather than a handled
            // outcome. Mark it in the group, then drain.
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
        // A member's retry request that arrived here rather than in the
        // device inbox is answered all the same.
        SealedSenderResult::ResendRequested {
            peer_id,
            peer_device_id,
            timestamp_ms,
        } => {
            let _ = event_tx.send(InboxEvent::GroupResendRequested {
                peer_id,
                peer_device_id,
                timestamp_ms,
            });
        }
        SealedSenderResult::SessionHealed {
            peer_id,
            peer_device_id,
            archived,
            timestamp_ms,
        } => {
            tracing::info!(
                group_id = %group_id,
                message_id = %message_id,
                peer_id = %peer_id,
                archived,
                "DecryptionErrorMessage processed via group mailbox path"
            );
            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::DmSessionHealed {
                    peer_user_id: peer_id,
                    peer_device_id,
                    archived,
                });
            }
            let _ = event_tx.send(InboxEvent::DmResendRequested {
                peer_id,
                peer_device_id,
                timestamp_ms,
            });
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
            ..
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
                match attachment_service.create_from_pointers(&attachments) {
                    Ok(created_ids) => tracing::debug!(
                        channel_id = %channel_id,
                        attachment_count = created_ids.len(),
                        "Created attachment records from pointers"
                    ),
                    Err(error) => tracing::warn!(%error, "Failed to store attachment records"),
                }
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
            ..
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
            // Sender-key requests travel sealed to the target's devices, never
            // through a group mailbox.
            tracing::warn!(
                group_id = %group_id,
                channel_id = %channel_id,
                requester_id = %requester_id,
                "Unexpected sender key request in group mailbox"
            );
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
            if !crate::inbox::reaction_handoff::group_destination_matches(group_id, &anon_group_id)
            {
                tracing::warn!("Rejected group content claiming a different mailbox destination");
                return AckDecision::Ack;
            }
            tracing::info!(
                sender_id = %sender_id,
                group_id_hex = %hex::encode(&anon_group_id),
                "Received anonymous group message, emitting UI event"
            );

            let event = InboxEvent::AnonGroupMessage {
                peer_id: sender_id,
                group_id: anon_group_id,
                inner_content,
            };
            let _ = event_tx.send(event);
        }
        SealedSenderResult::AnonGroupJoinAckReceived {
            sender_id,
            sender_device_id,
            group_id: anon_group_id,
            joiner_mailbox_pk,
            timestamp,
            profile_key,
            encrypted_display_name,
            avatar,
        } => {
            tracing::info!(
                has_profile = profile_key.is_some(),
                "Received secret group join ack, emitting event to update member mailbox"
            );

            // Emit event to update the joiner's mailbox key, device and profile
            let _ = event_tx.send(InboxEvent::AnonGroupJoinAck {
                peer_id: sender_id,
                sender_device_id,
                group_id: anon_group_id,
                joiner_mailbox_pk,
                timestamp,
                profile_key,
                encrypted_display_name,
                avatar,
            });
        }
        SealedSenderResult::ControlContent { .. } => {
            // ControlContent rides the device inbox, not the group mailbox.
            // Drop if it lands here — match must stay exhaustive.
        }
        SealedSenderResult::SecretChatBootstrap { .. } => {
            // Requests and invites ride the device inbox, not the group
            // mailbox. Drop if it lands here.
        }
        SealedSenderResult::ProfileKeyReceived { .. } => {
            // Friend-DM profile-key handshake rides the device inbox, not
            // the group mailbox. Drop if it lands here — match must stay
            // exhaustive.
        }
        // A blocked member posted into a group mailbox we poll. Same answer as
        // everywhere else: nothing was stored, nothing is emitted, and the
        // envelope is drained by the `AckDecision::Ack` below — leaving it
        // queued would re-deliver it on every poll for as long as the block
        // stands. Blocking a person does not remove them from a shared group,
        // so this arm is reachable in ordinary use, unlike the two above.
        SealedSenderResult::DroppedBlockedSender { sender_id } => {
            tracing::info!(
                group_id = %group_id,
                message_id = %message_id,
                sender_id = %sender_id,
                "Group mailbox message from a blocked sender — dropped after decryption"
            );
        }
    }

    // Every arm above is a handled outcome.
    AckDecision::Ack
}
