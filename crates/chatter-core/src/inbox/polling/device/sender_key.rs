use super::*;
use crate::services::gateway::inbound::content_channel;
use crate::services::gateway::inbound::PairwiseChannelContent;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone)]
enum PendingDeviceContent {
    Data(Vec<u8>),
    Pairwise(PairwiseChannelContent),
}

// The sender-key receive state can advance before the message reaches its table.
// Retry the authenticated plaintext on redelivery.
static PENDING_DEVICE_CONTENT: LazyLock<parking_lot::Mutex<HashMap<String, PendingDeviceContent>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

pub(crate) async fn retry_pairwise_channel(
    envelope_id: &str,
    gateway: &MessageGateway,
) -> Option<Result<SealedSenderResult, ServiceError>> {
    let cached = PENDING_DEVICE_CONTENT.lock().get(envelope_id).cloned();
    let Some(PendingDeviceContent::Pairwise(pending)) = cached else {
        return None;
    };
    let result = gateway.route_pairwise_channel(pending).await;
    if matches!(&result, Ok(outcome) if !matches!(outcome, SealedSenderResult::AwaitingChannelMembership(_))) {
        PENDING_DEVICE_CONTENT.lock().remove(envelope_id);
    }
    Some(result)
}

pub(super) fn clear_pending_device_content(envelope_id: &str) {
    PENDING_DEVICE_CONTENT.lock().remove(envelope_id);
}

pub(crate) fn retain_pairwise_channel(
    msg: &DeviceMessage,
    pending: PairwiseChannelContent,
    gateway: &MessageGateway,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    let decision = sender_key_nonmember_decision(
        pending.channel_id,
        pending.sender_id,
        msg.server_timestamp,
        &gateway.message_repo,
        &gateway.channel_repo,
        domain_tx,
        &msg.message_guid,
        budget,
    );
    let mut cache = PENDING_DEVICE_CONTENT.lock();
    if decision.should_ack() {
        cache.remove(&msg.message_guid);
    } else {
        cache.insert(msg.message_guid.clone(), PendingDeviceContent::Pairwise(pending));
    }
    decision
}

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
        chatter_proto::content_v1::GroupEncrypted::decode(encrypted_data.ciphertext.as_slice())
            .ok()
            .and_then(|ge| ge.channel_id.parse().ok());

    let cached = PENDING_DEVICE_CONTENT.lock().get(&msg.message_guid).cloned();
    let routed = if let Some(PendingDeviceContent::Data(plaintext)) = cached {
        Ok(RouteResult::Decrypted(plaintext))
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
            let attachments = crate::services::AttachmentService::new_local(Arc::new(
                crate::storage::repositories::AttachmentRepository::new(
                    message_gateway.settings_repo.database().clone(),
                ),
            ));
            match process_decrypted_data_message(
                msg,
                &plaintext,
                channel_id,
                sender_id,
                event_tx,
                stats,
                message_repo,
                channel_repo,
                ephemeral_repo,
                &attachments,
                domain_tx,
            ) {
                DataOutcome::Handled => {}
                DataOutcome::StoreFailed => {
                    PENDING_DEVICE_CONTENT
                        .lock()
                        .insert(msg.message_guid.clone(), PendingDeviceContent::Data(plaintext));
                    return AckDecision::Retain;
                }
                DataOutcome::Malformed(reason) => {
                    let decision = sender_key_malformed_decision(
                        channel_id,
                        *sender_id,
                        msg.server_timestamp,
                        &reason,
                        message_repo,
                        channel_repo,
                        domain_tx,
                        &msg.message_guid,
                        retry_budget,
                    );
                    if decision.should_ack() {
                        PENDING_DEVICE_CONTENT.lock().remove(&msg.message_guid);
                    }
                    return decision;
                }
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
            match chatter_proto::protovalidate::decode_inbound_content(
                plaintext.as_slice(),
                "device_sender_key_control",
            ) {
                Ok(content_proto) => {
                    if let Some(inner) = content_proto.content {
                        if let Err(reason) = bound_to_channel(&inner, channel_id) {
                            stats.write().messages_failed += 1;
                            return sender_key_malformed_decision(
                                channel_id,
                                *sender_id,
                                msg.server_timestamp,
                                &reason,
                                message_repo,
                                channel_repo,
                                domain_tx,
                                &msg.message_guid,
                                retry_budget,
                            );
                        }
                        if let Some(decision) = apply_exact_saved_delete(
                            &inner, *sender_id, message_gateway, event_tx, stats,
                        ) {
                            return decision;
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
                    return sender_key_malformed_decision(
                        channel_id,
                        *sender_id,
                        msg.server_timestamp,
                        &format!("Failed to parse Content: {e}"),
                        message_repo,
                        channel_repo,
                        domain_tx,
                        &msg.message_guid,
                        retry_budget,
                    );
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
                "[INBOX_SK] Waiting for the sender-key distribution"
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
            // The gate refuses a sender outside our VERIFIED membership before
            // decrypting. That is usually a join we have not synced yet, so
            // the same bytes are admitted later: retain, not mark-and-drain.
            if let Some(channel_id) = channel_id
                && matches!(e, ServiceError::Validation(_))
                && crate::services::message::GroupStateGate::new(&message_gateway.settings_repo)
                    .content_admission(&channel_id, sender_id).is_none()
            {
                return sender_key_nonmember_decision(
                    channel_id,
                    *sender_id,
                    msg.server_timestamp,
                    message_repo,
                    channel_repo,
                    domain_tx,
                    &msg.message_guid,
                    retry_budget,
                );
            }
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
    PENDING_DEVICE_CONTENT.lock().remove(&msg.message_guid);
    retry_budget.forget(&msg.message_guid);
    AckDecision::Ack
}

/// The inner `Content` of a decrypted sender-key plaintext.
fn sender_key_content(plaintext: &[u8]) -> Result<content::Content, String> {
    chatter_proto::protovalidate::decode_inbound_content(plaintext, "device_sender_key_data")
        .map_err(|e| e.to_string())?
        .content
        .ok_or_else(|| "empty Content".to_string())
}

/// The channel a `Content` names, if it names one.

/// A sender-key plaintext speaks only for the channel whose key decrypted it
/// (the envelope's `GroupEncrypted.channel_id`, bound to the ciphertext's
/// distribution id by `SignalService::decrypt_from_group`, and the channel
/// the membership gate checked). Content naming another channel — or none,
/// which would land as a DM or sync event — is refused, or a member of one
/// channel writes into another past its kick, ban or mute.
fn bound_to_channel(inner: &content::Content, outer: Option<ChannelId>) -> Result<(), String> {
    let named = content_channel(inner).and_then(|c| c.parse::<ChannelId>().ok());
    match (named, outer) {
        (Some(named), Some(outer)) if named == outer => Ok(()),
        _ => Err("sender-key content names a channel other than its envelope's".into()),
    }
}

/// What became of a decrypted sender-key plaintext.
#[derive(Debug)]
enum DataOutcome {
    /// Stored, deduplicated, or dispatched as control content.
    Handled,
    /// Our storage failed — retry the cached plaintext on redelivery.
    StoreFailed,
    /// The plaintext will never parse; the caller marks the channel and drains.
    Malformed(String),
}

/// Process a decrypted DataMessage.
#[allow(clippy::too_many_arguments)]
fn process_decrypted_data_message(
    msg: &DeviceMessage,
    plaintext: &[u8],
    // The envelope's channel: the one whose sender key decrypted `plaintext`.
    outer_channel: Option<ChannelId>,
    sender_id: &UserId,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    ephemeral_repo: &Arc<EphemeralRepository>,
    attachment_service: &crate::services::AttachmentService,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) -> DataOutcome {
    tracing::debug!(
        message_guid = %msg.message_guid,
        sender_id = %sender_id,
        plaintext_len = plaintext.len(),
        "[INBOX_DM] Decoding DataMessage from plaintext"
    );

    // The sender encrypts a padded `Content` (MessageService prepare), not a
    // bare DataMessage; decoding it as DataMessage failed on every message
    // and acked it unseen. Deletes are carried inside a DataMessage but use
    // the same control dispatch as the other channel actions.
    let decoded = match sender_key_content(plaintext) {
        Ok(content_proto) if bound_to_channel(&content_proto, outer_channel).is_err() => {
            tracing::warn!(
                message_id = %msg.message_guid,
                sender_id = %sender_id,
                "Sender-key content names another channel than its envelope; refused"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: "sender-key content names another channel".into(),
            });
            stats.write().messages_failed += 1;
            return DataOutcome::Malformed(
                "sender-key content names a channel other than its envelope's".into(),
            );
        }
        Ok(content_proto) => match content_proto {
            content::Content::DataMessage(dm) if dm.delete.is_none() => Ok(dm),
            inner => {
                dispatch_control_content(
                    inner,
                    sender_id,
                    msg.source_device_id,
                    &msg.message_guid,
                    msg.server_timestamp,
                    event_tx,
                    stats,
                );
                return DataOutcome::Handled;
            }
        },
        Err(e) => Err(e),
    };
    match decoded {
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
                    return DataOutcome::Malformed(format!("Invalid channel ID: {e}"));
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
                crate::storage::repositories::channel_repo::conversation_type_for_channel(
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

            if !message.attachments.is_empty() {
                let storage = attachment_service.for_conversation(
                    &crate::state::ActiveConversation::Channel(channel_id),
                );
                if let Err(error) = storage.create_from_pointers(&data_message.attachments) {
                    tracing::warn!(%error, "Channel attachment pointers await persistence");
                    return DataOutcome::StoreFailed;
                }
            }

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
                    return DataOutcome::StoreFailed;
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
            return DataOutcome::Malformed(format!("Failed to parse DataMessage: {e}"));
        }
    }
    DataOutcome::Handled
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// The plaintext a channel send encrypts is a padded `Content`
    /// (MessageService prepare); the receiver must read the DataMessage out of
    /// it. Decoding it as a bare DataMessage dropped every private-channel
    /// message.
    #[test]
    fn channel_send_plaintext_yields_its_data_message() {
        let channel = ChannelId::new();
        let dm = DataMessage {
            body: "priv from S25".into(),
            timestamp: 1_763_070_000_000,
            channel_id: channel.to_string(),
            ..Default::default()
        };
        let mut wrap = chatter_proto::content_v1::Content {
            timestamp: dm.timestamp,
            content: Some(content::Content::DataMessage(dm)),
            ..Default::default()
        };
        crate::services::message::MessageService::pad_content_to_bucket(&mut wrap);
        let plaintext = wrap.encode_to_vec();

        assert!(
            chatter_proto::protovalidate::decode_inbound::<DataMessage>(&plaintext, "t").is_err(),
            "the old bare-DataMessage decode must not accept a Content"
        );
        match sender_key_content(&plaintext).expect("decodes") {
            content::Content::DataMessage(got) => {
                assert_eq!(got.body, "priv from S25");
                assert_eq!(got.channel_id, channel.to_string());
            }
            other => panic!("expected DataMessage, got {other:?}"),
        }
    }

    fn padded(inner: content::Content) -> Vec<u8> {
        let mut wrap = chatter_proto::content_v1::Content {
            timestamp: 1_763_070_000_000,
            content: Some(inner),
            ..Default::default()
        };
        crate::services::message::MessageService::pad_content_to_bucket(&mut wrap);
        wrap.encode_to_vec()
    }

    /// A member of channel A sends an envelope for A whose DataMessage names
    /// B: it must not land in B (or anywhere) — it is malformed for A.
    #[test]
    fn data_message_naming_another_channel_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(
            crate::storage::Database::open(&dir.path().join("t.db"), "test_passphrase").unwrap(),
        );
        db.run_migrations().unwrap();
        let messages = Arc::new(MessageRepository::new(db.clone()));
        let channels = Arc::new(ChannelRepository::new(db.clone()));
        let ephemeral = Arc::new(EphemeralRepository::new(db.clone()));
        let (event_tx, _rx) = broadcast::channel(16);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let (a, b, sender) = (ChannelId::new(), ChannelId::new(), UserId::new());
        let data = |channel: ChannelId| {
            padded(content::Content::DataMessage(DataMessage {
                body: "injected".into(),
                timestamp: 1_763_070_000_000,
                channel_id: channel.to_string(),
                ..Default::default()
            }))
        };
        let msg = DeviceMessage {
            message_guid: MessageId::new().to_string(),
            server_timestamp: 1,
            ..Default::default()
        };
        let attachments = crate::services::AttachmentService::new_local(Arc::new(
            crate::storage::repositories::AttachmentRepository::new(db.clone()),
        ));
        let count = |channel: &ChannelId| {
            messages
                .list_for_conversation(
                    channel,
                    &crate::domain::message::ConversationType::Channel,
                    None,
                    50,
                )
                .unwrap()
                .len()
        };

        let outcome = process_decrypted_data_message(
            &msg, &data(b), Some(a), &sender, &event_tx, &stats, &messages, &channels,
            &ephemeral, &attachments, None,
        );
        assert!(matches!(outcome, DataOutcome::Malformed(_)), "{outcome:?}");
        assert_eq!(count(&b), 0, "nothing written into the named channel");
        assert_eq!(count(&a), 0);

        // No envelope channel recovered: refused too.
        let outcome = process_decrypted_data_message(
            &msg, &data(a), None, &sender, &event_tx, &stats, &messages, &channels, &ephemeral,
            &attachments, None,
        );
        assert!(matches!(outcome, DataOutcome::Malformed(_)));

        // The bound case still stores.
        let outcome = process_decrypted_data_message(
            &msg, &data(a), Some(a), &sender, &event_tx, &stats, &messages, &channels,
            &ephemeral, &attachments, None,
        );
        assert!(matches!(outcome, DataOutcome::Handled), "{outcome:?}");
        assert_eq!(count(&a), 1);
    }

    /// Control content over a sender key is held to the same channel, and
    /// content naming no channel (a DM or sync event) is refused outright.
    #[test]
    fn control_content_is_bound_to_the_envelope_channel() {
        use chatter_proto::content_v1::{Reaction, TypingMessage};
        let (a, b) = (ChannelId::new(), ChannelId::new());
        let typing = |channel_id: String| {
            content::Content::Typing(TypingMessage {
                channel_id,
                ..Default::default()
            })
        };
        assert!(bound_to_channel(&typing(a.to_string()), Some(a)).is_ok());
        assert!(bound_to_channel(&typing(b.to_string()), Some(a)).is_err());
        assert!(bound_to_channel(&typing(String::new()), Some(a)).is_err());
        let reaction = |aci: String| {
            content::Content::Reaction(Reaction {
                emoji: "x".into(),
                target_author_aci: aci,
                ..Default::default()
            })
        };
        let author = UserId::new();
        assert!(bound_to_channel(&reaction(format!("channel:{a}:{author}")), Some(a)).is_ok());
        assert!(bound_to_channel(&reaction(format!("channel:{b}:{author}")), Some(a)).is_err());
        assert!(bound_to_channel(&reaction(author.to_string()), Some(a)).is_err());
    }
}
