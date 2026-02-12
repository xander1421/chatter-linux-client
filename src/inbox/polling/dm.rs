//! DM mailbox polling.
//!
//! Handles polling and processing messages from DM (direct message) mailboxes
//! using two-way anonymous messaging.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::Message;
use crate::domain::UserId;
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::dm::{DmConversationManager, TwoWayAnonymity};
use crate::network::grpc::MailboxClient;
use crate::services::message_gateway::{DmContent, MessageGateway};
use crate::storage::repositories::settings_repo::keys;
use crate::storage::{ChannelRepository, MessageRepository, SettingsRepository};

/// Poll all DM mailboxes and process received messages.
///
/// DM messages are decrypted through the MessageGateway which handles:
/// - Signal Protocol layer (forward secrecy via Double Ratchet)
/// - Mailbox layer (sender anonymity via XChaCha20-Poly1305)
///
/// Decrypted messages are stored in the local database for persistence.
/// Unread counts are incremented for conversations not currently viewed.
/// KeyExchange messages (bootstrap responses) emit DomainEvent for the bootstrap handler.
#[allow(clippy::too_many_arguments)]
pub async fn poll_dm_mailboxes(
    two_way: &TwoWayAnonymity,
    conversation_manager: &DmConversationManager,
    mailbox_client: &Arc<tokio::sync::Mutex<MailboxClient>>,
    message_gateway: &Arc<MessageGateway>,
    event_tx: &broadcast::Sender<InboxEvent>,
    settings_repo: &Arc<SettingsRepository>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    // Get current user ID for DM channel derivation
    let current_user_id: Option<UserId> = settings_repo
        .get_string(keys::CURRENT_USER_ID)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());
    let mut client = mailbox_client.lock().await;

    match two_way.poll_all_mailboxes(&mut client).await {
        Ok(poll_results) => {
            let mut total_messages = 0u32;

            for result in poll_results {
                let peer_id = result.peer_id;

                // Collect message IDs for potential acknowledgment
                let message_ids: Vec<String> = result
                    .messages
                    .iter()
                    .map(|m| m.message_id.clone())
                    .collect();

                // Get shared secret for this conversation
                let shared_secret = match conversation_manager.load_conversation(&peer_id) {
                    Ok(Some(conv)) => conv.conversation_key.and_then(|k| k.shared_secret),
                    Ok(None) => {
                        tracing::warn!(peer_id = %peer_id, "No conversation state for DM");
                        None
                    }
                    Err(e) => {
                        tracing::error!(peer_id = %peer_id, error = %e, "Failed to load conversation");
                        None
                    }
                };

                // Process each message through the gateway
                for msg in result.messages {
                    total_messages += 1;

                    if let Some(ref secret) = shared_secret {
                        // Decrypt with Signal Protocol (forward secrecy via Double Ratchet)
                        match message_gateway
                            .process_dm_message(&msg.encrypted_envelope, secret)
                            .await
                        {
                            Ok((sender_id, dm_content)) => {
                                dispatch_dm_content(
                                    dm_content,
                                    &sender_id,
                                    peer_id,
                                    &msg.message_id,
                                    msg.server_timestamp,
                                    &current_user_id,
                                    event_tx,
                                    message_repo,
                                    channel_repo,
                                    domain_tx,
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    peer_id = %peer_id,
                                    message_id = %msg.message_id,
                                    error = %e,
                                    "DM decryption failed"
                                );
                                let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                                    peer_id,
                                    error: e.to_string(),
                                });
                            }
                        }
                    } else {
                        tracing::warn!(
                            peer_id = %peer_id,
                            message_id = %msg.message_id,
                            "No shared secret for DM decryption"
                        );
                        let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                            peer_id,
                            error: "No shared secret established".to_string(),
                        });
                    }
                }

                // Acknowledge processed messages
                if !message_ids.is_empty() {
                    if let Err(e) = two_way
                        .acknowledge_messages(&peer_id, message_ids, &mut client)
                        .await
                    {
                        tracing::warn!(peer_id = %peer_id, error = %e, "Failed to acknowledge DM messages");
                    }
                }
            }

            if total_messages > 0 {
                let _ = event_tx.send(InboxEvent::DmPollCompleted { total_messages });
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "DM mailbox poll failed (may be normal if no active conversations)");
        }
    }
}

/// Dispatch DM content based on type.
#[allow(clippy::too_many_arguments)]
fn dispatch_dm_content(
    dm_content: DmContent,
    sender_id: &UserId,
    peer_id: UserId,
    message_id: &str,
    server_timestamp: u64,
    current_user_id: &Option<UserId>,
    event_tx: &broadcast::Sender<InboxEvent>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    match dm_content {
        DmContent::Text { body, timestamp } => {
            // Store message in local database for persistence
            if let Some(current_user) = current_user_id {
                let dm_channel_id = ChannelId::from_dm_participants(current_user, sender_id);
                let msg_id = message_id
                    .parse::<MessageId>()
                    .unwrap_or_else(|_| MessageId::new());

                let mut message = Message::new_text(dm_channel_id, Some(*sender_id), body.clone());
                message.id = msg_id;

                match message_repo.insert_or_ignore(&message) {
                    Ok(true) => {
                        if let Ok(new_count) = channel_repo.increment_unread_count(&dm_channel_id) {
                            let _ = event_tx.send(InboxEvent::DmUnreadCountUpdated {
                                channel_id: dm_channel_id,
                                peer_id,
                                unread_count: new_count,
                            });
                        }
                    }
                    Ok(false) => {
                        tracing::debug!(peer_id = %peer_id, "DM duplicate skipped");
                    }
                    Err(e) => {
                        tracing::warn!(peer_id = %peer_id, error = %e, "Failed to store DM");
                    }
                }
            }

            let _ = event_tx.send(InboxEvent::DmReceived {
                peer_id,
                text: body,
                timestamp: if timestamp > 0 {
                    timestamp
                } else {
                    server_timestamp
                },
                message_id: message_id.to_string(),
            });
        }

        DmContent::Typing { action, .. } => {
            tracing::debug!(peer_id = %peer_id, action, "DM typing indicator");
            let _ = event_tx.send(InboxEvent::DmTyping { peer_id, action });
        }

        DmContent::Receipt {
            receipt_type,
            timestamps,
        } => {
            tracing::debug!(peer_id = %peer_id, receipt_type, count = timestamps.len(), "DM receipt");
            let _ = event_tx.send(InboxEvent::DmReceipt {
                peer_id,
                receipt_type,
                timestamps,
            });
        }

        DmContent::Reaction {
            emoji,
            remove,
            target_author_aci,
            target_sent_timestamp,
        } => {
            tracing::debug!(peer_id = %peer_id, emoji = %emoji, remove, "DM reaction");
            let _ = event_tx.send(InboxEvent::DmReaction {
                peer_id,
                emoji,
                remove,
                target_author_aci,
                target_sent_timestamp,
            });
        }

        DmContent::Pin {
            target_author_aci,
            target_sent_timestamp,
            unpin,
        } => {
            tracing::debug!(peer_id = %peer_id, unpin, "DM pin");
            let _ = event_tx.send(InboxEvent::DmPin {
                peer_id,
                target_author_aci,
                target_sent_timestamp,
                unpin,
            });
        }

        DmContent::Edit {
            target_sent_timestamp,
            new_body,
        } => {
            tracing::debug!(peer_id = %peer_id, target = target_sent_timestamp, "DM edit");
            let _ = event_tx.send(InboxEvent::DmEdit {
                peer_id,
                target_sent_timestamp,
                new_body,
            });
        }

        DmContent::DeleteForMe {
            conversation_id,
            target_author_aci,
            target_sent_timestamp,
        } => {
            tracing::debug!(conversation_id = %conversation_id, "DM delete for me sync");
            let _ = event_tx.send(InboxEvent::DmDeleteForMe {
                conversation_id,
                target_author_aci,
                target_sent_timestamp,
            });
        }

        DmContent::DeleteForEveryone {
            target_sent_timestamp,
        } => {
            tracing::debug!(
                peer_id = %peer_id,
                target_sent_timestamp = target_sent_timestamp,
                "DM delete for everyone received"
            );
            let _ = event_tx.send(InboxEvent::DmDeleteForEveryone {
                peer_id,
                target_sent_timestamp,
            });
        }

        DmContent::KeyExchange {
            conversation_id,
            ephemeral_pk,
            timestamp,
        } => {
            // Key exchange received in DM mailbox (bootstrap response from peer)
            tracing::debug!(
                peer_id = %peer_id,
                conversation_id = %conversation_id,
                key_len = ephemeral_pk.len(),
                "DM bootstrap key exchange received"
            );

            // Emit domain event for the bootstrap handler to process
            if let Some(tx) = domain_tx {
                // Parse conversation_id as ChannelId
                let conv_id: ChannelId = conversation_id
                    .parse()
                    .unwrap_or_else(|_| {
                        // Fallback: derive from peer
                        if let Some(current_user) = current_user_id {
                            ChannelId::from_dm_participants(current_user, sender_id)
                        } else {
                            ChannelId::new()
                        }
                    });

                let _ = tx.send(DomainEvent::KeyExchangeReceived {
                    sender_id: *sender_id,
                    conversation_id: conv_id,
                    ephemeral_public_key: ephemeral_pk,
                    timestamp,
                });
                tracing::info!(
                    peer_id = %peer_id,
                    "Emitted KeyExchangeReceived domain event from DM mailbox"
                );
            } else {
                tracing::warn!(
                    peer_id = %peer_id,
                    "Received KeyExchange in DM mailbox but no domain event sender configured"
                );
            }
        }
    }
}
