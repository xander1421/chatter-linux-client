use super::*;
use std::collections::HashMap;
use std::sync::LazyLock;

type PendingAnnouncement = (
    UserId,
    ChannelId,
    crate::proto::content_v1::GroupMailboxAddress,
);

// Sealed sender decryption can advance the ratchet before the mailbox save.
static PENDING_ANNOUNCEMENTS: LazyLock<parking_lot::Mutex<HashMap<String, PendingAnnouncement>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Process a sealed sender message.
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_sealed_sender_message(
    msg: &DeviceMessage,
    pipeline: &MessageValidationPipeline,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    anon_group_repo: &crate::storage::repositories::AnonGroupRepository,
    group_coordinator: &Arc<GroupCoordinator>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    retry_budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{self, AckDecision};
    let sealed_sender_msg = SealedSenderMessage {
        sealed_body: msg.envelope.clone(),
        server_key_id: 0,
        timestamp: msg.server_timestamp,
    };

    let pending = PENDING_ANNOUNCEMENTS.lock().get(&msg.message_guid).cloned();
    let result = if let Some((sender_id, channel_id, announcement)) = pending {
        Ok(SealedSenderResult::MailboxAnnouncement {
            sender_id,
            channel_id,
            announcement,
        })
    } else {
        message_gateway.process_incoming_message(sealed_sender_msg).await
    };

    match result {
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
            sender_device_id,
            conversation_id,
            ephemeral_public_key,
            timestamp,
        }) => {
            // Emit DomainEvent for KeyExchange so DM bootstrap handler can process it
            if let Some(tx) = domain_tx {
                // Parse conversation_id as ChannelId
                let conv_id: crate::domain::ChannelId =
                    conversation_id.parse().unwrap_or_else(|_| {
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
                    sender_device_id,
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
        Ok(SealedSenderResult::SenderKeyRequested {
            requester_id,
            channel_id,
        }) => {
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
        Ok(SealedSenderResult::MailboxAnnouncement {
            sender_id,
            channel_id,
            announcement,
        }) => {
            match group_coordinator.process_announcement(sender_id, &announcement) {
                Ok(()) => {
                    PENDING_ANNOUNCEMENTS.lock().remove(&msg.message_guid);
                }
                Err(crate::error::MailboxError::InvalidServerKey(error)) => {
                    tracing::warn!(%error, "Invalid group mailbox announcement");
                    return AckDecision::Ack;
                }
                Err(error) => {
                    tracing::warn!(%error, "Group mailbox announcement awaits persistence");
                    PENDING_ANNOUNCEMENTS.lock().insert(
                        msg.message_guid.clone(),
                        (sender_id, channel_id, announcement),
                    );
                    return AckDecision::Retain;
                }
            }
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                channel_id = %channel_id,
                "[INBOX] Stored GroupMailboxAddress via sealed sender"
            );
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
            profile_key_changed,
        }) => {
            tracing::info!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                sender_device_id,
                channel_id = %channel_id,
                "[INBOX] SKDM processed — emitting SenderKeyReceived to drain parked group messages"
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
        Ok(SealedSenderResult::SecretChatBootstrap {
            sender_id,
            sender_device_id,
            content,
        }) => {
            if let Some(inner) = content.content {
                if let Err(error) = dispatch_secret_chat_bootstrap(
                    inner,
                    sender_id,
                    sender_device_id,
                    event_tx,
                    anon_group_repo,
                ) {
                    tracing::error!(%error, "Failed to retain device inbox group invitation");
                    return AckDecision::Retain;
                }
            }
            stats.write().messages_processed += 1;
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
            channel_id,
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
                desync_marker_target(channel_repo, channel_id, sender_id, msg.server_timestamp),
                ack_policy::FailureMarker::DeliveryIssue,
            );
        }
        // SENDER half of the 1:1 self-heal: a peer's DecryptionErrorMessage
        // about a message we sent was processed (session archived when
        // `archived`). Surface on the domain bus for observability, and have
        // the InboxEventProcessor send the message again — archived or not,
        // as Signal-Desktop's `onRetryRequest` does.
        Ok(SealedSenderResult::SessionHealed {
            peer_id,
            peer_device_id,
            archived,
            timestamp_ms,
        }) => {
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
            let _ = event_tx.send(InboxEvent::MessageProcessed {
                message_id: msg.message_guid.clone(),
            });
            stats.write().messages_processed += 1;
        }
        // SENDER half of the sender-key retry: a member could not decrypt a
        // group message we sent. The InboxEventProcessor resends it pairwise.
        Ok(SealedSenderResult::ResendRequested {
            peer_id,
            peer_device_id,
            timestamp_ms,
        }) => {
            let _ = event_tx.send(InboxEvent::GroupResendRequested {
                peer_id,
                peer_device_id,
                timestamp_ms,
            });
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
        // dedups and orders a SYNTHETIC proto (`channel_id: "pending"`, no
        // content: dedup keys on the id, ordering on channel and timestamp,
        // and the validator skips absent content) purely to gate the UI
        // events. So every arm of it acks, including `Buffered` and `Dropped`:
        // the payload is on disk before we get here and there is nothing left
        // to lose.
        //
        // Worth knowing about the orderer it feeds, though: `get_ready` and
        // `flush_stale` have no production callers, so anything it buffers is
        // never drained. That leaks UI events (and memory), not messages.
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
            let envelope_type = EnvelopeType::SealedSender;

            // Create a Message for pipeline processing
            let proto_msg = crate::proto::chat_v1::Message {
                id: msg.message_guid.clone(),
                channel_id: "pending".to_string(),
                author_id: msg.source_user_id.clone(),
                timestamp: msg.server_timestamp,
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
