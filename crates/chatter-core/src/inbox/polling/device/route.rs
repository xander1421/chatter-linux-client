use super::*;

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
    anon_group_repo: &crate::storage::repositories::AnonGroupRepository,
    ephemeral_repo: &Arc<EphemeralRepository>,
    group_coordinator: &Arc<GroupCoordinator>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    retry_budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::AckDecision;
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
                    // Malformed and deterministic in the bytes — and with no
                    // parseable sender there is no conversation to mark.
                    // Retaining it would replay forever.
                    return AckDecision::Ack;
                }
            };

            tracing::debug!(
                message_guid = %msg.message_guid,
                sender_id = %sender_id,
                source_device_id = msg.source_device_id,
                "[INBOX] Routing to sender key message handler"
            );

            // Route through message gateway. Its answer decides the ack: this
            // used to discard the outcome and `return AckDecision::Ack`, so a
            // missing sender key or a failed store read deleted the message.
            return process_sender_key_message(
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
                retry_budget,
            )
            .await;
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
        return process_identified_sender_message(
            msg,
            event_tx,
            stats,
            message_gateway,
            message_repo,
            domain_tx,
            retry_budget,
        )
        .await;
    }

    // Check for ECIES_INIT (HPKE-encrypted initial contact, NDSS'21 compliant)
    if msg.envelope_type == EnvelopeType::EciesInit as i32 {
        tracing::info!(
            message_guid = %msg.message_guid,
            envelope_len = msg.envelope.len(),
            "[INBOX] Processing as ECIES init message (NDSS'21 private contact)"
        );
        return process_ecies_init_message(msg, event_tx, stats, message_gateway, domain_tx).await;
    }

    // Fall back to sealed sender processing
    tracing::info!(
        message_guid = %msg.message_guid,
        "[INBOX] Processing as sealed sender message"
    );
    process_sealed_sender_message(
        msg,
        pipeline,
        event_tx,
        stats,
        message_gateway,
        message_repo,
        channel_repo,
        anon_group_repo,
        domain_tx,
        retry_budget,
    )
    .await
}
