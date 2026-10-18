use super::*;

/// Process an identified sender message (PreKey or Ciphertext).
///
/// This is the fallback path when sealed sender is not used.
/// The server knows the sender (from Basic auth) and includes source_user_id.
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_identified_sender_message(
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
            sender_device_id,
            conversation_id,
            ephemeral_public_key,
            timestamp,
        }) => {
            // Emit DomainEvent for KeyExchange so DM bootstrap handler can process it
            if let Some(tx) = domain_tx {
                let conv_id: crate::domain::ChannelId =
                    conversation_id.parse().unwrap_or_else(|_| {
                        let hash = sha2::Sha256::digest(conversation_id.as_bytes());
                        let mut uuid_bytes = [0u8; 16];
                        uuid_bytes.copy_from_slice(&hash[..16]);
                        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
                        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
                        crate::domain::ChannelId::from_uuid(uuid::Uuid::from_bytes(uuid_bytes))
                    });

                let _ = tx.send(DomainEvent::KeyExchangeReceived {
                    sender_id,
                    sender_device_id,
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
        Ok(SealedSenderResult::ChannelReceiptTargetsSynced {
            channel_id,
            timestamp,
        }) => {
            let _ = event_tx.send(InboxEvent::ChannelReceiptTargetsSynced {
                channel_id,
                timestamp,
            });
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
            profile_key_changed,
        }) => {
            if profile_key_changed {
                // Same refresh the pure handshake gets: fetch + decrypt the
                // sender's profile with the key this message just delivered.
                let _ = event_tx.send(InboxEvent::ProfileKeyReceived {
                    sender_id: msg_sender,
                });
            }
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
        Ok(SealedSenderResult::SenderKeyRequested {
            requester_id,
            channel_id,
        }) => {
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
            profile_key_changed,
        }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                sender_device_id,
                channel_id = %channel_id,
                "[INBOX] SKDM processed via identified sender — emitting SenderKeyReceived"
            );
            if profile_key_changed {
                let _ = event_tx.send(InboxEvent::ProfileKeyReceived { sender_id });
            }
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
        Ok(SealedSenderResult::MailboxAnnouncement {
            sender_id,
            channel_id,
            announcement,
        }) => {
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
        // A secret-chat bootstrap names its sender to the server on this path;
        // secret chats only start sealed. Refused.
        Ok(SealedSenderResult::SecretChatBootstrap { .. }) => {
            tracing::warn!(
                message_id = %msg.message_guid,
                "[INBOX] Secret-chat bootstrap via identified sender refused"
            );
            stats.write().messages_failed += 1;
        }
        Ok(SealedSenderResult::ControlContent {
            sender_id: actual_sender,
            sender_device_id,
            content,
        }) => {
            if let Some(inner) = content.content {
                if let Err(error) =
                    persist_exact_saved_delete_before_ack(&inner, actual_sender, message_gateway)
                {
                    tracing::warn!(%error, "Exact Saved deletion not admitted before acknowledgement");
                    return match error {
                        ServiceError::Storage(crate::error::StorageError::InvalidData(_)) => {
                            AckDecision::Ack
                        }
                        _ => AckDecision::Retain,
                    };
                }
                if let content::Content::SavedMessageSync(sync) = &inner {
                    match crate::services::message::persist_saved_device_snapshot(
                        message_gateway,
                        actual_sender,
                        sync,
                    ) {
                        Ok(outcome) => {
                            let _ = event_tx.send(InboxEvent::SavedSnapshotApplied {
                                sender_id: actual_sender,
                                outcome,
                            });
                            stats.write().messages_processed += 1;
                            retry_budget.forget(&msg.message_guid);
                            return AckDecision::Ack;
                        }
                        Err(error) => {
                            return device_failure_decision(
                                Some(actual_sender),
                                msg.server_timestamp,
                                &error,
                                message_repo,
                                domain_tx,
                                &msg.message_guid,
                                retry_budget,
                            );
                        }
                    }
                }
                dispatch_control_content(
                    inner,
                    &actual_sender,
                    sender_device_id,
                    &msg.message_guid,
                    msg.server_timestamp,
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
        | Ok(SealedSenderResult::SessionHealed { .. })
        | Ok(SealedSenderResult::ResendRequested { .. }) => {
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
