//! Mailbox gRPC client.
//!
//! Client for ephemeral mailbox service operations.
//! Enables ephemeral mailbox management for privacy-preserving messaging (NDSS '21 system).

use crate::network::grpc::transport::GrpcTransport;
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::mailbox_v1::{
    AcknowledgeMessagesRequest, AcknowledgeMessagesResponse, CreateEphemeralMailboxRequest,
    CreateEphemeralMailboxResponse, GetBlindSignaturePublicKeyRequest,
    GetBlindSignaturePublicKeyResponse, GetMailboxChallengeRequest, GetMailboxChallengeResponse,
    GetMailboxTokensRequest, GetMailboxTokensResponse, PollEphemeralMailboxRequest,
    PollEphemeralMailboxResponse, SendToEphemeralMailboxRequest, SendToEphemeralMailboxResponse,
    ephemeral_mailbox_service_client::EphemeralMailboxServiceClient,
};

/// Mailbox service client wrapper.
///
/// Wraps the generated EphemeralMailboxServiceClient with ergonomic APIs and error mapping.
/// Provides operations for blind signature tokens, ephemeral mailbox creation, and message delivery.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut mailbox = MailboxClient::new(pool.channel(), pool.auth_clone());
/// let tokens = mailbox.get_mailbox_tokens(5, vec![], "manifest-key-id".into()).await?;
/// ```
pub struct MailboxClient {
    inner: EphemeralMailboxServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl MailboxClient {
    /// Create a new MailboxClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: EphemeralMailboxServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Token & Challenge RPCs ====================

    /// Get mailbox tokens with blind signatures.
    ///
    /// Requests blind signatures on blinded ephemeral public keys.
    /// Used to create ephemeral mailboxes anonymously.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of tokens requested (max 10 per request)
    /// * `blinded_keys` - Blinded ephemeral public keys (one per token)
    pub async fn get_mailbox_tokens(
        &mut self,
        count: i32,
        blinded_keys: Vec<Vec<u8>>,
        key_id: String,
    ) -> Result<GetMailboxTokensResponse, NetworkError> {
        let request = GetMailboxTokensRequest {
            count,
            blinded_keys,
            key_id,
        };

        self.inner
            .get_mailbox_tokens(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get the server's blind signature public key.
    ///
    /// Returns the RSA-2048 public key used for blind signatures.
    /// Clients use this to blind their ephemeral keys before requesting tokens.
    pub async fn get_blind_signature_public_key(
        &mut self,
    ) -> Result<GetBlindSignaturePublicKeyResponse, NetworkError> {
        let request = GetBlindSignaturePublicKeyRequest {};

        self.inner
            .get_blind_signature_public_key(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get a challenge for mailbox authentication.
    ///
    /// Returns a random challenge that must be signed with the mailbox's private key.
    /// Used for poll and acknowledge operations.
    ///
    /// # Arguments
    ///
    /// * `public_key` - Public key of the mailbox to authenticate
    pub async fn get_mailbox_challenge(
        &mut self,
        public_key: Vec<u8>,
    ) -> Result<GetMailboxChallengeResponse, NetworkError> {
        let request = GetMailboxChallengeRequest { public_key };

        self.inner
            .get_mailbox_challenge(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Mailbox Management RPCs ====================

    /// Create an ephemeral mailbox.
    ///
    /// Creates a mailbox using an unblinded token (public key + signature).
    /// This operation is anonymous - the server cannot link to the token request.
    ///
    /// # Arguments
    ///
    /// * `public_key` - The ephemeral public key (unblinded, Ed25519)
    /// * `signature` - The unblinded signature (proves right to create)
    /// * `ttl_seconds` - Requested TTL in seconds (server may cap at 30 days)
    pub async fn create_ephemeral_mailbox(
        &mut self,
        public_key: Vec<u8>,
        signature: Vec<u8>,
        ttl_seconds: u64,
        message_randomizer: Vec<u8>,
    ) -> Result<CreateEphemeralMailboxResponse, NetworkError> {
        let request = CreateEphemeralMailboxRequest {
            public_key,
            signature,
            ttl_seconds,
            message_randomizer,
        };

        self.inner
            .create_ephemeral_mailbox(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Poll an ephemeral mailbox for messages.
    ///
    /// Retrieves messages from the mailbox. Requires a signed challenge for authentication.
    ///
    /// # Arguments
    ///
    /// * `public_key` - Public key of the mailbox
    /// * `challenge_signature` - Ed25519 signature over the challenge
    /// * `challenge` - The challenge that was signed
    /// * `after_message_id` - Only return messages after this ID (for pagination)
    /// * `limit` - Max messages to return (default 100)
    pub async fn poll_ephemeral_mailbox(
        &mut self,
        public_key: Vec<u8>,
        challenge_signature: Vec<u8>,
        challenge: Vec<u8>,
        after_message_id: Option<&str>,
        limit: i32,
    ) -> Result<PollEphemeralMailboxResponse, NetworkError> {
        let request = PollEphemeralMailboxRequest {
            public_key,
            challenge_signature,
            challenge,
            after_message_id: after_message_id.unwrap_or("").to_string(),
            limit,
        };

        self.inner
            .poll_ephemeral_mailbox(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Message Operations RPCs ====================

    /// Send a message to an ephemeral mailbox.
    ///
    /// Delivers an encrypted envelope to the destination mailbox.
    /// This operation is anonymous.
    ///
    /// # Arguments
    ///
    /// * `destination_public_key` - Destination mailbox public key
    /// * `encrypted_envelope` - Encrypted envelope (sealed sender format)
    /// * `envelope_type` - Type of envelope (SealedSender, PrekeyMessage, Message)
    pub async fn send_to_ephemeral_mailbox(
        &mut self,
        destination_public_key: Vec<u8>,
        encrypted_envelope: Vec<u8>,
        envelope_type: i32,
    ) -> Result<SendToEphemeralMailboxResponse, NetworkError> {
        let request = SendToEphemeralMailboxRequest {
            destination_public_key,
            encrypted_envelope,
            envelope_type,
        };

        self.inner
            .send_to_ephemeral_mailbox(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Acknowledge messages from a mailbox.
    ///
    /// Acknowledges (and deletes) messages from the mailbox.
    /// Requires a signed challenge for authentication.
    ///
    /// # Arguments
    ///
    /// * `public_key` - Public key of the mailbox
    /// * `challenge_signature` - Ed25519 signature over the challenge
    /// * `challenge` - The challenge that was signed
    /// * `message_ids` - Message IDs to acknowledge (will be deleted)
    pub async fn acknowledge_messages(
        &mut self,
        public_key: Vec<u8>,
        challenge_signature: Vec<u8>,
        challenge: Vec<u8>,
        message_ids: Vec<String>,
    ) -> Result<AcknowledgeMessagesResponse, NetworkError> {
        let request = AcknowledgeMessagesRequest {
            public_key,
            challenge_signature,
            challenge,
            message_ids,
        };

        self.inner
            .acknowledge_messages(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_mailbox_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
            let _client = MailboxClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_get_mailbox_tokens_request_construction() {
        // Verify GetMailboxTokensRequest can be constructed
        let blinded_key = vec![0u8; 256]; // RSA-2048 blinded key
        let request = GetMailboxTokensRequest {
            count: 5,
            key_id: "test-key".into(),
            blinded_keys: vec![blinded_key.clone(), blinded_key],
        };

        assert_eq!(request.count, 5);
        assert_eq!(request.blinded_keys.len(), 2);
    }

    #[test]
    fn test_create_ephemeral_mailbox_request_construction() {
        // Verify CreateEphemeralMailboxRequest can be constructed
        let request = CreateEphemeralMailboxRequest {
            public_key: vec![0u8; 32],         // Ed25519 public key
            signature: vec![0u8; 256],         // RSA-2048 unblinded signature
            ttl_seconds: 86400,                // 1 day
            message_randomizer: vec![0u8; 32], // RFC 9474 randomizer
        };

        assert_eq!(request.public_key.len(), 32);
        assert_eq!(request.signature.len(), 256);
        assert_eq!(request.ttl_seconds, 86400);
        assert_eq!(request.message_randomizer.len(), 32);
    }

    #[test]
    fn test_poll_ephemeral_mailbox_request_construction() {
        // Verify PollEphemeralMailboxRequest can be constructed
        let request = PollEphemeralMailboxRequest {
            public_key: vec![0u8; 32],
            challenge_signature: vec![0u8; 64], // Ed25519 signature
            challenge: vec![0u8; 32],
            after_message_id: "msg-123".to_string(),
            limit: 100,
        };

        assert_eq!(request.public_key.len(), 32);
        assert_eq!(request.challenge_signature.len(), 64);
        assert_eq!(request.challenge.len(), 32);
        assert_eq!(request.after_message_id, "msg-123");
        assert_eq!(request.limit, 100);
    }

    #[test]
    fn test_send_to_ephemeral_mailbox_request_construction() {
        // Verify SendToEphemeralMailboxRequest can be constructed
        use crate::proto::mailbox_v1::EnvelopeType;

        let request = SendToEphemeralMailboxRequest {
            destination_public_key: vec![0u8; 32],
            encrypted_envelope: vec![0u8; 512],
            envelope_type: EnvelopeType::SealedSender as i32,
        };

        assert_eq!(request.destination_public_key.len(), 32);
        assert_eq!(request.encrypted_envelope.len(), 512);
        assert_eq!(request.envelope_type, EnvelopeType::SealedSender as i32);
    }

    #[test]
    fn test_acknowledge_messages_request_construction() {
        // Verify AcknowledgeMessagesRequest can be constructed
        let request = AcknowledgeMessagesRequest {
            public_key: vec![0u8; 32],
            challenge_signature: vec![0u8; 64],
            challenge: vec![0u8; 32],
            message_ids: vec!["msg-1".to_string(), "msg-2".to_string()],
        };

        assert_eq!(request.message_ids.len(), 2);
        assert_eq!(request.message_ids[0], "msg-1");
    }

    #[test]
    fn test_envelope_type_values() {
        // Verify EnvelopeType enum values
        use crate::proto::mailbox_v1::EnvelopeType;

        assert_eq!(EnvelopeType::Unspecified as i32, 0);
        assert_eq!(EnvelopeType::SealedSender as i32, 1);
        assert_eq!(EnvelopeType::PrekeyMessage as i32, 2);
        assert_eq!(EnvelopeType::Message as i32, 3);
    }
}
