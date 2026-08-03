//! Device inbox message processing.
//!
//! Handles processing messages from the device inbox including:
//! - Sender key encrypted channel messages
//! - Sealed sender messages
//! - Control content (typing, reactions, pins, edits, deletes)

use std::sync::Arc;

use parking_lot::RwLock;
use prost::Message as ProstMessage;
use sha2::Digest;
use tokio::sync::broadcast;

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message};
use crate::domain::UserId;
use crate::error::{CryptoError, ServiceError};
use crate::inbox::events::InboxEvent;
use crate::inbox::state::InboxStats;
use crate::mailbox::group::GroupCoordinator;
use crate::network::validation::{MessageValidationPipeline, PipelineResult};
use crate::proto::chat_v1::{DeviceMessage, EncryptedData, SealedSenderMessage};
use crate::proto::content_v1::{content, Content, DataMessage};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::inbox::handlers::DomainEvent;
use crate::services::gateway::{MessageGateway, RouteResult, SealedSenderResult};
use crate::storage::repositories::ephemeral_repo::EphemeralRepository;
use crate::storage::{ChannelRepository, MessageRepository};

/// Build the conversation entry that marks a DM we could not decrypt.
///
/// The inbox acks every polled envelope whatever the outcome, so an
/// undecryptable message is deleted from the server queue and never comes
/// back. The `DecryptionErrorMessage` we return heals the SESSION (the sender
/// archives it, so the next message decrypts) but does nothing for the message
/// that failed. Without this entry the user sees an unexplained gap.
///
/// Signal does the same thing on the receiving side: Signal-Android inserts a
/// "Delivery issue" conversation update on a bad decrypt.
///
/// `envelope_ts_ns` is the envelope's own server timestamp, used as `sent_at`
/// for two reasons: the marker sorts where the lost message would have been,
/// and the existing UNIQUE index on
/// (conversation_type, conversation_id, sender_id, sent_at) collapses repeats
/// when a failed `acknowledge` causes the same envelope to be replayed at the
/// next start.
#[must_use]
pub fn delivery_issue_message(peer_id: UserId, envelope_ts_ns: u64) -> Message {
    system_dm_marker(
        peer_id,
        envelope_ts_ns,
        crate::domain::message::SystemMessage::DeliveryIssue,
    )
}

/// Build the conversation entry that marks the message which surfaced a peer
/// identity change.
///
/// The SECOND marker, not a reuse of [`delivery_issue_message`], because the two
/// failures are not the same failure. A delivery issue is a desynced session:
/// the `DecryptionErrorMessage` we ship back heals it and a resend works, which
/// is what that marker tells the user to ask for. An identity change is a
/// changed pinned key: `classify_inner_failure`
/// (`crypto/signal/sealed_sender.rs`) deliberately refuses to call it a desync,
/// no report is sent, and a resend re-presents the same new key and fails
/// identically forever. Telling the user to ask for a resend here would send
/// them round a loop that cannot terminate.
///
/// `attributed` is [`crate::error::IdentityChangeSource::is_authenticated`] —
/// it decides whether the entry may state who this was, not how it is styled.
#[must_use]
pub fn identity_changed_message(
    peer_id: UserId,
    envelope_ts_ns: u64,
    attributed: bool,
) -> Message {
    system_dm_marker(
        peer_id,
        envelope_ts_ns,
        crate::domain::message::SystemMessage::IdentityChanged { attributed },
    )
}

/// Decide what to do with a device-inbox envelope whose processing failed.
///
/// `peer_id` is `Some` only when the failure authenticated a sender first. A
/// device-inbox failure that authenticated nobody has no conversation to land
/// in — the envelope is what failed. Fall back to a marker in the peer's DM
/// when we have one, and to a bare retain/ack when we do not: writing "someone
/// sent you something unreadable" into an arbitrary conversation would be worse
/// than the log line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn device_failure_decision(
    peer_id: Option<UserId>,
    envelope_ts_ns: u64,
    err: &ServiceError,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{self, AckDecision};
    let Some(peer) = peer_id else {
        return match ack_policy::classify_service(err) {
            ack_policy::Durability::Transient => ack_policy::retain_within_budget(
                // No conversation to mark, so the target only carries the
                // budget's timestamp. If the budget runs out the marker write
                // is attempted against this synthetic conversation and fails
                // to be useful — so give up quietly instead.
                ack_policy::dm_target(UserId::from_uuid(uuid::Uuid::nil()), envelope_ts_ns),
                message_repo,
                domain_tx,
                envelope_id,
                budget,
                ack_policy::now_ns(),
                &err.to_string(),
            ),
            ack_policy::Durability::Permanent => {
                tracing::warn!(
                    error = %err,
                    "[INBOX] permanent failure with no authenticated sender — draining \
                     the envelope with no marker, because there is no conversation to \
                     put one in"
                );
                budget.forget(envelope_id);
                AckDecision::Ack
            }
        };
    };
    ack_policy::decide_after_failure_bounded(
        err,
        ack_policy::dm_target(peer, envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
    )
}

/// The conversation a channel/group failure marker belongs in.
///
/// A sender-key envelope is a CHANNEL message, so its marker goes in the
/// channel — not in a DM with the sender, which is a different conversation the
/// user would read as "I lost a private message".
fn channel_marker_target(
    channel_repo: &Arc<ChannelRepository>,
    channel_id: ChannelId,
    peer_id: Option<UserId>,
    envelope_ts_ns: u64,
) -> crate::inbox::ack_policy::MarkerTarget {
    let conversation_type = match channel_repo.get(&channel_id) {
        Ok(Some(channel)) => channel.conversation_type(),
        _ => ConversationType::AnonGroup,
    };
    crate::inbox::ack_policy::MarkerTarget {
        conversation_id: channel_id,
        conversation_type,
        peer_id,
        envelope_ts_ns,
    }
}

/// Decide what to do with a sender-key (channel) envelope whose routing failed.
///
/// `channel_id` is `Some` only when the failure got far enough to name one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sender_key_failure_decision(
    channel_id: Option<ChannelId>,
    sender_id: UserId,
    envelope_ts_ns: u64,
    err: &ServiceError,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{self, AckDecision};
    // Without a channel there is nowhere honest to put the marker: the sender's
    // DM is a different conversation, and "you lost a private message" is not
    // what happened. Classify only.
    let Some(channel_id) = channel_id else {
        return match ack_policy::classify_service(err) {
            ack_policy::Durability::Transient => ack_policy::retain_within_budget(
                ack_policy::dm_target(sender_id, envelope_ts_ns),
                message_repo,
                domain_tx,
                envelope_id,
                budget,
                ack_policy::now_ns(),
                &err.to_string(),
            ),
            ack_policy::Durability::Permanent => {
                tracing::warn!(
                    sender_id = %sender_id,
                    error = %err,
                    "[INBOX_SK] permanent failure on an envelope whose channel could \
                     not be recovered — draining it with no marker, because the only \
                     conversation we could write to is the wrong one"
                );
                budget.forget(envelope_id);
                AckDecision::Ack
            }
        };
    };
    ack_policy::decide_after_failure_bounded(
        err,
        channel_marker_target(channel_repo, channel_id, Some(sender_id), envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
    )
}

/// Decide what to do when a channel message arrives before the sender key that
/// opens it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sender_key_missing_decision(
    channel_id: ChannelId,
    sender_id: UserId,
    envelope_ts_ns: u64,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy;
    // The policy names this case itself: "the SKDM … may land on a later poll
    // and then the same bytes decrypt". The path emitted `SenderKeyNeeded`,
    // logged "will succeed on retry after SKDM arrives" — and then acked, so
    // the server deleted the message before the key it was waiting for could
    // arrive. Nothing buffers the ciphertext on this path either;
    // `defer_pending_group_decrypt` is only reached from the channel-history
    // path in `client/channels.rs`.
    ack_policy::retain_within_budget(
        channel_marker_target(channel_repo, channel_id, Some(sender_id), envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
        "waiting on the sender key that opens this channel message",
    )
}

/// Why an ECIES init envelope could not be opened.
#[derive(Debug)]
pub(crate) enum EciesInitFailure<'a> {
    /// We could not read our own identity key — our state, not their bytes.
    IdentityKeyUnavailable(&'a ServiceError),
    /// The envelope, the HPKE open, or the inner init message would not parse.
    Malformed(&'a str),
}

/// Decide what to do with an ECIES init envelope that could not be opened.
///
/// There is no conversation yet — an ECIES init IS the first contact — so no
/// marker can be written either way; the decision is purely whether to keep the
/// envelope for a later attempt.
pub(crate) fn ecies_init_decision(
    failure: &EciesInitFailure<'_>,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{classify_service, AckDecision, Durability};
    match failure {
        // OUR identity key. A storage error or a registration that has not
        // finished writing it are both answered by a later poll; the stranger's
        // contact request is intact and will open then.
        EciesInitFailure::IdentityKeyUnavailable(e) => match classify_service(e) {
            Durability::Transient => AckDecision::Retain,
            Durability::Permanent => AckDecision::Ack,
        },
        // The envelope, the HPKE open against a key we did read, or the inner
        // init message. All pure functions of the bytes, so retaining replays
        // forever — and there is no conversation yet to mark, because an ECIES
        // init IS the first contact.
        EciesInitFailure::Malformed(reason) => {
            tracing::warn!(
                reason,
                "[INBOX_ECIES] malformed private-contact request — draining it; \
                 there is no conversation yet in which to record the loss"
            );
            AckDecision::Ack
        }
    }
}

/// Decide what to do with an envelope the gateway refused because its
/// authenticated sender is blocked.
///
/// Split out from the match arms so both device-inbox paths — sealed sender and
/// identified sender — answer identically, and so a test can pin the two
/// properties that matter without standing up a gateway: the envelope IS
/// drained, and NOTHING is written into the conversation.
///
/// Note what this function does not take: no `event_tx`, no `stats`, no
/// `message_repo` to write to. There is no way for it to surface the message,
/// which is the point.
pub(crate) fn blocked_sender_decision(
    sender_id: UserId,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    tracing::info!(
        sender_id = %sender_id,
        envelope_id,
        "[INBOX] message from a blocked sender — dropped after decryption"
    );
    crate::inbox::ack_policy::drop_and_ack("blocked sender", envelope_id, budget)
}

/// Shared shape for the system entries the device inbox writes into a DM.
///
/// Delegates to the one builder in [`crate::inbox::ack_policy`] so the four
/// loops cannot drift into writing four differently-shaped markers.
fn system_dm_marker(
    peer_id: UserId,
    envelope_ts_ns: u64,
    system: crate::domain::message::SystemMessage,
) -> Message {
    use crate::inbox::ack_policy::{self, FailureMarker};
    let marker = match system {
        crate::domain::message::SystemMessage::IdentityChanged { attributed } => {
            FailureMarker::IdentityChanged { attributed }
        }
        _ => FailureMarker::DeliveryIssue,
    };
    ack_policy::marker_message(ack_policy::dm_target(peer_id, envelope_ts_ns), marker)
}

/// Process a single message through the validation pipeline.
///
/// Handles:
/// - Sender key encrypted channel messages (EncryptedData with associated_data)
/// - Sealed sender messages (direct Signal Protocol encryption)
/// - GroupMailboxAddress announcements (initial mailbox exchange via device inbox)
/// - KeyExchange messages for DM bootstrap (emits DomainEvent)
#[allow(clippy::too_many_arguments)]
pub async fn process_device_message(
    msg: &DeviceMessage,
    pipeline: &MessageValidationPipeline,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    ephemeral_repo: &Arc<EphemeralRepository>,
    group_coordinator: &Arc<GroupCoordinator>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    retry_budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::AckDecision;
    tracing::info!(
        message_guid = %msg.message_guid,
        source_user_id = %msg.source_user_id,
        source_device_id = msg.source_device_id,
        envelope_type = msg.envelope_type,
        envelope_len = msg.envelope.len(),
        queue_id = msg.queue_id,
        "[INBOX] Processing device message"
    );

    // IMPORTANT: Check for sender key messages FIRST, before envelope_type routing.
    // Sender key messages may arrive with envelope_type=1 or 2 (ciphertext/prekey)
    // but they need sender key decryption, not Double Ratchet.
    // First, try to decode as EncryptedData (sender key encrypted channel message)
    if let Ok(encrypted_data) = EncryptedData::decode(msg.envelope.as_slice()) {
        let associated_data_str = String::from_utf8_lossy(&encrypted_data.associated_data);
        tracing::debug!(
            message_guid = %msg.message_guid,
            associated_data = %associated_data_str,
            "[INBOX] Decoded as EncryptedData"
        );

        // Check if this is a sender key message
        if MessageGateway::is_sender_key_message(&encrypted_data)
            || MessageGateway::is_sender_key_distribution(&encrypted_data)
            || MessageGateway::is_sender_key_request(&encrypted_data)
        {
            tracing::info!(
                message_guid = %msg.message_guid,
                associated_data = %associated_data_str,
                "[INBOX] Detected sender key related message"
            );

            // Parse sender info from message
            let sender_id: UserId = match msg.source_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(
                        message_id = %msg.message_guid,
                        source_user_id = %msg.source_user_id,
                        error = %e,
                        "[INBOX] Invalid sender ID in channel message"
                    );
                    let _ = event_tx.send(InboxEvent::MessageFailed {
                        message_id: msg.message_guid.clone(),
                        error: format!("Invalid sender ID: {}", e),
                    });
                    stats.write().messages_failed += 1;
                    // Malformed and deterministic in the bytes — and with no
                    // parseable sender there is no conversation to mark.
                    // Retaining it would replay forever.
                    return AckDecision::Ack;
                }
            };

            tracing::debug!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                source_device_id = msg.source_device_id,
                "[INBOX] Routing to sender key message handler"
            );

            // Route through message gateway. Its answer decides the ack: this
            // used to discard the outcome and `return AckDecision::Ack`, so a
            // missing sender key or a failed store read deleted the message.
            return process_sender_key_message(
                msg,
                &encrypted_data,
                &sender_id,
                event_tx,
                stats,
                message_gateway,
                message_repo,
                channel_repo,
                ephemeral_repo,
                group_coordinator,
                domain_tx,
                retry_budget,
            )
            .await;
        } else {
            tracing::debug!(
                message_guid = %msg.message_guid,
                associated_data = %associated_data_str,
                "[INBOX] EncryptedData but not sender key type, falling back to sealed sender"
            );
        }
    } else {
        tracing::debug!(
            message_guid = %msg.message_guid,
            "[INBOX] Not EncryptedData, checking envelope type"
        );
    }

    // Check envelope type for identified sender (PreKey/Ciphertext vs SealedSender)
    // EnvelopeType: 0=unspecified, 1=sealed_sender, 2=prekey_message, 3=message(ciphertext), 4=ecies_init
    // PreKey(2) and Message(3) are identified sender; SealedSender(1) falls through below
    let is_identified_sender = msg.envelope_type == EnvelopeType::PrekeyMessage as i32
        || msg.envelope_type == EnvelopeType::Message as i32;
    if is_identified_sender && !msg.source_user_id.is_empty() {
        tracing::info!(
            message_guid = %msg.message_guid,
            source_user_id = %msg.source_user_id,
            envelope_type = msg.envelope_type,
            "[INBOX] Processing as identified sender message (PreKey/Ciphertext)"
        );
        return process_identified_sender_message(
            msg,
            event_tx,
            stats,
            message_gateway,
            message_repo,
            domain_tx,
            retry_budget,
        )
        .await;
    }

    // Check for ECIES_INIT (HPKE-encrypted initial contact, NDSS'21 compliant)
    if msg.envelope_type == EnvelopeType::EciesInit as i32 {
        tracing::info!(
            message_guid = %msg.message_guid,
            envelope_len = msg.envelope.len(),
            "[INBOX] Processing as ECIES init message (NDSS'21 private contact)"
        );
        return process_ecies_init_message(msg, event_tx, stats, message_gateway, domain_tx).await;
    }

    // Fall back to sealed sender processing
    tracing::info!(
        message_guid = %msg.message_guid,
        "[INBOX] Processing as sealed sender message"
    );
    process_sealed_sender_message(
        msg,
        pipeline,
        event_tx,
        stats,
        message_gateway,
        message_repo,
        domain_tx,
        retry_budget,
    )
    .await
}

/// Process a sender key encrypted message.
#[allow(clippy::too_many_arguments)]
async fn process_sender_key_message(
    msg: &DeviceMessage,
    encrypted_data: &EncryptedData,
    sender_id: &UserId,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    ephemeral_repo: &Arc<EphemeralRepository>,
    group_coordinator: &Arc<GroupCoordinator>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    retry_budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::AckDecision;
    tracing::info!(
        message_guid = %msg.message_guid,
        sender_id = %sender_id,
        source_device_id = msg.source_device_id,
        "[INBOX_SK] Processing sender key message"
    );

    // Recovered for the marker target: a sender-key failure belongs in the
    // CHANNEL, and the gateway's error does not carry one.
    let channel_id: Option<ChannelId> =
        crate::proto::content_v1::GroupEncrypted::decode(encrypted_data.ciphertext.as_slice())
            .ok()
            .and_then(|ge| ge.channel_id.parse().ok());

    match message_gateway
        .route_encrypted_message(encrypted_data, sender_id, msg.source_device_id)
        .await
    {
        Ok(RouteResult::Decrypted(plaintext)) => {
            tracing::info!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                plaintext_len = plaintext.len(),
                "[INBOX_SK] Successfully decrypted, processing DataMessage"
            );
            process_decrypted_data_message(
                msg,
                &plaintext,
                sender_id,
                event_tx,
                stats,
                message_repo,
                channel_repo,
                ephemeral_repo,
                domain_tx,
            );
        }
        Ok(RouteResult::ControlContent(plaintext)) => {
            tracing::debug!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                plaintext_len = plaintext.len(),
                "[INBOX_SK] Decrypted control content"
            );
            // Parse as Content proto (typing, reaction, edit, pin, delete)
            match Content::decode(plaintext.as_slice()) {
                Ok(content_proto) => {
                    if let Some(inner) = content_proto.content {
                        dispatch_control_content(
                            inner,
                            sender_id,
                            &msg.message_guid,
                            event_tx,
                            stats,
                        );
                    } else {
                        tracing::warn!(
                            message_id = %msg.message_guid,
                            "Channel control content has no inner content"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        message_id = %msg.message_guid,
                        error = %e,
                        "Failed to parse Content from control message"
                    );
                    let _ = event_tx.send(InboxEvent::MessageFailed {
                        message_id: msg.message_guid.clone(),
                        error: format!("Failed to parse Content: {}", e),
                    });
                    stats.write().messages_failed += 1;
                }
            }
        }
        Ok(RouteResult::Processed) => {
            tracing::info!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                "[INBOX_SK] Processed SKDM or control message"
            );
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(RouteResult::NeedsSenderKey { channel_id, sender_id }) => {
            tracing::warn!(
                message_guid = %msg.message_guid,
                channel_id = %channel_id,
                sender_id = %sender_id,
                "[INBOX_SK] DECRYPTION FAILED - missing sender key, requesting SKDM"
            );
            // Emit event to request sender's SKDM
            let _ = event_tx.send(InboxEvent::SenderKeyNeeded {
                channel_id,
                sender_id,
            });
            // Also mark message as failed for now (will succeed on retry after SKDM arrives)
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("Missing sender key from {}", sender_id),
            });
            stats.write().messages_failed += 1;
            return sender_key_missing_decision(
                channel_id,
                sender_id,
                msg.server_timestamp,
                message_repo,
                channel_repo,
                domain_tx,
                &msg.message_guid,
                retry_budget,
            );
        }
        Ok(RouteResult::RespondWithSkdm { channel_id, requester_id, skdm }) => {
            tracing::info!(
                message_guid = %msg.message_guid,
                channel_id = %channel_id,
                requester_id = %requester_id,
                "[INBOX_SK] Received sender key request, emitting event to respond"
            );
            // Serialize SKDM for event
            use prost::Message as ProstMessage;
            let skdm_bytes = skdm.encode_to_vec();
            let _ = event_tx.send(InboxEvent::RespondWithSkdm {
                channel_id,
                requester_id,
                skdm_bytes,
            });
            stats.write().messages_processed += 1;
        }
        Ok(RouteResult::MailboxAnnouncement { channel_id, announcement }) => {
            // Gateway detected GroupMailboxAddress - process via coordinator
            if let Err(e) = group_coordinator.process_announcement(*sender_id, &announcement) {
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
                    "Processed group mailbox announcement from device inbox"
                );
                let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                    channel_id,
                    member_id: *sender_id,
                    mailbox_pk: announcement.mailbox_pk.clone(),
                    timestamp: announcement.timestamp,
                });
            }
            stats.write().messages_processed += 1;
        }
        Err(e) => {
            tracing::error!(
                message_id = %msg.message_guid,
                error = %e,
                "Failed to route encrypted message"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: e.to_string(),
            });
            stats.write().messages_failed += 1;
            return sender_key_failure_decision(
                channel_id,
                *sender_id,
                msg.server_timestamp,
                &e,
                message_repo,
                channel_repo,
                domain_tx,
                &msg.message_guid,
                retry_budget,
            );
        }
    }

    // Every other arm handled the message.
    retry_budget.forget(&msg.message_guid);
    AckDecision::Ack
}

/// Process a decrypted DataMessage.
fn process_decrypted_data_message(
    msg: &DeviceMessage,
    plaintext: &[u8],
    sender_id: &UserId,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    ephemeral_repo: &Arc<EphemeralRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    tracing::debug!(
        message_guid = %msg.message_guid,
        sender_id = %sender_id,
        plaintext_len = plaintext.len(),
        "[INBOX_DM] Decoding DataMessage from plaintext"
    );

    match DataMessage::decode(plaintext) {
        Ok(data_message) => {
            tracing::debug!(
                message_guid = %msg.message_guid,
                channel_id_raw = %data_message.channel_id,
                body_len = data_message.body.len(),
                timestamp = data_message.timestamp,
                "[INBOX_DM] DataMessage decoded successfully"
            );

            // Extract channel ID
            let channel_id: ChannelId = match data_message.channel_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(
                        message_id = %msg.message_guid,
                        error = %e,
                        "Invalid channel ID in DataMessage"
                    );
                    let _ = event_tx.send(InboxEvent::MessageFailed {
                        message_id: msg.message_guid.clone(),
                        error: format!("Invalid channel ID: {}", e),
                    });
                    stats.write().messages_failed += 1;
                    return;
                }
            };

            // Create message ID
            let message_id = msg
                .message_guid
                .parse::<MessageId>()
                .unwrap_or_else(|_| MessageId::new());

            // Derive ConversationType from the channel row rather than
            // hardcoding AnonGroup. Sender-key sealed-sender delivery is
            // used by BOTH identified groups (require_encryption=true on
            // a normal channel) and anon groups — the channel row's
            // channel_type + require_encryption config determines which.
            // Fallback to AnonGroup when the channel row is missing (rare
            // race between SKDM landing and channel sync); Channel::
            // conversation_type() default for encrypted-but-unknown is
            // AnonGroup so this preserves the prior behaviour for that
            // edge case.
            let conversation_type = match channel_repo.get(&channel_id) {
                Ok(Some(channel)) => channel.conversation_type(),
                Ok(None) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        "channel row missing when assigning ConversationType — defaulting to AnonGroup"
                    );
                    ConversationType::AnonGroup
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "failed to load channel for ConversationType derivation — defaulting to AnonGroup"
                    );
                    ConversationType::AnonGroup
                }
            };

            // Create domain message with sender's timestamp for Signal-style dedup
            // created_at is derived from sent_at in Message::new_text()
            let mut message =
                Message::new_text(channel_id, conversation_type, Some(*sender_id), data_message.body.clone(), data_message.timestamp);
            message.id = message_id;

            // Add attachment IDs to message (attachments are stored separately in attachment_repo)
            message.attachments = data_message
                .attachments
                .iter()
                .filter(|p| !p.cdn_key.is_empty())
                .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
                .collect();

            // Carry the reply quote (Signal-style) so the recipient's reply
            // bubble renders. Channel/group sender-key deliveries used to drop
            // it (MISSING-FEATURES #9) — the wire quote rode in the DataMessage
            // but this store path never read it. Shared with the DM inbound
            // paths (dm.rs / gateway::store_data_message).
            crate::services::gateway::inbound::apply_inbound_quote(
                &mut message,
                data_message.quote.as_ref(),
                message_repo,
            );

            // Store message with deduplication
            match message_repo.insert_or_ignore(&message) {
                Ok(true) => {
                    tracing::debug!(
                        message_id = %message.id,
                        channel_id = %channel_id,
                        sender_id = %sender_id,
                        "Stored channel message locally"
                    );

                    // Disappearing messages — RECIPIENT local half for
                    // channel/group sender-key messages. If the sender armed
                    // the E2E ttl marker (V29), track our local copy so the
                    // reaper removes it once the timer elapses. Best-effort.
                    if data_message.ephemeral && data_message.ephemeral_ttl_seconds > 0 {
                        if let Err(e) =
                            ephemeral_repo.insert_armed(&message.id, data_message.ephemeral_ttl_seconds)
                        {
                            tracing::warn!(
                                message_id = %message.id,
                                error = %e,
                                "failed to arm local disappearing timer for inbound channel message"
                            );
                        }
                    }

                    // Increment unread count
                    if let Ok(new_count) = channel_repo.increment_unread_count(&channel_id) {
                        tracing::debug!(
                            channel_id = %channel_id,
                            unread_count = new_count,
                            "Incremented channel unread count"
                        );
                    }

                    // Emit ChannelMessageReceived with decrypted content for UI
                    let _ = event_tx.send(InboxEvent::ChannelMessageReceived {
                        channel_id,
                        sender_id: *sender_id,
                        message_id: msg.message_guid.clone(),
                        text: data_message.body.clone(),
                        timestamp: data_message.timestamp,
                        attachments: data_message.attachments.clone(),
                    });

                    let _ = event_tx.send(InboxEvent::MessageReceived {
                        message_id: msg.message_guid.clone(),
                        envelope_type: EnvelopeType::SealedSender,
                    });
                    let _ = event_tx.send(InboxEvent::MessageProcessed {
                        message_id: msg.message_guid.clone(),
                    });

                    // Emit domain event so notification listener + state
                    // handlers learn about the fresh group message. Only
                    // on Ok(true) (new insert) — duplicates fall through
                    // Ok(false) arm. Mirror of the retry-decrypt path in
                    // services/message/group.rs:284 which used to be the
                    // only branch that emitted, causing fresh messages to
                    // silently miss the desktop toast.
                    if let Some(tx) = domain_tx {
                        let _ = tx.send(DomainEvent::MessageReceived {
                            message: message.clone(),
                        });
                    }

                    let mut s = stats.write();
                    s.messages_received += 1;
                    s.messages_processed += 1;
                }
                Ok(false) => {
                    tracing::debug!(
                        message_id = %message.id,
                        "Channel message already exists (skipped duplicate)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        message_id = %message.id,
                        error = %e,
                        "Failed to store channel message"
                    );
                }
            }
        }
        Err(e) => {
            tracing::error!(
                message_id = %msg.message_guid,
                error = %e,
                "Failed to parse DataMessage from decrypted content"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("Failed to parse DataMessage: {}", e),
            });
            stats.write().messages_failed += 1;
        }
    }
}

/// Process an identified sender message (PreKey or Ciphertext).
///
/// This is the fallback path when sealed sender is not used.
/// The server knows the sender (from Basic auth) and includes source_user_id.
#[allow(clippy::too_many_arguments)]
async fn process_identified_sender_message(
    msg: &DeviceMessage,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    retry_budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::AckDecision;
    // Self-send detection: skip messages from ourselves
    // This prevents "Key already requested" errors when trying to decrypt our own messages
    // (we don't have a valid session with ourselves, and Kyber keys were consumed during encryption)
    if !msg.source_user_id.is_empty()
        && !msg.destination_user_id.is_empty()
        && msg.source_user_id == msg.destination_user_id
    {
        tracing::debug!(
            message_id = %msg.message_guid,
            user_id = %msg.source_user_id,
            "[INBOX] Self-send detected in identified sender message, skipping decryption"
        );
        // Mark as processed (not failed) - self-sends are expected during multi-device sync
        let _ = event_tx.send(InboxEvent::MessageProcessed {
            message_id: msg.message_guid.clone(),
        });
        stats.write().messages_processed += 1;
        retry_budget.forget(&msg.message_guid);
        return AckDecision::Ack;
    }

    // Parse sender address
    let sender_id: UserId = match msg.source_user_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(
                message_id = %msg.message_guid,
                source_user_id = %msg.source_user_id,
                error = %e,
                "[INBOX] Invalid source_user_id in identified sender message"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("Invalid source_user_id: {}", e),
            });
            stats.write().messages_failed += 1;
            // Deterministic in the bytes, and with no parseable sender there is
            // no conversation to mark. Retaining it would replay forever.
            retry_budget.forget(&msg.message_guid);
            return AckDecision::Ack;
        }
    };

    // Decrypt using regular Signal Protocol (not sealed sender)
    match message_gateway
        .decrypt_identified_message(&msg.envelope, &sender_id, msg.source_device_id)
        .await
    {
        Ok(SealedSenderResult::KeyExchange {
            sender_id,
            conversation_id,
            ephemeral_public_key,
            timestamp,
        }) => {
            // Emit DomainEvent for KeyExchange so DM bootstrap handler can process it
            if let Some(tx) = domain_tx {
                let conv_id: crate::domain::ChannelId = conversation_id
                    .parse()
                    .unwrap_or_else(|_| {
                        let hash = sha2::Sha256::digest(conversation_id.as_bytes());
                        let mut uuid_bytes = [0u8; 16];
                        uuid_bytes.copy_from_slice(&hash[..16]);
                        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
                        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
                        crate::domain::ChannelId::from_uuid(uuid::Uuid::from_bytes(uuid_bytes))
                    });

                let _ = tx.send(DomainEvent::KeyExchangeReceived {
                    sender_id,
                    conversation_id: conv_id,
                    ephemeral_public_key,
                    timestamp,
                });
                tracing::info!(
                    message_id = %msg.message_guid,
                    sender_id = %sender_id,
                    "Emitted KeyExchangeReceived domain event for DM bootstrap (identified sender)"
                );
            } else {
                tracing::warn!(
                    message_id = %msg.message_guid,
                    "Received KeyExchange but no domain event sender configured"
                );
            }

            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(SealedSenderResult::MessageStored) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                "[INBOX] Identified sender message stored"
            );
            let _ = event_tx.send(InboxEvent::MessageReceived {
                message_id: msg.message_guid.clone(),
                envelope_type: EnvelopeType::PrekeyMessage,
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            let mut s = stats.write();
            s.messages_received += 1;
            s.messages_processed += 1;
        }
        Ok(SealedSenderResult::DataMessageReceived {
            sender_id: msg_sender,
            channel_id,
            message_id: msg_id,
            body,
            timestamp,
            attachments,
        }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %msg_sender,
                channel_id = %channel_id,
                "[INBOX] DataMessage received via identified sender"
            );
            let _ = event_tx.send(InboxEvent::ChannelMessageReceived {
                channel_id,
                sender_id: msg_sender,
                message_id: msg_id.to_string(),
                text: body,
                timestamp,
                attachments,
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            let mut s = stats.write();
            s.messages_received += 1;
            s.messages_processed += 1;
        }
        Ok(SealedSenderResult::ProfileKeyReceived { sender_id }) => {
            // Handshake is always sent sealed-sender (`send_sealed_dm_content`),
            // so this path is unexpected, but handle it the same way for
            // exhaustiveness/robustness — the gateway already stored the key.
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                "[INBOX] Profile-key handshake received via identified sender (no message)"
            );
            let _ = event_tx.send(InboxEvent::ProfileKeyReceived { sender_id });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(SealedSenderResult::SenderKeyRequested { requester_id, channel_id }) => {
            // SenderKeyRequest via identified sender (unusual but handled)
            tracing::info!(
                message_id = %msg.message_guid,
                requester_id = %requester_id,
                channel_id = %channel_id,
                "[INBOX] Received SenderKeyRequest via identified sender"
            );
            let _ = event_tx.send(InboxEvent::RespondWithSkdm {
                channel_id,
                requester_id,
                skdm_bytes: vec![], // Empty - handler will create SKDM
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        // SKDM via identified sender (unusual — SKDMs normally ride sealed
        // sender — but drain parked group messages just the same, F14c).
        Ok(SealedSenderResult::SenderKeyProcessed {
            sender_id,
            sender_device_id,
            channel_id,
            distribution_message,
            timestamp,
        }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                sender_device_id,
                channel_id = %channel_id,
                "[INBOX] SKDM processed via identified sender — emitting SenderKeyReceived"
            );
            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::SenderKeyReceived {
                    sender_id,
                    sender_device_id,
                    channel_id,
                    distribution_message,
                    timestamp,
                });
            }
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(SealedSenderResult::MailboxAnnouncement { sender_id, channel_id, announcement }) => {
            // MailboxAnnouncement via identified sender (unusual - should use sealed sender)
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                channel_id = %channel_id,
                "[INBOX] Received GroupMailboxAddress via identified sender (unexpected path)"
            );
            let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                channel_id,
                member_id: sender_id,
                mailbox_pk: announcement.mailbox_pk.clone(),
                timestamp: announcement.timestamp,
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        // AnonGroupMessage should come via group mailbox, not identified sender
        Ok(SealedSenderResult::AnonGroupMessageReceived { .. }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                "[INBOX] AnonGroupMessage via identified sender (wrong path)"
            );
            stats.write().messages_failed += 1;
        }
        // AnonGroupJoinAck should come via group mailbox, not identified sender
        Ok(SealedSenderResult::AnonGroupJoinAckReceived { .. }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                "[INBOX] AnonGroupJoinAck via identified sender (wrong path)"
            );
            stats.write().messages_failed += 1;
        }
        Ok(SealedSenderResult::ControlContent { sender_id: actual_sender, content }) => {
            if let Some(inner) = content.content {
                dispatch_control_content(
                    inner,
                    &actual_sender,
                    &msg.message_guid,
                    event_tx,
                    stats,
                );
            }
        }
        // Blocked. Drain the envelope and say nothing: no InboxEvent (an
        // event is how the message reaches the UI), no stats bump that would
        // put it in a badge count, no marker. `messages_processed` is not
        // incremented either — nothing was processed.
        Ok(SealedSenderResult::DroppedBlockedSender { sender_id }) => {
            return blocked_sender_decision(sender_id, &msg.message_guid, retry_budget);
        }
        // 1:1 self-heal results only arise on the SEALED path
        // (process_incoming_message): the sender identity there is
        // authenticated by the sealed-sender certificate. The identified
        // path's sender is server-claimed, so no DEM is built for it.
        Ok(SealedSenderResult::DecryptionErrorNeedsReport { .. })
        | Ok(SealedSenderResult::SessionHealed { .. }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                "[INBOX] Session self-heal result on identified path (unexpected)"
            );
            stats.write().messages_failed += 1;
        }
        Err(e) => {
            tracing::error!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                error = %e,
                "[INBOX] Failed to decrypt identified sender message"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: e.to_string(),
            });
            {
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
            }
            // The sender on this path is server-claimed rather than proved, but
            // it is the only conversation this envelope can belong to, and the
            // alternative was acking a `SessionNotFound` — deleting a message
            // that would have decrypted once the session existed.
            return device_failure_decision(
                Some(sender_id),
                msg.server_timestamp,
                &e,
                message_repo,
                domain_tx,
                &msg.message_guid,
                retry_budget,
            );
        }
    }

    // Every other arm handled the message.
    retry_budget.forget(&msg.message_guid);
    AckDecision::Ack
}

/// Process an ECIES init message (NDSS'21 private contact).
///
/// These messages are HPKE-encrypted to our identity key, containing:
/// - Conversation ID
/// - Sender's mailbox public key
/// - Sender's identity key
/// - Sender's delivery token
/// - Optional intro message
async fn process_ecies_init_message(
    msg: &DeviceMessage,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::AckDecision;
    use crate::crypto::ecies;
    use crate::domain::anon_dm_init::AnonDmInitMessage;

    // Get our identity key for decryption
    let identity_key = match message_gateway.get_identity_key().await {
        Ok(key) => key,
        Err(e) => {
            tracing::error!(
                message_guid = %msg.message_guid,
                error = %e,
                "[INBOX_ECIES] Failed to get identity key for decryption"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("Failed to get identity key: {}", e),
            });
            stats.write().messages_failed += 1;
            return ecies_init_decision(&EciesInitFailure::IdentityKeyUnavailable(&e));
        }
    };

    // Parse ECIES message structure
    let ecies_msg = match ecies::EciesMessage::from_bytes(&msg.envelope) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(
                message_guid = %msg.message_guid,
                error = %e,
                "[INBOX_ECIES] Failed to parse ECIES message"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("Invalid ECIES message: {}", e),
            });
            stats.write().messages_failed += 1;
            return ecies_init_decision(&EciesInitFailure::Malformed("ECIES envelope would not parse"));
        }
    };

    // Decrypt using our X25519 private key (already 32 bytes from get_identity_key)
    let plaintext = match ecies::decrypt(&identity_key, &ecies_msg) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                message_guid = %msg.message_guid,
                error = %e,
                "[INBOX_ECIES] Failed to decrypt ECIES message"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("ECIES decryption failed: {}", e),
            });
            stats.write().messages_failed += 1;
            return ecies_init_decision(&EciesInitFailure::Malformed("HPKE open failed against our identity key"));
        }
    };

    // Parse AnonDmInitMessage
    let init_msg = match AnonDmInitMessage::from_bytes(&plaintext) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(
                message_guid = %msg.message_guid,
                error = %e,
                "[INBOX_ECIES] Failed to parse AnonDmInitMessage"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("Invalid init message: {}", e),
            });
            stats.write().messages_failed += 1;
            return ecies_init_decision(&EciesInitFailure::Malformed("AnonDmInitMessage would not parse"));
        }
    };

    tracing::info!(
        message_guid = %msg.message_guid,
        conversation_id = %init_msg.conversation_id,
        sender_id = %init_msg.sender_id,
        intro_len = init_msg.intro_message.len(),
        "[INBOX_ECIES] Successfully decrypted NDSS'21 private contact request"
    );

    // Emit domain event for the anon DM handler to process
    if let Some(tx) = domain_tx {
        let event = DomainEvent::AnonDmInitReceived {
            conversation_id: init_msg.conversation_id,
            sender_id: init_msg.sender_id,
            sender_display_name: init_msg.sender_display_name.clone(),
            sender_avatar: init_msg.sender_avatar.clone(),
            peer_mailbox_pk: init_msg.mailbox_pk,
            peer_identity_key: init_msg.identity_key,
            peer_delivery_token: init_msg.delivery_token,
            intro_message: init_msg.intro_message.clone(),
            server_timestamp: msg.server_timestamp,
        };
        match tx.send(event) {
            Ok(receivers) => {
                tracing::info!(
                    conversation_id = %init_msg.conversation_id,
                    receivers = receivers,
                    "[INBOX_ECIES] Emitted AnonDmInitReceived event"
                );
            }
            Err(e) => {
                tracing::error!(
                    conversation_id = %init_msg.conversation_id,
                    error = %e,
                    "[INBOX_ECIES] Failed to emit AnonDmInitReceived event (no receivers?)"
                );
            }
        }
    } else {
        tracing::warn!(
            conversation_id = %init_msg.conversation_id,
            "[INBOX_ECIES] No domain_tx channel available - event not emitted!"
        );
    }

    let _ = event_tx.send(InboxEvent::MessageReceived {
        message_id: msg.message_guid.clone(),
        envelope_type: EnvelopeType::EciesInit,
    });
    stats.write().messages_processed += 1;
    AckDecision::Ack
}

/// Process a sealed sender message.
#[allow(clippy::too_many_arguments)]
async fn process_sealed_sender_message(
    msg: &DeviceMessage,
    pipeline: &MessageValidationPipeline,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    retry_budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{self, AckDecision};
    let sealed_sender_msg = SealedSenderMessage {
        sealed_body: msg.envelope.clone(),
        server_key_id: 0,
        timestamp: msg.server_timestamp,
    };

    match message_gateway
        .process_incoming_message(sealed_sender_msg.clone())
        .await
    {
        // Blocked. This is the arm that matters most: on the sealed path the
        // sender certificate is the ONLY thing that ever names the sender —
        // `DeviceMessage.SourceUserID` is empty here — so the server could not
        // have filtered this envelope even in principle. Drain it, emit
        // nothing, store nothing.
        Ok(SealedSenderResult::DroppedBlockedSender { sender_id }) => {
            return blocked_sender_decision(sender_id, &msg.message_guid, retry_budget);
        }
        Ok(SealedSenderResult::KeyExchange {
            sender_id,
            conversation_id,
            ephemeral_public_key,
            timestamp,
        }) => {
            // Emit DomainEvent for KeyExchange so DM bootstrap handler can process it
            if let Some(tx) = domain_tx {
                // Parse conversation_id as ChannelId
                let conv_id: crate::domain::ChannelId = conversation_id
                    .parse()
                    .unwrap_or_else(|_| {
                        // Fallback: generate deterministic ID from string
                        let hash = sha2::Sha256::digest(conversation_id.as_bytes());
                        let mut uuid_bytes = [0u8; 16];
                        uuid_bytes.copy_from_slice(&hash[..16]);
                        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
                        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
                        crate::domain::ChannelId::from_uuid(uuid::Uuid::from_bytes(uuid_bytes))
                    });

                let _ = tx.send(DomainEvent::KeyExchangeReceived {
                    sender_id,
                    conversation_id: conv_id,
                    ephemeral_public_key,
                    timestamp,
                });
                tracing::info!(
                    message_id = %msg.message_guid,
                    sender_id = %sender_id,
                    "Emitted KeyExchangeReceived domain event for DM bootstrap"
                );
            } else {
                tracing::warn!(
                    message_id = %msg.message_guid,
                    "Received KeyExchange but no domain event sender configured"
                );
            }

            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(SealedSenderResult::SenderKeyRequested { requester_id, channel_id }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                requester_id = %requester_id,
                channel_id = %channel_id,
                "[INBOX] Received SenderKeyRequest via sealed sender, emitting event"
            );
            // Emit event to respond with our SKDM - handler will create and send it
            let _ = event_tx.send(InboxEvent::RespondWithSkdm {
                channel_id,
                requester_id,
                skdm_bytes: vec![], // Empty - handler will create SKDM
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(SealedSenderResult::MailboxAnnouncement { sender_id, channel_id, announcement }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                channel_id = %channel_id,
                "[INBOX] Received GroupMailboxAddress via sealed sender, emitting event"
            );
            // Emit event for group mailbox handler to process
            let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                channel_id,
                member_id: sender_id,
                mailbox_pk: announcement.mailbox_pk.clone(),
                timestamp: announcement.timestamp,
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        // SKDM installed. Fire SenderKeyReceived so the crypto-maintenance
        // listener drains any group messages that arrived from this
        // (sender, device, channel) BEFORE the key (the F14c first-message
        // race). This is the trigger that was missing: the buffer +
        // retry_pending_group_decrypts machinery already existed, but nothing
        // ever emitted this event, so a parked first message stayed lost.
        Ok(SealedSenderResult::SenderKeyProcessed {
            sender_id,
            sender_device_id,
            channel_id,
            distribution_message,
            timestamp,
        }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                sender_device_id,
                channel_id = %channel_id,
                "[INBOX] SKDM processed — emitting SenderKeyReceived to drain parked group messages"
            );
            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::SenderKeyReceived {
                    sender_id,
                    sender_device_id,
                    channel_id,
                    distribution_message,
                    timestamp,
                });
            } else {
                tracing::warn!(
                    message_id = %msg.message_guid,
                    "SKDM processed but no domain event sender configured; parked \
                     group messages will not self-heal"
                );
            }
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        // AnonGroupMessage should come via group mailbox polling, not device inbox
        Ok(SealedSenderResult::AnonGroupMessageReceived { .. }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                "[INBOX] AnonGroupMessage via device sealed sender (wrong path)"
            );
            stats.write().messages_failed += 1;
        }
        // AnonGroupJoinAck should come via group mailbox polling, not device inbox
        Ok(SealedSenderResult::AnonGroupJoinAckReceived { .. }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                "[INBOX] AnonGroupJoinAck via device sealed sender (wrong path)"
            );
            stats.write().messages_failed += 1;
        }
        Ok(SealedSenderResult::ControlContent { sender_id: actual_sender, content }) => {
            if let Some(inner) = content.content {
                dispatch_control_content(
                    inner,
                    &actual_sender,
                    &msg.message_guid,
                    event_tx,
                    stats,
                );
            }
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
        }
        // RECEIVER half of the 1:1 self-heal: this sender's inner message
        // could not be decrypted (desynced session). Ship the
        // DecryptionErrorMessage back to them via the InboxEventProcessor
        // (MessageService::send_decryption_error_report). The wedged
        // ciphertext itself is unrecoverable (Signal semantics) — the heal
        // applies to the SESSION so the next message decrypts.
        Ok(SealedSenderResult::DecryptionErrorNeedsReport {
            sender_id,
            sender_device_id,
            dem_serialized,
        }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                sender_device_id,
                "[INBOX] 1:1 decrypt failed — dispatching DecryptionErrorMessage report to sender"
            );
            let _ = event_tx.send(InboxEvent::DmDecryptionErrorNeedsReport {
                peer_id: sender_id,
                peer_device_id: sender_device_id,
                dem_serialized,
            });
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: "session desync — DecryptionErrorMessage reported to sender".to_string(),
            });

            {
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
            }

            // The report above heals the SESSION; this message is dead either
            // way, so the envelope is drained — but only once the marker is
            // durably written. If that write fails we RETAIN instead: deleting
            // the server's copy of a message we can neither show nor explain
            // loses the message and the evidence at once.
            return ack_policy::record_marker_then_ack(
                message_repo,
                domain_tx,
                ack_policy::dm_target(sender_id, msg.server_timestamp),
                ack_policy::FailureMarker::DeliveryIssue,
            );
        }
        // SENDER half of the 1:1 self-heal: a peer's DecryptionErrorMessage
        // about a message we sent was processed (session archived when
        // `archived`). Surface on the domain bus for observability.
        Ok(SealedSenderResult::SessionHealed {
            peer_id,
            peer_device_id,
            archived,
        }) => {
            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::DmSessionHealed {
                    peer_user_id: peer_id,
                    peer_device_id,
                    archived,
                });
            }
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        Ok(SealedSenderResult::DataMessageReceived {
            sender_id: msg_sender,
            channel_id,
            message_id: msg_id,
            body,
            timestamp,
            attachments,
        }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %msg_sender,
                channel_id = %channel_id,
                "[INBOX] DataMessage received via sealed sender"
            );
            // Emit UI event so message appears in chat
            let _ = event_tx.send(InboxEvent::ChannelMessageReceived {
                channel_id,
                sender_id: msg_sender,
                message_id: msg_id.to_string(),
                text: body,
                timestamp,
                attachments,
            });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            let mut s = stats.write();
            s.messages_received += 1;
            s.messages_processed += 1;
        }
        Ok(SealedSenderResult::ProfileKeyReceived { sender_id }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                "[INBOX] Profile-key handshake received via sealed sender (no message)"
            );
            // No bubble to render — gateway already stored the profile key.
            // Hand off to InboxEventProcessor to fetch + decrypt the sender's
            // profile and refresh the UI.
            let _ = event_tx.send(InboxEvent::ProfileKeyReceived { sender_id });
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        // `MessageStored` is the gateway's "handled, caller need do no more":
        // either the row is already in the database (`store_data_message` →
        // `message_repo.insert`) or there was nothing to store at all (a
        // duplicate inner message, a `DecryptionErrorMessage` addressed to
        // another of our devices, a content type with no bubble). The pipeline
        // below therefore decides nothing about the message's survival — it
        // dedups and orders a SYNTHETIC proto (`channel_id: "pending"`, body =
        // the sealed envelope) purely to gate the UI events. So every arm of it
        // acks, including `Buffered` and `Dropped`: the payload is on disk
        // before we get here and there is nothing left to lose.
        //
        // Worth knowing about the orderer it feeds, though: `get_ready` and
        // `flush_stale` have no production callers, so anything it buffers is
        // never drained. That leaks UI events (and memory), not messages.
        Ok(SealedSenderResult::MessageStored) => {
            let envelope_type = EnvelopeType::SealedSender;

            // Create a Message for pipeline processing
            let proto_msg = crate::proto::chat_v1::Message {
                id: msg.message_guid.clone(),
                channel_id: "pending".to_string(),
                author_id: msg.source_user_id.clone(),
                timestamp: msg.server_timestamp,
                content: Some(crate::proto::chat_v1::message::Content {
                    r#type: Some(
                        crate::proto::chat_v1::message::content::Type::SealedSender(
                            sealed_sender_msg,
                        ),
                    ),
                }),
                ..Default::default()
            };

            match pipeline.process(proto_msg) {
                PipelineResult::Ready(_) => {
                    let _ = event_tx.send(InboxEvent::MessageReceived {
                        message_id: msg.message_guid.clone(),
                        envelope_type,
                    });
                    let _ = event_tx.send(InboxEvent::MessageProcessed {
                        message_id: msg.message_guid.clone(),
                    });
                    let mut s = stats.write();
                    s.messages_received += 1;
                    s.messages_processed += 1;
                }
                PipelineResult::Buffered => {
                    let _ = event_tx.send(InboxEvent::MessageReceived {
                        message_id: msg.message_guid.clone(),
                        envelope_type,
                    });
                    stats.write().messages_received += 1;
                }
                PipelineResult::Duplicate => {
                    tracing::debug!(message_id = %msg.message_guid, "Duplicate message skipped");
                }
                PipelineResult::Invalid(err) => {
                    let _ = event_tx.send(InboxEvent::MessageFailed {
                        message_id: msg.message_guid.clone(),
                        error: err.to_string(),
                    });
                    let mut s = stats.write();
                    s.messages_received += 1;
                    s.messages_failed += 1;
                }
                PipelineResult::Dropped => {
                    tracing::warn!(message_id = %msg.message_guid, "Message dropped (buffer full)");
                    let mut s = stats.write();
                    s.messages_received += 1;
                    s.messages_failed += 1;
                }
            }
        }
        // The peer's identity key changed. This envelope is already gone (the
        // batch is acked whatever the outcome) and — unlike a desync — nothing
        // will bring it back: `classify_inner_failure` refuses to treat this as
        // a desync precisely because no session-layer action changes which key
        // is pinned. Without an entry here the message that WEDGED the
        // conversation is the one message with no trace at all, which is how
        // this landed with detection working and the user seeing nothing.
        //
        // The change itself was already parked by `note_identity_change` on the
        // decrypt path, so nothing is recorded here beyond what the user sees.
        Err(ServiceError::Crypto(CryptoError::UntrustedIdentity {
            ref peer_id,
            device_id: peer_device_id,
            ref attribution,
            ..
        })) => {
            let attributed = attribution.is_authenticated();
            tracing::warn!(
                message_id = %msg.message_guid,
                peer_id = %peer_id,
                peer_device_id,
                attributed,
                "[INBOX] peer identity key changed — conversation wedged until accepted"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: "peer identity changed — awaiting the user's decision".to_string(),
            });

            {
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
            }

            let Ok(peer) = peer_id.parse::<UserId>() else {
                tracing::warn!(
                    peer_id = %peer_id,
                    "[INBOX] identity change for an unparseable peer id — no marker \
                     written, but the envelope is still drained: a resend re-presents \
                     the same key, so retaining it replays forever"
                );
                return AckDecision::Ack;
            };
            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::PeerIdentityChanged {
                    peer_user_id: peer,
                    attributed,
                });
            }
            // `IdentityChanged`, never `DeliveryIssue`: no DecryptionErrorMessage
            // was sent, a resend re-presents the same key and fails identically,
            // and only a human accepting it clears the wedge.
            return ack_policy::record_marker_then_ack(
                message_repo,
                domain_tx,
                ack_policy::dm_target(peer, msg.server_timestamp),
                ack_policy::FailureMarker::IdentityChanged { attributed },
            );
        }
        Err(e) => {
            let e: ServiceError = e;
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: e.to_string(),
            });
            {
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
            }
            tracing::error!(message_id = %msg.message_guid, error = %e, "Failed to decrypt message");
            // Was: log and fall through, while `provider` acked the guid anyway
            // — the generic branch destroyed the message with no marker at all.
            // A sealed sender envelope that failed to open authenticated
            // nobody, so the sender is only known when the error carries one.
            let peer = match &e {
                ServiceError::Crypto(CryptoError::UntrustedIdentity { peer_id, .. }) => {
                    peer_id.parse::<UserId>().ok()
                }
                ServiceError::Crypto(CryptoError::SessionDesync { sender_uuid, .. }) => {
                    sender_uuid.parse::<UserId>().ok()
                }
                _ => msg.source_user_id.parse::<UserId>().ok(),
            };
            return device_failure_decision(
                peer,
                msg.server_timestamp,
                &e,
                message_repo,
                domain_tx,
                &msg.message_guid,
                retry_budget,
            );
        }
    }

    // Every other arm handled the message.
    retry_budget.forget(&msg.message_guid);
    AckDecision::Ack
}

/// Dispatch channel control content based on type.
///
/// Parses the Content proto inner type and emits appropriate channel events.
pub fn dispatch_control_content(
    inner: content::Content,
    sender_id: &UserId,
    message_guid: &str,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
) {
    match inner {
        content::Content::Typing(typing) => {
            tracing::info!(
                sender_id = %sender_id,
                action = typing.action,
                channel_id = %typing.channel_id,
                "DEVICE_INBOX: Received Typing content in control dispatch"
            );
            if typing.channel_id.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm). Emit the DM event.
                let _ = event_tx.send(InboxEvent::DmTyping {
                    peer_id: *sender_id,
                    action: typing.action,
                });
            } else if let Ok(channel_id) = typing.channel_id.parse::<ChannelId>() {
                tracing::info!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    action = typing.action,
                    "DEVICE_INBOX: Emitting ChannelTyping event"
                );
                let _ = event_tx.send(InboxEvent::ChannelTyping {
                    channel_id,
                    sender_id: *sender_id,
                    action: typing.action,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %typing.channel_id,
                    "DEVICE_INBOX: Channel typing has invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::Reaction(reaction) => {
            tracing::debug!(
                sender_id = %sender_id,
                emoji = %reaction.emoji,
                remove = reaction.remove,
                target_author = %reaction.target_author_aci,
                target_ts = reaction.target_sent_timestamp,
                "Channel reaction received"
            );
            // Parse channel_id from encoded target_author_aci if present
            if let Some((channel_id_str, actual_author)) =
                parse_channel_reaction_author(&reaction.target_author_aci)
            {
                if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                    let _ = event_tx.send(InboxEvent::ChannelReaction {
                        channel_id,
                        sender_id: *sender_id,
                        emoji: reaction.emoji,
                        remove: reaction.remove,
                        target_author_aci: actual_author,
                        target_sent_timestamp: reaction.target_sent_timestamp,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id_str,
                        "Channel reaction received with invalid channel_id"
                    );
                }
            } else {
                // No "channel:" prefix => channel-free DM (ServerDm) reaction.
                let _ = event_tx.send(InboxEvent::DmReaction {
                    peer_id: *sender_id,
                    emoji: reaction.emoji,
                    remove: reaction.remove,
                    target_author_aci: reaction.target_author_aci,
                    target_sent_timestamp: reaction.target_sent_timestamp,
                });
            }
            stats.write().messages_processed += 1;
        }
        content::Content::Pin(pin) => {
            if pin.channel_id.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm) pin.
                let _ = event_tx.send(InboxEvent::DmPin {
                    peer_id: *sender_id,
                    target_author_aci: pin.target_author_aci,
                    target_sent_timestamp: pin.target_sent_timestamp,
                    unpin: pin.unpin,
                });
            } else if let Ok(channel_id) = pin.channel_id.parse::<ChannelId>() {
                tracing::debug!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    target_author = %pin.target_author_aci,
                    target_ts = pin.target_sent_timestamp,
                    unpin = pin.unpin,
                    "Channel pin received"
                );
                let _ = event_tx.send(InboxEvent::ChannelPin {
                    channel_id,
                    sender_id: *sender_id,
                    target_author_aci: pin.target_author_aci,
                    target_sent_timestamp: pin.target_sent_timestamp,
                    unpin: pin.unpin,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %pin.channel_id,
                    "Channel pin received with invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::Edit(edit) => {
            let channel_id_str = edit
                .data_message
                .as_ref()
                .map(|dm| dm.channel_id.clone())
                .unwrap_or_default();
            let new_body = edit
                .data_message
                .as_ref()
                .map(|dm| dm.body.clone())
                .unwrap_or_default();
            let new_body_preview = if new_body.len() > 50 {
                format!("{}...", &new_body[..47])
            } else {
                new_body.clone()
            };

            if channel_id_str.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm) edit.
                let _ = event_tx.send(InboxEvent::DmEdit {
                    peer_id: *sender_id,
                    target_sent_timestamp: edit.target_sent_timestamp,
                    new_body,
                });
            } else if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                tracing::debug!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    target_ts = edit.target_sent_timestamp,
                    preview = %new_body_preview,
                    "Channel edit received"
                );
                let _ = event_tx.send(InboxEvent::ChannelEdit {
                    channel_id,
                    sender_id: *sender_id,
                    target_sent_timestamp: edit.target_sent_timestamp,
                    new_body,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %channel_id_str,
                    "Channel edit received with invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        content::Content::DataMessage(dm) => {
            if let Some(delete) = dm.delete {
                if dm.channel_id.is_empty() {
                    // Empty channel_id => channel-free DM (ServerDm) delete-for-everyone.
                    let _ = event_tx.send(InboxEvent::DmDeleteForEveryone {
                        peer_id: *sender_id,
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                } else if let Ok(channel_id) = dm.channel_id.parse::<ChannelId>() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        target_ts = delete.target_sent_timestamp,
                        "Channel delete for everyone received"
                    );
                    let _ = event_tx.send(InboxEvent::ChannelDeleteForEveryone {
                        channel_id,
                        sender_id: *sender_id,
                        target_sent_timestamp: delete.target_sent_timestamp,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %dm.channel_id,
                        "Channel delete for everyone received with invalid channel_id"
                    );
                }
                stats.write().messages_processed += 1;
            } else {
                tracing::debug!(
                    message_id = %message_guid,
                    "Unexpected DataMessage in control content (expected delete)"
                );
            }
        }
        content::Content::DeleteForMeSync(sync) => {
            tracing::debug!(
                conversation_id = %sync.conversation_id,
                target_author = %sync.target_author_aci,
                target_ts = sync.target_sent_timestamp,
                "Channel delete for me sync"
            );
            let _ = event_tx.send(InboxEvent::ChannelDeleteForMe {
                conversation_id: sync.conversation_id,
                target_author_aci: sync.target_author_aci,
                target_sent_timestamp: sync.target_sent_timestamp,
            });
            stats.write().messages_processed += 1;
        }
        content::Content::Receipt(receipt) => {
            if receipt.channel_id.is_empty() {
                // Empty channel_id => channel-free DM (ServerDm) receipt.
                let _ = event_tx.send(InboxEvent::DmReceipt {
                    peer_id: *sender_id,
                    receipt_type: receipt.r#type,
                    timestamps: receipt.timestamps,
                });
            } else if let Ok(channel_id) = receipt.channel_id.parse::<ChannelId>() {
                tracing::debug!(
                    sender_id = %sender_id,
                    channel_id = %channel_id,
                    receipt_type = receipt.r#type,
                    timestamps = ?receipt.timestamps,
                    "Channel receipt received"
                );
                let _ = event_tx.send(InboxEvent::ChannelReceipt {
                    channel_id,
                    sender_id: *sender_id,
                    receipt_type: receipt.r#type,
                    timestamps: receipt.timestamps,
                });
            } else {
                tracing::warn!(
                    sender_id = %sender_id,
                    channel_id = %receipt.channel_id,
                    "Channel receipt received with invalid channel_id"
                );
            }
            stats.write().messages_processed += 1;
        }
        _ => {
            tracing::debug!(
                message_id = %message_guid,
                "Unhandled channel control content type"
            );
            stats.write().messages_processed += 1;
        }
    }
}

/// Parse channel_id and actual author from encoded reaction target_author_aci.
///
/// The Reaction proto doesn't have a channel_id field, so for channel reactions
/// we encode the channel context in target_author_aci as "channel:{channel_id}:{author_aci}".
fn parse_channel_reaction_author(target_author_aci: &str) -> Option<(String, String)> {
    if target_author_aci.starts_with("channel:") {
        let parts: Vec<&str> = target_author_aci.splitn(3, ':').collect();
        if parts.len() == 3 {
            return Some((parts[1].to_string(), parts[2].to_string()));
        }
    }
    None
}
