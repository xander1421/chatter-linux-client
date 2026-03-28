use chrono::Utc;
use prost::Message as ProstMessage;
use uuid::Uuid;
use crate::domain::anon_dm::{AnonDmId, AnonDmMailbox};
use crate::domain::UserId;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::proto::content_v1::Content;
use crate::proto::mailbox_v1::EnvelopeType;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Send content to a peer's device inbox (for message requests).
    ///
    /// Uses FULLY ANONYMOUS operations:
    /// 1. Anonymous prekey fetch (blind signature token #1)
    /// 2. Anonymous message send (blind signature token #2)
    pub(crate) async fn send_to_device_inbox(
        &self,
        peer_id: &UserId,
        content: Content,
    ) -> Result<(), AnonDmError> {
        // Serialize content
        let plaintext = content.encode_to_vec();

        // Get token #1 for anonymous prekey fetch
        let prekey_token = self.get_or_request_token().await?;

        // Encrypt with Signal Protocol ANONYMOUSLY
        let ciphertext = self
            .pairwise_service
            .encrypt_anonymous(peer_id, &plaintext, &prekey_token)
            .await
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Mark prekey token as used
        self.token_manager
            .mark_token_used(&prekey_token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        // Get token #2 for anonymous message send
        let send_token = self.get_or_request_token().await?;

        // Build SealedSenderMessage
        let server_key_id = self.settings_repo
            .get::<u32>(crate::storage::repositories::settings_repo::keys::SENDER_CERTIFICATE_SERVER_KEY_ID)
            .ok()
            .flatten()
            .unwrap_or(1);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time should be after Unix epoch")
            .as_nanos() as u64;
        let sealed_message = SealedSenderMessage {
            sealed_body: ciphertext,
            server_key_id,
            timestamp,
        };

        // Send to device inbox using ANONYMOUS client (no auth headers)
        let mut anon_client = self.grpc_pool.anonymous_message_client();

        anon_client
            .send_with_anonymous_credential(
                &peer_id.to_string(),
                0, // All devices
                sealed_message,
                &send_token.public_key,
                &send_token.signature,
                send_token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| AnonDmError::MessageError(format!("Failed to send to device inbox: {}", e)))?;

        // Mark send token as used
        self.token_manager
            .mark_token_used(&send_token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        tracing::debug!(
            peer_id = %peer_id,
            "Sent to device inbox using fully anonymous path"
        );

        Ok(())
    }

    /// Get an available token or request new ones.
    pub(crate) async fn get_or_request_token(&self) -> Result<crate::mailbox::token_manager::MailboxToken, AnonDmError> {
        match self.token_manager.get_available_token() {
            Ok(Some(token)) => Ok(token),
            Ok(None) => {
                // Request new tokens from server
                let mut mailbox_client = self.grpc_pool.mailbox_client();
                self.token_manager
                    .request_tokens(&mut mailbox_client, 5)
                    .await
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

                self.token_manager
                    .get_available_token()
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
                    .ok_or_else(|| AnonDmError::CryptoError("No tokens available".to_string()))
            }
            Err(e) => Err(AnonDmError::CryptoError(e.to_string())),
        }
    }

    /// Send content to a peer's anonymous mailbox.
    pub(crate) async fn send_to_anon_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_pk: &[u8; 32],
        content: Content,
    ) -> Result<(), AnonDmError> {
        // Serialize content
        let plaintext = content.encode_to_vec();

        tracing::debug!(
            peer_id = %peer_id,
            mailbox_pk = hex::encode(&mailbox_pk[..8]),
            plaintext_len = plaintext.len(),
            "[ANON_DM_MAILBOX] Encrypting message for peer"
        );

        // Encrypt with Signal Protocol
        let ciphertext = self
            .pairwise_service
            .encrypt(peer_id, &plaintext)
            .await
            .map_err(|e| {
                tracing::error!(
                    peer_id = %peer_id,
                    error = %e,
                    "[ANON_DM_MAILBOX] Failed to encrypt"
                );
                AnonDmError::CryptoError(e.to_string())
            })?;

        tracing::debug!(
            peer_id = %peer_id,
            mailbox_pk = hex::encode(&mailbox_pk[..8]),
            ciphertext_len = ciphertext.len(),
            "[ANON_DM_MAILBOX] Sending to mailbox"
        );

        // Send to anonymous mailbox
        self.mailbox_manager
            .send_to_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
            .await
            .map_err(|e| {
                tracing::error!(
                    mailbox_pk = hex::encode(&mailbox_pk[..8]),
                    error = %e,
                    "[ANON_DM_MAILBOX] Failed to send to mailbox"
                );
                AnonDmError::MessageError(e.to_string())
            })?;

        tracing::info!(
            peer_id = %peer_id,
            mailbox_pk = hex::encode(&mailbox_pk[..8]),
            "[ANON_DM_MAILBOX] Successfully sent to peer mailbox"
        );

        Ok(())
    }

    /// Create a mailbox for a conversation using blind signature tokens.
    pub(crate) async fn create_conversation_mailbox(
        &self,
        conversation_id: &AnonDmId,
    ) -> Result<AnonDmMailbox, AnonDmError> {
        // Get or request a blind-signed token
        let token = match self.token_manager.get_available_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                // Request new tokens from server
                let mut mailbox_client = self.grpc_pool.mailbox_client();
                self.token_manager
                    .request_tokens(&mut mailbox_client, 5)
                    .await
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

                self.token_manager
                    .get_available_token()
                    .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
                    .ok_or_else(|| AnonDmError::CryptoError("No tokens available".to_string()))?
            }
            Err(e) => return Err(AnonDmError::CryptoError(e.to_string())),
        };

        // Use the token's Ed25519 keypair for the mailbox
        let public_key: [u8; 32] = token
            .public_key
            .clone()
            .try_into()
            .map_err(|_| AnonDmError::InvalidData("Invalid public key length".to_string()))?;
        let private_key: [u8; 64] = token
            .private_key
            .clone()
            .try_into()
            .map_err(|_| AnonDmError::InvalidData("Invalid private key length".to_string()))?;

        // Register the mailbox with the server using the blind signature (ANONYMOUS - no auth headers)
        // Retry on transient transport errors (connection not ready, HTTP/2 reset, etc.)
        let ttl_seconds = 30 * 24 * 60 * 60; // 30 days
        let mut last_err = None;
        let mut response_result = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1))).await;
                tracing::info!(attempt = attempt + 1, "Retrying create_ephemeral_mailbox");
            }
            let mut mailbox_client = self.grpc_pool.anonymous_mailbox_client();
            match mailbox_client
                .create_ephemeral_mailbox(
                    token.public_key.clone(),
                    token.signature.clone(),
                    ttl_seconds,
                    token.randomizer.clone().unwrap_or_default(),
                )
                .await
            {
                Ok(resp) => {
                    response_result = Some(resp);
                    break;
                }
                Err(e) if e.is_retryable() => {
                    tracing::warn!(attempt = attempt + 1, error = %e, "create_ephemeral_mailbox transient error, disconnecting stale channel and retrying");
                    self.grpc_pool.disconnect();
                    last_err = Some(e);
                }
                Err(e) => {
                    return Err(AnonDmError::MessageError(format!("Failed to create mailbox on server: {}", e)));
                }
            }
        }
        let response = response_result.ok_or_else(|| {
            AnonDmError::MessageError(format!(
                "Failed to create mailbox on server after 3 attempts: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            ))
        })?;

        tracing::info!(
            conversation_id = %conversation_id,
            mailbox_id = %response.mailbox_id,
            expires_at = response.expires_at,
            "Created ephemeral mailbox for anonymous DM"
        );

        let now = Utc::now();
        let mailbox = AnonDmMailbox {
            id: Uuid::new_v4().to_string(),
            conversation_id: *conversation_id,
            public_key,
            private_key,
            token_id: token.id.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::days(30),
        };

        // Mark the token as used
        self.token_manager
            .mark_token_used(&token.id)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?;

        Ok(mailbox)
    }
}
