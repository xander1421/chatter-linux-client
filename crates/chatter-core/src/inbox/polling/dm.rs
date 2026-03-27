//! DM mailbox polling.
//!
//! Handles polling and processing messages from DM (direct message) mailboxes
//! using two-way anonymous messaging.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::crypto::envelope_crypto::decrypt_envelope;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::Message;
use crate::domain::UserId;
use crate::error::MailboxError;
use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::mailbox::dm::{ConversationKey, DmConversationManager, PeerMailbox, TwoWayAnonymity};
use crate::network::grpc::MailboxClient;
use crate::network::validation::MessageDeduplicator;
use crate::services::message_gateway::{DmContent, MessageGateway};
use crate::services::attachment::AttachmentService;
use crate::storage::repositories::settings_repo::keys;
use crate::storage::repositories::user_repo::UserRepository;
use crate::storage::{AttachmentRepository, ChannelRepository, MessageRepository, SettingsRepository};

/// Poll all DM mailboxes and process received messages.
///
/// DM messages are decrypted through the MessageGateway which handles:
/// - Signal Protocol layer (forward secrecy via Double Ratchet)
/// - Mailbox layer (sender anonymity via XChaCha20-Poly1305)
///
/// Decrypted messages are stored in the local database for persistence.
/// Unread counts are incremented for conversations not currently viewed.
/// KeyExchange messages (bootstrap responses) emit DomainEvent for the bootstrap handler.
///
/// The deduplicator prevents re-decryption of already-processed messages,
/// which would fail with "old counter" errors due to Signal's ratchet advancement.
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
    attachment_repo: &Arc<AttachmentRepository>,
    user_repo: &Arc<UserRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    deduplicator: &MessageDeduplicator,
) {
    // Get current user ID for DM channel derivation
    let current_user_id: Option<UserId> = settings_repo
        .get_string(keys::CURRENT_USER_ID)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());
    let mut client = mailbox_client.lock().await;

    // 1. Check for expired mailboxes and trigger healing (NDSS '21 healing path)
    if let Ok(expiring) = two_way.get_expiring_conversations(0) {
        for peer_id in expiring {
            tracing::info!(
                peer_id = %peer_id,
                "DM mailbox expired, triggering healing path (re-bootstrap)"
            );
            let _ = event_tx.send(InboxEvent::DmMailboxExpired { peer_id });
        }
    }

    match two_way.poll_all_mailboxes(&mut client).await {
        Ok((poll_results, failures)) => {
            let mut total_messages = 0u32;

            // Handle failures first (healing path)
            for (peer_id, error) in failures {
                if matches!(error, MailboxError::MailboxNotFound | MailboxError::TokenExpired) {
                    tracing::info!(
                        peer_id = %peer_id,
                        error = %error,
                        "DM mailbox invalid or gone from server, triggering healing path (re-bootstrap)"
                    );
                    let _ = event_tx.send(InboxEvent::DmMailboxExpired { peer_id });
                }
            }

            for result in poll_results {
                let peer_id = result.peer_id;

                // Collect message IDs for potential acknowledgment
                let message_ids: Vec<String> = result
                    .messages
                    .iter()
                    .map(|m| m.message_id.clone())
                    .collect();

                // Get shared secret for this conversation (will be reloaded after bootstrap)
                let mut shared_secret: Option<[u8; 32]> = match conversation_manager.load_conversation(&peer_id) {
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

                    // Check if we've already processed this message ID.
                    // This prevents "old counter" errors when acknowledgment fails
                    // and the server re-delivers the same message.
                    if !deduplicator.mark_processed(&msg.message_id) {
                        tracing::debug!(
                            peer_id = %peer_id,
                            message_id = %msg.message_id,
                            "DM message already processed, skipping decryption"
                        );
                        continue;
                    }

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
                                    attachment_repo,
                                    user_repo,
                                    domain_tx,
                                );
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                let convergence_failed = error_str.contains("convergence failed");
                                tracing::error!(
                                    peer_id = %peer_id,
                                    message_id = %msg.message_id,
                                    error = %e,
                                    convergence_failed,
                                    "DM decryption failed"
                                );
                                let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                                    peer_id,
                                    error: error_str,
                                    convergence_failed,
                                });
                            }
                        }
                    } else {
                        // No shared secret yet - this might be the bootstrap response
                        // The bootstrap response is encrypted with Signal Protocol (X3DH/DR)
                        // but NOT with the mailbox-layer encryption (we need peer's key first)
                        tracing::info!(
                            peer_id = %peer_id,
                            message_id = %msg.message_id,
                            envelope_len = msg.encrypted_envelope.len(),
                            "BOOTSTRAP[4/4] No shared secret - checking if this is bootstrap RESPONSE"
                        );

                        // Try to decrypt with Signal Protocol and parse as KeyExchange
                        if let Some(_key_exchange_result) = try_parse_bootstrap_response(
                            &msg.encrypted_envelope,
                            peer_id,
                            conversation_manager,
                            message_gateway,
                        ).await {
                            // Successfully extracted and saved peer's keys
                            // NOTE: We intentionally do NOT emit KeyExchangeReceived here.
                            // This is a bootstrap RESPONSE (we were the initiator), not a new
                            // bootstrap request. The dm_bootstrap_handler handles NEW bootstraps
                            // from device inbox, not responses we receive in our DM mailbox.
                            // Emitting here would cause the handler to send a duplicate response.
                            tracing::info!(
                                peer_id = %peer_id,
                                "BOOTSTRAP[4/4] SUCCESS - RESPONSE processed, shared secret established!"
                            );

                            // Reload shared secret so remaining messages in this batch can be decrypted
                            shared_secret = conversation_manager
                                .load_conversation(&peer_id)
                                .ok()
                                .flatten()
                                .and_then(|c| c.conversation_key)
                                .and_then(|k| k.shared_secret);

                            tracing::info!(
                                peer_id = %peer_id,
                                has_shared_secret = shared_secret.is_some(),
                                "BOOTSTRAP[4/4] Reloaded shared secret for remaining messages in batch"
                            );

                            tracing::info!(
                                peer_id = %peer_id,
                                "BOOTSTRAP[4/4] Emitting DmBootstrapCompleted event to flush pending messages"
                            );
                            // Emit bootstrap completed event so UI can retry pending messages
                            let _ = event_tx.send(InboxEvent::DmBootstrapCompleted { peer_id });
                        } else {
                            tracing::warn!(
                                peer_id = %peer_id,
                                message_id = %msg.message_id,
                                "BOOTSTRAP[4/4] FAILED - Message is not a valid bootstrap response"
                            );
                            let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                                peer_id,
                                error: "No shared secret established".to_string(),
                                convergence_failed: false, // Not a convergence failure - no session exists
                            });
                        }
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
    attachment_repo: &Arc<AttachmentRepository>,
    user_repo: &Arc<UserRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) {
    // Log the content type being dispatched
    let content_type = match &dm_content {
        DmContent::Text { .. } => "Text",
        DmContent::Typing { .. } => "Typing",
        DmContent::Receipt { .. } => "Receipt",
        DmContent::Reaction { .. } => "Reaction",
        DmContent::Pin { .. } => "Pin",
        DmContent::Edit { .. } => "Edit",
        DmContent::DeleteForMe { .. } => "DeleteForMe",
        DmContent::DeleteForEveryone { .. } => "DeleteForEveryone",
        DmContent::KeyExchange { .. } => "KeyExchange",
        DmContent::SavedMessageSync { .. } => "SavedMessageSync",
        _ => "Other",
    };
    tracing::info!(
        peer_id = %peer_id,
        sender_id = %sender_id,
        content_type = content_type,
        message_id = %message_id,
        "DM_DISPATCH: dispatch_dm_content called"
    );

    match dm_content {
        DmContent::Text { body, timestamp, attachments, profile_key } => {
            // Store sender's profile key if present (for decrypting their display_name)
            if let Some(ref key) = profile_key {
                if key.len() == 32 {
                    if let Some(current_user) = current_user_id {
                        let key_array: [u8; 32] = key.as_slice().try_into().unwrap();
                        match user_repo.set_friend_profile_key(current_user, sender_id, &key_array) {
                            Ok(()) => {
                                tracing::info!(
                                    sender_id = %sender_id,
                                    "Stored sender's profile key for display_name decryption"
                                );
                            }
                            Err(e) => {
                                tracing::debug!(
                                    sender_id = %sender_id,
                                    error = %e,
                                    "Failed to store profile key (sender may not be a friend)"
                                );
                            }
                        }
                    }
                }
            }

            // Resolve the DM channel ID for this peer (needed for both DB storage and UI routing)
            let dm_channel_id = if let Some(current_user) = current_user_id {
                // Look up the DM channel by peer ID (server-assigned, NOT derived)
                let dm_channel = channel_repo.get_dm_channel_by_peer(current_user, sender_id);

                if let Ok(Some(channel)) = dm_channel {
                    channel.id
                } else {
                    // Channel doesn't exist locally - create it with derived ID
                    // This can happen if we receive a DM before the channel is synced
                    let derived_id = ChannelId::from_dm_participants(current_user, sender_id);

                    tracing::info!(
                        peer_id = %peer_id,
                        sender_id = %sender_id,
                        derived_channel_id = %derived_id,
                        "Creating local DM channel for incoming message"
                    );

                    // Create a minimal DM channel entry
                    let channel = crate::domain::channel::Channel::new(
                        derived_id,
                        format!("DM with {}", sender_id), // Placeholder name
                        crate::domain::channel::ChannelType::DirectMessage,
                        *current_user, // Owner is current user for local tracking
                    );

                    // Insert if doesn't exist, ignore if already present
                    if let Err(e) = channel_repo.insert_or_ignore(&channel) {
                        tracing::warn!(
                            peer_id = %peer_id,
                            error = %e,
                            "Failed to ensure DM channel exists"
                        );
                    }

                    derived_id
                }
            } else {
                // No current user - derive channel ID from peer (best effort)
                ChannelId::new()
            };

            // Store message in local database for persistence
            if current_user_id.is_some() {
                let msg_id = message_id
                    .parse::<MessageId>()
                    .unwrap_or_else(|_| MessageId::new());

                // Use sender's timestamp for Signal-style dedup (sender_id + sent_at)
                // Both timestamp and server_timestamp are in nanoseconds
                let sent_at = if timestamp > 0 { timestamp } else { server_timestamp };
                tracing::debug!(
                    proto_timestamp = timestamp,
                    server_timestamp = server_timestamp,
                    chosen_sent_at = sent_at,
                    peer_id = %peer_id,
                    "dispatch_dm_content: timestamp selection"
                );
                let mut message = Message::new_text(dm_channel_id, Some(*sender_id), body.clone(), sent_at);
                message.id = msg_id;

                // Add attachment IDs to message (attachments are stored separately in attachment_repo)
                message.attachments = attachments
                    .iter()
                    .filter(|p| !p.cdn_key.is_empty())
                    .map(|p| AttachmentId::from_cdn_key(&p.cdn_key))
                    .collect();

                match message_repo.insert_or_ignore(&message) {
                    Ok(true) => {
                        if let Ok(new_count) = channel_repo.increment_unread_count(&dm_channel_id) {
                            let _ = event_tx.send(InboxEvent::DmUnreadCountUpdated {
                                channel_id: dm_channel_id,
                                peer_id,
                                unread_count: new_count,
                            });
                        }
                        tracing::debug!(
                            peer_id = %peer_id,
                            channel_id = %dm_channel_id,
                            message_id = %msg_id,
                            "Stored received DM in database"
                        );
                    }
                    Ok(false) => {
                        tracing::debug!(peer_id = %peer_id, "DM duplicate skipped");
                    }
                    Err(e) => {
                        tracing::warn!(peer_id = %peer_id, error = %e, "Failed to store DM");
                    }
                }
            }

            // Create attachment records from pointers (service layer responsibility)
            if !attachments.is_empty() {
                let attachment_service = AttachmentService::new_local(attachment_repo.clone());
                let created_ids = attachment_service.create_from_pointers(&attachments);
                tracing::debug!(
                    peer_id = %peer_id,
                    attachment_count = created_ids.len(),
                    "Created attachment records from pointers"
                );
            }

            let _ = event_tx.send(InboxEvent::DmReceived {
                peer_id,
                channel_id: dm_channel_id,
                text: body,
                // Both timestamp and server_timestamp are in nanoseconds
                timestamp: if timestamp > 0 { timestamp } else { server_timestamp },
                message_id: message_id.to_string(),
                attachments,
            });
        }

        DmContent::Typing { action, .. } => {
            // Emit typing event - the UI layer handles stale indicators with 15s auto-clear timers
            // (Signal-style: each STARTED resets a 15s timer, timer expiry clears indicator)
            tracing::info!(
                peer_id = %peer_id,
                action,
                action_name = if action == 1 { "STARTED" } else { "STOPPED" },
                "DM_MAILBOX: Emitting DmTyping event"
            );
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

        DmContent::SavedMessageSync {
            message_id,
            text,
            sent_at,
            attachment_ids,
        } => {
            tracing::info!(message_id = %message_id, "Saved message sync received from another device");
            let _ = event_tx.send(InboxEvent::SavedMessageSync {
                message_id,
                text,
                sent_at,
                attachment_ids,
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

        DmContent::CallKeyOffer {
            call_id,
            ephemeral_public_key,
            timestamp,
            call_type,
            channel_id,
        } => {
            // Call key offer received (ephemeral key for PFS)
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                "Call key offer received (incoming call with ephemeral key)"
            );

            // Look up the actual DM channel by peer ID (server-assigned, NOT derived)
            let resolved_channel_id = if channel_id.is_empty() {
                if let Some(current_user) = current_user_id {
                    match channel_repo.get_dm_channel_by_peer(current_user, sender_id) {
                        Ok(Some(channel)) => channel.id.to_string(),
                        _ => {
                            let derived = ChannelId::from_dm_participants(current_user, sender_id);
                            derived.to_string()
                        }
                    }
                } else {
                    peer_id.to_string()
                }
            } else {
                channel_id
            };

            let resolved_call_type = if call_type.is_empty() {
                "Voice Call".to_string()
            } else {
                call_type
            };

            let _ = event_tx.send(InboxEvent::CallKeyOfferReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                call_type: resolved_call_type,
                channel_id: resolved_channel_id,
                timestamp,
            });
        }

        DmContent::CallKeyAnswer {
            call_id,
            ephemeral_public_key,
            timestamp,
        } => {
            // Call key answer received (response to our offer)
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                "Call key answer received (peer's ephemeral key)"
            );

            let _ = event_tx.send(InboxEvent::CallKeyAnswerReceived {
                peer_id,
                call_id,
                ephemeral_public_key,
                timestamp,
            });
        }

        DmContent::GroupCallKey {
            call_id,
            key_material,
            key_epoch,
            channel_id,
            timestamp,
        } => {
            // Group call key distributed via pairwise sealed sender
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                key_epoch = key_epoch,
                "Group call key received via pairwise distribution"
            );

            // Parse channel_id
            let channel_id: ChannelId = channel_id
                .parse()
                .unwrap_or_else(|_| ChannelId::new());

            let _ = event_tx.send(InboxEvent::GroupCallKeyReceived {
                sender_id: *sender_id,
                call_id,
                key_material,
                key_epoch,
                channel_id,
                timestamp,
            });
        }

        DmContent::CallHangup {
            call_id,
            reason,
            timestamp,
        } => {
            // Call hangup notification via sealed sender
            tracing::info!(
                peer_id = %peer_id,
                call_id = %call_id,
                reason = ?reason,
                "Call hangup notification received"
            );

            let reason_str = format!("{:?}", reason);
            let _ = event_tx.send(InboxEvent::CallHangup {
                peer_id,
                call_id,
                reason: reason_str,
                timestamp,
            });
        }

        // ====================================================================
        // Anonymous Group Messages (NDSS'21 style)
        // ====================================================================
        DmContent::AnonGroupInvite {
            group_id,
            group_name,
            inviter_mailbox_pk,
            timestamp,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                group_name = %group_name,
                "Anonymous group invite received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupInvite {
                peer_id,
                group_id,
                group_name,
                inviter_mailbox_pk,
                timestamp,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
            });
        }

        DmContent::AnonGroupJoinAck {
            group_id,
            joiner_mailbox_pk,
            timestamp,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                has_profile = profile_key.is_some(),
                "Anonymous group join ack received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupJoinAck {
                peer_id,
                group_id,
                joiner_mailbox_pk,
                timestamp,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
            });
        }

        DmContent::AnonGroupMemberList {
            group_id,
            members,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                member_count = members.len(),
                "Anonymous group member list received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupMemberList {
                peer_id,
                group_id,
                members,
                timestamp,
            });
        }

        DmContent::AnonGroupLeave {
            group_id,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group leave received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupLeave {
                peer_id,
                group_id,
                timestamp,
            });
        }

        DmContent::AnonGroupKick {
            group_id,
            kicked_user_id,
            admin_signature,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group kick received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupKick {
                peer_id,
                group_id,
                kicked_user_id,
                admin_signature,
                timestamp,
            });
        }

        DmContent::AnonGroupUpdate {
            group_id,
            update,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group update received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupUpdate {
                peer_id,
                group_id,
                update,
                timestamp,
            });
        }

        DmContent::AnonGroupMessage {
            group_id,
            inner_content,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                "Anonymous group message received"
            );
            let _ = event_tx.send(InboxEvent::AnonGroupMessage {
                peer_id,
                group_id,
                inner_content,
            });
        }
        // =====================================================================
        // Anonymous DM Messages
        // =====================================================================
        DmContent::AnonDmRequest {
            conversation_id,
            initiator_mailbox_pk,
            intro_message,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                has_intro = intro_message.is_some(),
                has_encrypted_name = encrypted_display_name.is_some(),
                has_encrypted_avatar = encrypted_avatar.is_some(),
                timestamp = timestamp,
                "Anonymous DM request received"
            );
            let _ = event_tx.send(InboxEvent::AnonDmRequest {
                peer_id,
                conversation_id,
                initiator_mailbox_pk,
                intro_message,
                profile_key,
                encrypted_display_name,
                encrypted_avatar,
                timestamp,
            });
        }
        DmContent::AnonDmAccept {
            conversation_id,
            responder_mailbox_pk,
            profile_key,
            encrypted_display_name,
            encrypted_avatar,
            timestamp,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                has_encrypted_display_name = encrypted_display_name.is_some(),
                has_encrypted_avatar = encrypted_avatar.is_some(),
                timestamp = timestamp,
                "Anonymous DM acceptance received"
            );
            let _ = event_tx.send(InboxEvent::AnonDmAccept {
                peer_id,
                conversation_id,
                responder_mailbox_pk,
                profile_key,
                responder_display_name: None, // Will be decrypted in handler
                encrypted_display_name,
                encrypted_avatar,
                timestamp,
            });
        }
        DmContent::AnonDmMessage {
            conversation_id,
            inner_content,
        } => {
            tracing::info!(
                peer_id = %peer_id,
                conversation_id_len = conversation_id.len(),
                "Anonymous DM message received"
            );
            let _ = event_tx.send(InboxEvent::AnonDmMessage {
                peer_id,
                conversation_id,
                inner_content,
            });
        }
    }
}

/// Try to parse a mailbox message as a bootstrap response.
///
/// Per NDSS '21, the bootstrap response has TWO encryption layers:
/// 1. **Transport Layer (Envelope)**: ECDH(sender_identity, our_mailbox_ephemeral)
/// 2. **Content Layer (Signal)**: Signal Protocol (X3DH/Double Ratchet)
///
/// We must:
/// 1. Decrypt the envelope using our mailbox private key
/// 2. Decrypt the Signal Protocol content from the result
///
/// # Returns
///
/// Some(ephemeral_pk) if successfully decrypted and saved peer's keys,
/// None if the message is not a valid bootstrap response.
async fn try_parse_bootstrap_response(
    envelope: &[u8],
    peer_id: UserId,
    conversation_manager: &DmConversationManager,
    message_gateway: &Arc<MessageGateway>,
) -> Option<Vec<u8>> {
    tracing::debug!(
        peer_id = %peer_id,
        envelope_len = envelope.len(),
        "BOOTSTRAP[4/4] Attempting to decrypt as bootstrap RESPONSE (2-layer encryption)"
    );

    // Step 1: Get our mailbox private key to decrypt the envelope
    let our_mailbox = match conversation_manager.load_conversation(&peer_id) {
        Ok(Some(conv)) => {
            tracing::debug!(
                peer_id = %peer_id,
                has_mailbox = conv.our_mailbox.is_some(),
                has_peer_mailbox = conv.peer_mailbox.is_some(),
                has_conv_key = conv.conversation_key.is_some(),
                "BOOTSTRAP[4/4] Loaded conversation state"
            );
            conv.our_mailbox
        }
        Ok(None) => {
            tracing::warn!(peer_id = %peer_id, "BOOTSTRAP[4/4] No conversation state found");
            return None;
        }
        Err(e) => {
            tracing::error!(peer_id = %peer_id, error = %e, "BOOTSTRAP[4/4] Failed to load conversation");
            return None;
        }
    };

    let mailbox = match our_mailbox {
        Some(m) => m,
        None => {
            tracing::warn!(peer_id = %peer_id, "No mailbox configured for envelope decryption");
            return None;
        }
    };

    // Step 2: Decrypt the envelope layer
    // The mailbox private key is stored as 64 bytes (Ed25519 keypair: 32-byte seed + 32-byte public)
    // We need the 32-byte seed for envelope decryption
    let mailbox_seed: [u8; 32] = mailbox.private_key[..32]
        .try_into()
        .map_err(|_| {
            tracing::error!(peer_id = %peer_id, "Invalid mailbox private key format");
        })
        .ok()?;

    let (sender_identity, signal_ciphertext) = match decrypt_envelope(envelope, &mailbox_seed, None) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                envelope_len = envelope.len(),
                "Failed to decrypt envelope layer"
            );
            return None;
        }
    };

    tracing::info!(
        peer_id = %peer_id,
        sender_identity_len = sender_identity.len(),
        signal_ciphertext_len = signal_ciphertext.len(),
        "BOOTSTRAP[4/4] Layer 1 (envelope) decrypted, now decrypting Layer 2 (Signal)"
    );

    // Step 3: Decrypt the Signal Protocol content
    match message_gateway
        .decrypt_identified_message(&signal_ciphertext, &peer_id, 1) // device_id = 1 (primary)
        .await
    {
        Ok(result) => {
            match result {
                crate::services::message_gateway::SealedSenderResult::KeyExchange {
                    ephemeral_public_key,
                    ..
                } => {
                    tracing::info!(
                        peer_id = %peer_id,
                        key_len = ephemeral_public_key.len(),
                        "BOOTSTRAP[4/4] Layer 2 (Signal) decrypted - got 64-byte KeyExchange"
                    );

                    // Process the KeyExchange
                    let result = process_bootstrap_key_exchange(
                        &ephemeral_public_key,
                        peer_id,
                        conversation_manager,
                    );

                    if result.is_some() {
                        tracing::info!(
                            peer_id = %peer_id,
                            "BOOTSTRAP[4/4] Peer keys extracted and saved, shared secret derived!"
                        );
                    }

                    result
                }
                crate::services::message_gateway::SealedSenderResult::MessageStored => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Decrypted and stored message (not a KeyExchange)"
                    );
                    None
                }
                crate::services::message_gateway::SealedSenderResult::SenderKeyRequested { .. } => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Received SenderKeyRequest in DM bootstrap path (unexpected)"
                    );
                    None
                }
                crate::services::message_gateway::SealedSenderResult::MailboxAnnouncement { .. } => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Received MailboxAnnouncement in DM bootstrap path (unexpected)"
                    );
                    None
                }
                crate::services::message_gateway::SealedSenderResult::DataMessageReceived { .. } => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Received DataMessage in DM bootstrap path (unexpected)"
                    );
                    None
                }
                // Other result types are not expected in DM bootstrap path
                _ => None,
            }
        }
        Err(e) => {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to decrypt Signal content from bootstrap response"
            );
            None
        }
    }
}

/// Process the ephemeral_pk from a bootstrap KeyExchange.
///
/// Extracts peer's mailbox and conversation keys, saves them, and derives shared secret.
fn process_bootstrap_key_exchange(
    ephemeral_pk: &[u8],
    peer_id: UserId,
    conversation_manager: &DmConversationManager,
) -> Option<Vec<u8>> {
    if ephemeral_pk.len() != 64 {
        tracing::error!(
            peer_id = %peer_id,
            key_len = ephemeral_pk.len(),
            "Invalid bootstrap key length"
        );
        return None;
    }

    tracing::info!(
        peer_id = %peer_id,
        "Processing bootstrap key exchange from peer"
    );

    // Extract peer's mailbox pk (first 32 bytes) and conversation ephemeral pk (next 32 bytes)
    let mut peer_mailbox_pk = [0u8; 32];
    let mut peer_conversation_pk = [0u8; 32];
    peer_mailbox_pk.copy_from_slice(&ephemeral_pk[0..32]);
    peer_conversation_pk.copy_from_slice(&ephemeral_pk[32..64]);

    // Save peer's mailbox address
    let peer_mailbox = PeerMailbox {
        public_key: peer_mailbox_pk,
        received_at: chrono::Utc::now(),
        expires_at: None,
    };

    if let Err(e) = conversation_manager.save_peer_mailbox(&peer_id, &peer_mailbox) {
        tracing::error!(
            peer_id = %peer_id,
            error = %e,
            "Failed to save peer's mailbox from bootstrap response"
        );
        return None;
    }

    tracing::debug!(peer_id = %peer_id, "Saved peer's mailbox address");

    // Load our existing conversation key and derive the shared secret
    match conversation_manager.load_conversation(&peer_id) {
        Ok(Some(conv)) => {
            if let Some(mut key) = conv.conversation_key {
                // Derive shared secret using peer's conversation ephemeral public key
                if let Err(e) = key.derive_shared_secret(peer_conversation_pk) {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to derive shared secret from peer's ephemeral key"
                    );
                    return None;
                }

                // Save the updated key with shared secret
                if let Err(e) = conversation_manager.save_conversation_key(&peer_id, &key) {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to save conversation key with shared secret"
                    );
                    return None;
                }

                tracing::info!(
                    peer_id = %peer_id,
                    "Derived and saved shared secret from bootstrap response"
                );

                Some(ephemeral_pk.to_vec())
            } else {
                // No conversation key yet - this shouldn't happen for the initiator
                // (they created the key when sending the bootstrap)
                tracing::warn!(
                    peer_id = %peer_id,
                    "No conversation key found - creating new one"
                );

                // Create a new key and derive the shared secret
                let mut new_key = ConversationKey::new();
                if let Err(e) = new_key.derive_shared_secret(peer_conversation_pk) {
                    tracing::error!(peer_id = %peer_id, error = %e, "Failed to derive shared secret");
                    return None;
                }

                if let Err(e) = conversation_manager.save_conversation_key(&peer_id, &new_key) {
                    tracing::error!(peer_id = %peer_id, error = %e, "Failed to save new conversation key");
                    return None;
                }

                Some(ephemeral_pk.to_vec())
            }
        }
        Ok(None) => {
            tracing::warn!(
                peer_id = %peer_id,
                "No conversation state found - creating new"
            );

            // Create new conversation key
            let mut new_key = ConversationKey::new();
            if let Err(e) = new_key.derive_shared_secret(peer_conversation_pk) {
                tracing::error!(peer_id = %peer_id, error = %e, "Failed to derive shared secret");
                return None;
            }

            if let Err(e) = conversation_manager.save_conversation_key(&peer_id, &new_key) {
                tracing::error!(peer_id = %peer_id, error = %e, "Failed to save conversation key");
                return None;
            }

            Some(ephemeral_pk.to_vec())
        }
        Err(e) => {
            tracing::error!(
                peer_id = %peer_id,
                error = %e,
                "Failed to load conversation state"
            );
            None
        }
    }
}
