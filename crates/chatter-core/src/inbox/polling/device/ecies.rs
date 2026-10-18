use super::*;

/// Process an ECIES init message (NDSS'21 private contact).
///
/// These messages are HPKE-encrypted to our identity key, containing:
/// - Conversation ID
/// - Sender's mailbox public key
/// - Sender's identity key
/// - Sender's delivery token
/// - Optional intro message
pub(super) async fn process_ecies_init_message(
    msg: &DeviceMessage,
    event_tx: &broadcast::Sender<InboxEvent>,
    stats: &Arc<RwLock<InboxStats>>,
    message_gateway: &Arc<MessageGateway>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::domain::anon_dm_init::AnonDmInitMessage;
    use crate::error::{CryptoError, ServiceError};
    use crate::inbox::ack_policy::AckDecision;

    // HPKE open against our identity key, inside the Signal actor. A failed
    // open is a property of the bytes; any other error is reading our key.
    let plaintext = match message_gateway.open_anon_dm_init(&msg.envelope).await {
        Ok(p) => p,
        Err(ServiceError::Crypto(CryptoError::DecryptionFailed(e))) => {
            tracing::error!(
                message_guid = %msg.message_guid,
                error = %e,
                "[INBOX_ECIES] Failed to open HPKE init message"
            );
            let _ = event_tx.send(InboxEvent::MessageFailed {
                message_id: msg.message_guid.clone(),
                error: format!("ECIES decryption failed: {}", e),
            });
            stats.write().messages_failed += 1;
            return ecies_init_decision(&EciesInitFailure::Malformed(
                "HPKE open failed against our identity key",
            ));
        }
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
            return ecies_init_decision(&EciesInitFailure::Malformed(
                "AnonDmInitMessage would not parse",
            ));
        }
    };

    tracing::info!(
        message_guid = %msg.message_guid,
        conversation_id = %init_msg.conversation_id,
        "[INBOX_ECIES] Successfully decrypted NDSS'21 private contact request"
    );

    // Emit domain event for the anon DM handler to process
    if let Some(tx) = domain_tx {
        let event = DomainEvent::AnonDmInitReceived {
            conversation_id: init_msg.conversation_id,
            sender_id: init_msg.sender_id,
            sender_device_id: init_msg.sender_device_id,
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
