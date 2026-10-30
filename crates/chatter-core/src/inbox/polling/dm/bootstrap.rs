use super::*;

/// Try to parse a mailbox message as a bootstrap response.
///
/// Per NDSS '21, the bootstrap response has TWO encryption layers:
/// 1. **Transport Layer (Envelope)**: HPKE to our mailbox's X25519 encryption key
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
pub(super) async fn try_parse_bootstrap_response(
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
                peer_devices = conv.peer_mailboxes.len(),
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

    // Step 2: Decrypt the envelope layer with the mailbox's ENCRYPTION key
    // (its Ed25519 signing key only signs poll/ack challenges).
    let signal_ciphertext = match decrypt_envelope(envelope, &mailbox.encryption_private_key) {
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
        signal_ciphertext_len = signal_ciphertext.len(),
        "BOOTSTRAP[4/4] Layer 1 (envelope) decrypted, now decrypting Layer 2 (Signal)"
    );

    // Step 3: Decrypt the Signal Protocol content, against the responder's
    // device — named by the frame, since the Signal layer needs it to open.
    let frame = match crate::network::validation::protovalidate::decode_inbound::<
        crate::proto::content_v1::BootstrapResponseFrame,
    >(signal_ciphertext.as_slice(), "bootstrap_response_frame")
    {
        Ok(frame) => frame,
        Err(e) => {
            tracing::warn!(peer_id = %peer_id, error = %e, "Bootstrap response frame malformed");
            return None;
        }
    };
    match message_gateway
        .decrypt_identified_message(&frame.signal_ciphertext, &peer_id, frame.sender_device_id)
        .await
    {
        Ok(result) => {
            match result {
                crate::services::gateway::SealedSenderResult::KeyExchange {
                    ephemeral_public_key,
                    ..
                } => {
                    tracing::info!(
                        peer_id = %peer_id,
                        key_len = ephemeral_public_key.len(),
                        "BOOTSTRAP[4/4] Layer 2 (Signal) decrypted - got KeyExchange"
                    );

                    // Process the KeyExchange
                    let result = process_bootstrap_key_exchange(
                        &ephemeral_public_key,
                        peer_id,
                        frame.sender_device_id,
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
                crate::services::gateway::SealedSenderResult::MessageStored => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Decrypted and stored message (not a KeyExchange)"
                    );
                    None
                }
                crate::services::gateway::SealedSenderResult::ChannelReceiptTargetsSynced {
                    ..
                } => None,
                crate::services::gateway::SealedSenderResult::SenderKeyRequested { .. } => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Received SenderKeyRequest in DM bootstrap path (unexpected)"
                    );
                    None
                }
                crate::services::gateway::SealedSenderResult::MailboxAnnouncement { .. } => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Received MailboxAnnouncement in DM bootstrap path (unexpected)"
                    );
                    None
                }
                crate::services::gateway::SealedSenderResult::DataMessageReceived { .. } => {
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Received DataMessage in DM bootstrap path (unexpected)"
                    );
                    None
                }
                // A blocked peer answering our bootstrap. `None` means "no
                // shared secret was established", so no conversation keys are
                // saved and no DmBootstrapCompleted is emitted — the block
                // holds even against the handshake, not just against messages.
                // Spelled out rather than left to the wildcard because the
                // silent outcome and the "unexpected" outcomes below happen to
                // share a return value for entirely different reasons.
                crate::services::gateway::SealedSenderResult::DroppedBlockedSender { .. } => {
                    tracing::info!(
                        peer_id = %peer_id,
                        "DM bootstrap response from a blocked sender — dropped"
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
pub(super) fn process_bootstrap_key_exchange(
    ephemeral_pk: &[u8],
    peer_id: UserId,
    // The device that sent the RESPONSE: it made the mailbox it names.
    peer_device_id: u32,
    conversation_manager: &DmConversationManager,
) -> Option<Vec<u8>> {
    // The RESPONSE's encryption key goes unused: nothing is HPKE-sealed to
    // the responder's mailbox after the bootstrap (deposits there use the
    // conversation key). It is announced so both directions share one layout.
    let Some(announcement) =
        crate::mailbox::dm::bootstrap::MailboxAnnouncement::parse(ephemeral_pk)
    else {
        tracing::error!(
            peer_id = %peer_id,
            key_len = ephemeral_pk.len(),
            "Invalid bootstrap key length"
        );
        return None;
    };

    tracing::info!(
        peer_id = %peer_id,
        "Processing bootstrap key exchange from peer"
    );

    let peer_mailbox_pk = announcement.mailbox_pk;
    let peer_conversation_pk = announcement.conversation_pk;

    // That device's half only: a late device adds its own mailbox and
    // secret beside the others', never over them. Our half must exist — we
    // announced it in the bootstrap this answers.
    match conversation_manager.record_peer_device(
        &peer_id,
        peer_device_id,
        peer_mailbox_pk,
        peer_conversation_pk,
    ) {
        Ok(_) => {
            tracing::info!(peer_id = %peer_id, peer_device_id, "Recorded peer device's half from its bootstrap response");
            Some(ephemeral_pk.to_vec())
        }
        Err(e) => {
            tracing::error!(peer_id = %peer_id, peer_device_id, error = %e, "Bootstrap response not recorded");
            None
        }
    }
}
