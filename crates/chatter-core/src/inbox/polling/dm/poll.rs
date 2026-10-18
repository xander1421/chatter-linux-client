use super::*;

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
    mailbox_client: &Arc<tokio::sync::Mutex<AnonymousMailboxClient>>,
    message_gateway: &Arc<MessageGateway>,
    event_tx: &broadcast::Sender<InboxEvent>,
    settings_repo: &Arc<SettingsRepository>,
    message_repo: &Arc<MessageRepository>,
    attachment_repo: &Arc<AttachmentRepository>,
    user_repo: &Arc<UserRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    deduplicator: &MessageDeduplicator,
    retry_budget: &ack_policy::RetryBudget,
) {
    // Get current user ID for DM channel derivation
    let current_user_id: Option<UserId> = settings_repo
        .get_string(keys::CURRENT_USER_ID)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());
    let local_recipient = current_user_id.as_ref().and_then(|uuid| {
        let device_id = settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten()?;
        local_recipient_address(uuid, device_id)
    });
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
                if matches!(
                    error,
                    MailboxError::MailboxNotFound | MailboxError::TokenExpired
                ) {
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

                // Acking makes the SERVER DELETE the message, so ids go in here
                // per-message, only once `ack_policy` has decided this one is
                // safe to destroy. Collecting them all up front — which is what
                // this did — acked failures too, and a failed DM vanished with
                // no marker of any kind.
                let mut message_ids: Vec<String> = Vec::with_capacity(result.messages.len());

                // Per-device, per-direction keys: every peer device deposits
                // into our one mailbox under its own secret, so the key that
                // opens an envelope also names the device that sent it.
                let mut recv_keys: Vec<(u32, [u8; 32])> = match conversation_manager
                    .load_conversation(&peer_id)
                {
                    Ok(Some(conv)) => conv.recv_keys(),
                    Ok(None) => {
                        tracing::warn!(peer_id = %peer_id, "No conversation state for DM");
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::error!(peer_id = %peer_id, error = %e, "Failed to load conversation");
                        Vec::new()
                    }
                };

                // Process each message through the gateway
                for msg in result.messages {
                    total_messages += 1;

                    // Already handled earlier in this session — the server just
                    // re-delivered it because an `acknowledge` failed. Don't
                    // decrypt it again ("old counter" errors), but DO ack it:
                    // it was processed, so draining it loses nothing.
                    //
                    // Note this only *reads* the dedup set. Marking here (which
                    // is what `mark_processed` did) meant a message we chose to
                    // retain was skipped on its redelivery and never retried.
                    if deduplicator.is_duplicate(&msg.message_id) {
                        tracing::debug!(
                            peer_id = %peer_id,
                            message_id = %msg.message_id,
                            "DM message already processed, skipping decryption"
                        );
                        message_ids.push(msg.message_id.clone());
                        continue;
                    }

                    // A trial AEAD open picks the device's key; nothing opening
                    // is either a device's bootstrap RESPONSE (a different
                    // envelope) or a failure, reported under the first key.
                    let opening = recv_keys
                        .iter()
                        .find(|(_, key)| {
                            crate::crypto::dm_crypto::decrypt_dm_bytes(&msg.encrypted_envelope, key)
                                .is_ok()
                        })
                        .copied();
                    let response = if opening.is_none() {
                        try_parse_bootstrap_response(
                            &msg.encrypted_envelope,
                            peer_id,
                            conversation_manager,
                            message_gateway,
                        )
                        .await
                    } else {
                        None
                    };

                    let decision = if response.is_some() {
                        // NOTE: We intentionally do NOT emit KeyExchangeReceived here.
                        // This is a bootstrap RESPONSE (we were the initiator), not a new
                        // bootstrap request. The dm_bootstrap_handler handles NEW bootstraps
                        // from device inbox, not responses we receive in our DM mailbox.
                        // Emitting here would cause the handler to send a duplicate response.
                        tracing::info!(
                            peer_id = %peer_id,
                            "BOOTSTRAP[4/4] SUCCESS - RESPONSE processed, device secret established"
                        );
                        // The answering device's key opens the rest of this batch.
                        recv_keys = conversation_manager
                            .load_conversation(&peer_id)
                            .ok()
                            .flatten()
                            .map(|c| c.recv_keys())
                            .unwrap_or_default();
                        // Flushes what is queued, and what is owed to this device.
                        let _ = event_tx.send(InboxEvent::DmBootstrapCompleted { peer_id });
                        AckDecision::Ack
                    } else if let Some((sender_device_id, secret)) =
                        opening.or(recv_keys.first().copied())
                    {
                        // Decrypt with Signal Protocol (forward secrecy via Double Ratchet)
                        match message_gateway
                            .process_dm_message(
                                &msg.encrypted_envelope,
                                &secret,
                                msg.server_timestamp,
                            )
                            .await
                        {
                            // Blocked sender: the gateway authenticated them
                            // and refused before parsing, so there is nothing
                            // to dispatch. Ack anyway — an unacked envelope is
                            // re-fetched and re-decrypted on every poll.
                            Ok((_, DmContent::DroppedBlockedSender)) => {
                                crate::inbox::ack_policy::drop_and_ack(
                                    "blocked sender",
                                    &msg.message_id,
                                    retry_budget,
                                )
                            }
                            Ok((sender_id, dm_content)) => {
                                // Disappearing timer stated by this message
                                // (Signal's expireTimer): (timer, sent_at,
                                // is a timer update, is a visible message).
                                let disappearing = match &dm_content {
                                    DmContent::Text {
                                        stated_timer,
                                        expire_timer: ttl,
                                        timer_update,
                                        body,
                                        attachments,
                                        timestamp,
                                        ..
                                    } => Some((
                                        *stated_timer,
                                        if *timestamp > 0 {
                                            *timestamp
                                        } else {
                                            msg.server_timestamp
                                        },
                                        *timer_update,
                                        !body.is_empty() || !attachments.is_empty(),
                                        *ttl,
                                    )),
                                    _ => None,
                                };
                                if let Some((timer, sent_at, update, visible, _)) = disappearing {
                                    if update || visible {
                                        let notice = message_gateway.adopt_server_dm_timer(
                                            &peer_id, &sender_id, timer, sent_at,
                                        );
                                        if let (Some(notice), Some(tx)) = (notice, domain_tx) {
                                            let _ = tx.send(DomainEvent::MessageReceived {
                                                message: notice,
                                            });
                                        }
                                    }
                                }
                                dispatch_dm_content(
                                    dm_content,
                                    &sender_id,
                                    sender_device_id,
                                    peer_id,
                                    &msg.message_id,
                                    msg.server_timestamp,
                                    &current_user_id,
                                    event_tx,
                                    message_repo,
                                    attachment_repo,
                                    user_repo,
                                    conversation_manager,
                                    domain_tx,
                                );
                                // Arm the copy just stored (filed under the
                                // sender's (id, sent_at) key) so the reaper
                                // removes it when the timer runs out.
                                if let Some((_, sent_at, _, true, secs)) = disappearing {
                                    if secs > 0 {
                                        if let Ok(Some(stored)) = message_repo
                                            .get_by_sender_timestamp(&sender_id, sent_at)
                                        {
                                            message_gateway.arm_disappearing(&stored.id, secs);
                                        }
                                    }
                                }
                                AckDecision::Ack
                            }
                            Err(e)
                                if addressed_to_another_device(&e, local_recipient.as_deref()) =>
                            {
                                ack_policy::drop_and_ack(
                                    "sealed for another of our devices",
                                    &msg.message_id,
                                    retry_budget,
                                )
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                // The session's ratchet state is gone and this
                                // path sends no DecryptionErrorMessage, so the
                                // session is reset instead (Signal's
                                // lightSessionReset).
                                let session_desync = matches!(
                                    e,
                                    crate::error::ServiceError::Crypto(
                                        crate::error::CryptoError::SessionDesync { .. }
                                    )
                                );
                                tracing::error!(
                                    peer_id = %peer_id,
                                    message_id = %msg.message_id,
                                    error = %e,
                                    session_desync,
                                    "DM decryption failed"
                                );
                                let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                                    peer_id,
                                    error: error_str,
                                    session_desync,
                                });
                                dm_failure_decision(
                                    peer_id,
                                    msg.server_timestamp,
                                    &e,
                                    message_repo,
                                    domain_tx,
                                    &msg.message_id,
                                    retry_budget,
                                )
                            }
                        }
                    } else {
                        tracing::warn!(
                            peer_id = %peer_id,
                            message_id = %msg.message_id,
                            "BOOTSTRAP[4/4] FAILED - no peer device has a key yet and this is not a \
                             bootstrap response; leaving it queued until the session exists"
                        );
                        let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                            peer_id,
                            error: "No shared secret established".to_string(),
                            session_desync: false, // No session to reset
                        });
                        // TRANSIENT, and the clearest case of it: we have no
                        // session yet. The bootstrap can still complete via
                        // the device inbox, and then these same bytes
                        // decrypt. Acking here deleted every message that
                        // arrived before its own bootstrap landed.
                        //
                        // Bounded, though: if the bootstrap never completes
                        // this is a re-decrypt of the same bytes on every
                        // poll for as long as the client runs.
                        ack_policy::retain_within_budget(
                            ack_policy::dm_target(peer_id, msg.server_timestamp),
                            message_repo,
                            domain_tx,
                            &msg.message_id,
                            retry_budget,
                            ack_policy::now_ns(),
                            "no shared secret established for this peer yet",
                        )
                    };

                    match decision {
                        // Safe to destroy the server's copy. Record it so a
                        // failed `acknowledge` does not make us decrypt it
                        // twice on the replay.
                        AckDecision::Ack => {
                            deduplicator.mark_processed(&msg.message_id);
                            retry_budget.forget(&msg.message_id);
                            message_ids.push(msg.message_id.clone());
                        }
                        // Deliberately NOT marked processed — it must be
                        // decrypted again when the server re-delivers it.
                        AckDecision::Retain => {}
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
