//! Sealed sender service.
//!
//! Consolidates the common pattern of:
//! 1. Encrypt with Signal Protocol (pairwise)
//! 2. Get anonymous credential (blind signature token)
//! 3. Send via sealed sender with credential
//!
//! Used by server-managed groups for SKDM distribution and announcements.

use std::sync::Arc;

use crate::domain::UserId;
use crate::mailbox::token_manager::{MailboxToken, TokenManager};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::mailbox_client::MailboxClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::services::signal_service::SignalService;
use crate::util::time::TimestampNanos;

/// Errors from sealed sender operations.
#[derive(Debug, thiserror::Error)]
pub enum SealedSenderError {
    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// No token available.
    #[error("no anonymous credential available")]
    NoToken,

    /// Token request failed.
    #[error("token request failed: {0}")]
    TokenRequestFailed(String),

    /// Send failed.
    #[error("send failed: {0}")]
    SendFailed(String),
}

/// Sealed sender service for anonymous credential-based sending.
///
/// Consolidates the encrypt → get token → send with credential pattern
/// that's repeated across group messaging code.
#[derive(Clone)]
pub struct SealedSenderService {
    signal_service: Arc<SignalService>,
    grpc_pool: Arc<GrpcClientPool>,
}

impl SealedSenderService {
    /// Create a new SealedSenderService.
    pub fn new(signal_service: Arc<SignalService>, grpc_pool: Arc<GrpcClientPool>) -> Self {
        Self {
            signal_service,
            grpc_pool,
        }
    }

    /// Send plaintext to a recipient using sealed sender with anonymous credentials.
    ///
    /// This consolidates the common pattern:
    /// 1. Encrypt with Signal Protocol (pairwise Double Ratchet)
    /// 2. Get anonymous credential from token manager
    /// 3. Send via sealed sender message with credential
    ///
    /// # Arguments
    /// * `auth_client` - For session establishment if needed
    /// * `token_manager` - For anonymous credentials
    /// * `recipient_id` - The target user ID
    /// * `plaintext` - The data to send
    ///
    /// # Returns
    /// Ok if sent successfully, or error describing failure.
    pub async fn send_with_credential(
        &self,
        auth_client: &mut AuthClient,
        token_manager: &TokenManager,
        recipient_id: &UserId,
        plaintext: &[u8],
    ) -> Result<(), SealedSenderError> {
        let recipient_id_str = recipient_id.to_string();
        let device_id = 1u32;
        let timestamp = TimestampNanos::now().as_u64();

        // 1. Encrypt with Signal Protocol
        let sealed_bytes = self
            .signal_service
            .encrypt_message(auth_client, &recipient_id_str, device_id, plaintext)
            .await
            .map_err(|e| SealedSenderError::EncryptionFailed(e.to_string()))?;

        // 2. Get anonymous credential
        let token: MailboxToken = self
            .get_or_request_token(token_manager)
            .await
            .ok_or(SealedSenderError::NoToken)?;

        // 3. Build sealed sender message
        let sealed_message = SealedSenderMessage {
            sealed_body: sealed_bytes,
            server_key_id: 1,
            timestamp,
        };

        // 4. Send with credential
        let mut message_client = MessageClient::new(
            self.grpc_pool.channel(),
            self.grpc_pool.auth_clone(),
        );

        message_client
            .send_sealed_message_with_credential(
                &recipient_id_str,
                0, // device_id for routing
                sealed_message,
                &token.public_key,
                &token.signature,
                token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| SealedSenderError::SendFailed(e.to_string()))?;

        // 5. Mark token used
        let _ = token_manager.mark_token_used(&token.id);

        Ok(())
    }

    /// Get an available token or request new ones.
    ///
    /// This consolidates the token request + retry pattern used in multiple places.
    pub async fn get_or_request_token(
        &self,
        token_manager: &TokenManager,
    ) -> Option<MailboxToken> {
        // Try to get an available token
        match token_manager.get_available_token() {
            Ok(Some(token)) => return Some(token),
            Ok(None) => {
                // Request new tokens
                let mut mailbox_client = MailboxClient::new(
                    self.grpc_pool.channel(),
                    self.grpc_pool.auth_clone(),
                );

                match token_manager.request_tokens(&mut mailbox_client, 5).await {
                    Ok(count) if count > 0 => {
                        tracing::debug!("Requested {} new anonymous tokens", count);
                    }
                    Ok(_) => {
                        tracing::warn!("Token request returned 0 tokens");
                        return None;
                    }
                    Err(e) => {
                        tracing::error!("Failed to request tokens: {}", e);
                        return None;
                    }
                }

                // Retry get after request
                token_manager.get_available_token().ok().flatten()
            }
            Err(e) => {
                tracing::error!("Failed to check available tokens: {}", e);
                None
            }
        }
    }
}
