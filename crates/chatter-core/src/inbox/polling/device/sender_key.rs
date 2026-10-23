use super::*;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone)]
enum PendingSenderKey {
    Data(Vec<u8>),
}

// The sender-key receive state can advance before the message reaches its table.
// Retry the authenticated plaintext on redelivery.
static PENDING_SENDER_KEY: LazyLock<parking_lot::Mutex<HashMap<String, PendingSenderKey>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Process a sender key encrypted message.
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_sender_key_message(
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

    let cached = PENDING_SENDER_KEY.lock().get(&msg.message_guid).cloned();
    let routed = if let Some(cached) = cached {
        Ok(match cached {
            PendingSenderKey::Data(plaintext) => RouteResult::Decrypted(plaintext),
        })
    } else {
        message_gateway
            .route_encrypted_message(encrypted_data, sender_id, msg.source_device_id)
            .await
    };
    match routed {
        Ok(RouteResult::Decrypted(plaintext)) => {
            tracing::info!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                plaintext_len = plaintext.len(),
                "[INBOX_SK] Successfully decrypted, processing DataMessage"
            );
            if !process_decrypted_data_message(
                msg,
                &plaintext,
                sender_id,
                event_tx,
                stats,
                message_repo,
                channel_repo,
                ephemeral_repo,
                domain_tx,
            ) {
                PENDING_SENDER_KEY
                    .lock()
                    .insert(msg.message_guid.clone(), PendingSenderKey::Data(plaintext));
                return AckDecision::Retain;
            }
        }
        Ok(RouteResult::ControlContent(plaintext)) => {
            tracing::debug!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                plaintext_len = plaintext.len(),
                "[INBOX_SK] Decrypted control content"
            );
            // Parse as Content proto (typing, reaction, edit, pin, delete)
            match crate::network::validation::protovalidate::decode_inbound_content(
                plaintext.as_slice(),
                "device_sender_key_control",
            ) {
                Ok(content_proto) => {
                    if let Some(inner) = content_proto.content {
                        if let Err(error) = persist_exact_saved_delete_before_ack(
                            &inner,
                            *sender_id,
                            message_gateway,
                        ) {
                            tracing::warn!(%error, "Exact Saved deletion not admitted before acknowledgement");
                            return match error {
                                ServiceError::Storage(crate::error::StorageError::InvalidData(
                                    _,
                                )) => AckDecision::Ack,
                                _ => AckDecision::Retain,
                            };
                        }
                        if let content::Content::SavedMessageSync(sync) = &inner {
                            match crate::services::message::persist_saved_device_snapshot(
                                message_gateway,
                                *sender_id,
                                sync,
                            ) {
                                Ok(outcome) => {
                                    let _ = event_tx.send(InboxEvent::SavedSnapshotApplied {
                                        sender_id: *sender_id,
                                        outcome,
                                    });
                                    stats.write().messages_processed += 1;
                                    retry_budget.forget(&msg.message_guid);
                                    return AckDecision::Ack;
                                }
                                Err(error) => {
                                    return device_failure_decision(
                                        Some(*sender_id),
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
                            sender_id,
                            msg.source_device_id,
                            &msg.message_guid,
                            msg.server_timestamp,
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
        Ok(RouteResult::NeedsSenderKey {
            channel_id,
            sender_id,
        }) => {
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
        Ok(RouteResult::MailboxAnnouncement {
            channel_id,
            announcement,
        }) => {
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
    PENDING_SENDER_KEY.lock().remove(&msg.message_guid);
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
) -> bool {
    tracing::debug!(
        message_guid = %msg.message_guid,
        sender_id = %sender_id,
        plaintext_len = plaintext.len(),
        "[INBOX_DM] Decoding DataMessage from plaintext"
    );

    match crate::network::validation::protovalidate::decode_inbound::<DataMessage>(
        plaintext,
        "device_sender_key_data",
    ) {
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
                    return true;
                }
            };

            // Create message ID
            let message_id = msg
                .message_guid
                .parse::<MessageId>()
                .unwrap_or_else(|_| MessageId::new());

            // A sender-key envelope is a server channel message (anon groups
            // ride the mailbox, never this path), so its bucket is the channel
            // row's type — `Channel`, even when the row isn't synced yet.
            let conversation_type =
                crate::services::message::conversion::conversation_type_for_channel(
                    channel_repo,
                    &channel_id,
                );

            // Create domain message with sender's timestamp for Signal-style dedup
            // created_at is derived from sent_at in Message::new_text()
            let mut message = Message::new_text(
                channel_id,
                conversation_type,
                Some(*sender_id),
                data_message.body.clone(),
                data_message.timestamp,
            );
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

            // Store message with deduplication. Everything below announces
            // the row as stored (an early edit, reaction or pin applied), not
            // the decoded one; an early delete stores nothing.
            match message_repo.insert_or_ignore_received(&message) {
                Ok(Some(message)) => {
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
                        if let Err(e) = ephemeral_repo
                            .insert_armed(&message.id, data_message.ephemeral_ttl_seconds)
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
                        message_id: message.id.to_string(),
                        text: match &message.content {
                            crate::domain::message::MessageContent::Text(text) => text.clone(),
                            _ => data_message.body.clone(),
                        },
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
                Ok(None) => {
                    tracing::debug!(
                        message_id = %message.id,
                        "Channel message already exists, or was deleted before it arrived — not announced"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        message_id = %message.id,
                        error = %e,
                        "Failed to store channel message"
                    );
                    return false;
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
    true
}
