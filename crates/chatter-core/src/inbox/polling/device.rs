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
use crate::error::ServiceError;
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
) {
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
                    return;
                }
            };

            tracing::debug!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                source_device_id = msg.source_device_id,
                "[INBOX] Routing to sender key message handler"
            );

            // Route through message gateway
            process_sender_key_message(
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
            )
            .await;
            return;
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
        process_identified_sender_message(msg, event_tx, stats, message_gateway, domain_tx).await;
        return;
    }

    // Check for ECIES_INIT (HPKE-encrypted initial contact, NDSS'21 compliant)
    if msg.envelope_type == EnvelopeType::EciesInit as i32 {
        tracing::info!(
            message_guid = %msg.message_guid,
            envelope_len = msg.envelope.len(),
            "[INBOX] Processing as ECIES init message (NDSS'21 private contact)"
        );
        process_ecies_init_message(msg, event_tx, stats, message_gateway, domain_tx).await;
        return;
    }

    // Fall back to sealed sender processing
    tracing::info!(
        message_guid = %msg.message_guid,
        "[INBOX] Processing as sealed sender message"
    );
    process_sealed_sender_message(msg, pipeline, event_tx, stats, message_gateway, domain_tx).await;
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
) {
    tracing::info!(
        message_guid = %msg.message_guid,
        sender_id = %sender_id,
        source_device_id = msg.source_device_id,
        "[INBOX_SK] Processing sender key message"
    );

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
        }
    }
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
async fn process_identified_sender_message(
    msg: &DeviceMessage,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
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
        return;
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
            return;
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
            let mut s = stats.write();
            s.messages_received += 1;
            s.messages_failed += 1;
        }
    }
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
) {
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
            return;
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
            return;
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
            return;
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
            return;
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
}

/// Process a sealed sender message.
async fn process_sealed_sender_message(
    msg: &DeviceMessage,
    pipeline: &MessageValidationPipeline,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    let sealed_sender_msg = SealedSenderMessage {
        sealed_body: msg.envelope.clone(),
        server_key_id: 0,
        timestamp: msg.server_timestamp,
    };

    match message_gateway
        .process_incoming_message(sealed_sender_msg.clone())
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
        Err(e) => {
            let e: ServiceError = e;
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: e.to_string(),
            });
            let mut s = stats.write();
            s.messages_received += 1;
            s.messages_failed += 1;
            tracing::error!(message_id = %msg.message_guid, error = %e, "Failed to decrypt message");
        }
    }
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
