use super::*;

impl MessageGateway {
    /// Decrypt an identified sender message (PreKey or Ciphertext).
    ///
    /// This is the fallback path when sealed sender is not used.
    /// The server knows the sender and passes source_user_id/source_device_id.
    pub async fn decrypt_identified_message(
        &self,
        envelope: &[u8],
        sender_id: &UserId,
        sender_device_id: u32,
    ) -> Result<SealedSenderResult, ServiceError> {
        // The identified path never STARTS a session. Its sender is the
        // server's claim alone, and a first identity key is trusted on first
        // use (`crypto/signal/store.rs` is_trusted_identity), so a PreKey
        // message here could pin a server-chosen key for a peer we never
        // talked to. Sessions are established over sealed sender, whose
        // certificate names the sender.
        let session_key = format!("{}:{}", sender_id, sender_device_id);
        if self
            .signal_service
            .key_store
            .load_session(&session_key)
            .map_err(ServiceError::Storage)?
            .is_none()
        {
            tracing::warn!(%sender_id, sender_device_id,
                "Identified message with no session for the sender; refusing to start one");
            return Err(ServiceError::Crypto(CryptoError::Protocol(
                "No session available for an identified sender".into(),
            )));
        }
        // libsignal's SessionRecord keeps the archived states and, for a
        // Whisper message the current state cannot open, tries them and
        // promotes the one that decrypts (libsignal `session_management.rs`
        // `try_decrypt_from_record`).
        let mut plaintext = self
            .signal_service
            .decrypt_message_identified(envelope, &sender_id.to_string(), sender_device_id)
            .await?;

        // Process plaintext and ensure zeroization
        let result = self
            .decrypt_identified_message_inner(sender_id, sender_device_id, &plaintext)
            .await;

        // Zeroize decrypted plaintext before returning (security: prevent heap residue)
        plaintext.zeroize();

        result
    }

    /// Inner implementation for identified message decryption.
    /// Separated to ensure plaintext zeroization in the outer function.
    async fn decrypt_identified_message_inner(
        &self,
        sender_id: &UserId,
        sender_device_id: u32,
        plaintext: &[u8],
    ) -> Result<SealedSenderResult, ServiceError> {
        // Same refusal as the sealed path, before anything is parsed. The
        // sender here is not asserted by the server: `plaintext` came out of
        // the Double Ratchet session filed under this address, so only someone
        // holding that session could have produced it.
        if sender_is_blocked(&self.user_repo, &self.settings_repo, sender_id)? {
            tracing::info!(
                sender_id = %sender_id,
                "GATEWAY: dropping identified-sender message from a blocked sender"
            );
            return Ok(SealedSenderResult::DroppedBlockedSender {
                sender_id: *sender_id,
            });
        }

        // First try to parse as Content proto (preferred, contains KeyExchange)
        if let Ok(content_proto) = crate::network::validation::protovalidate::decode_inbound_content(
            plaintext,
            "identified_sender",
        ) {
            // Check for KeyExchange
            if let Some(content::Content::KeyExchange(key_exchange)) = content_proto.content {
                tracing::debug!(
                    sender_id = %sender_id,
                    conversation_id = %key_exchange.conversation_id,
                    key_len = key_exchange.ephemeral_pk.len(),
                    "Received KeyExchange in identified sender message"
                );
                return Ok(SealedSenderResult::KeyExchange {
                    sender_id: *sender_id,
                    sender_device_id,
                    conversation_id: key_exchange.conversation_id,
                    ephemeral_public_key: key_exchange.ephemeral_pk,
                    timestamp: key_exchange.timestamp,
                });
            }

            // Handle DataMessage inside Content
            if let Some(content::Content::DataMessage(data_message)) = content_proto.content {
                return self
                    .store_data_message(data_message, Some(*sender_id))
                    .await;
            }

            // Other content types
            tracing::debug!(
                sender_id = %sender_id,
                "Received non-data content in identified sender message"
            );
            return Ok(SealedSenderResult::MessageStored);
        }

        Err(not_content(plaintext.len()))
    }
}
