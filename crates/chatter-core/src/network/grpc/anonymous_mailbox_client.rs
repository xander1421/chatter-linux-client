//! Anonymous mailbox client.
//!
//! Client for ephemeral mailbox operations WITHOUT authentication headers.
//! Used for anonymous group messages and any mailbox operations where
//! the user's identity must not be leaked.
//!
//! # Important
//!
//! This client does NOT use AuthInterceptor. Operations authenticate via:
//! - Blind signature tokens (for mailbox creation)
//! - Challenge-response with mailbox private key (for polling/ack)
//!
//! # Which client to use?
//!
//! | Operation | Client | Auth Method |
//! |-----------|--------|-------------|
//! | get_mailbox_tokens | MailboxClient | Auth token (rate limiting) |
//! | create_ephemeral_mailbox | AnonymousMailboxClient | Blind signature |
//! | send_to_ephemeral_mailbox | AnonymousMailboxClient | None (fire and forget) |
//! | poll_ephemeral_mailbox | AnonymousMailboxClient | Challenge-response |
//! | acknowledge_messages | AnonymousMailboxClient | Challenge-response |
//! | get_mailbox_challenge | AnonymousMailboxClient | None |
//! | get_blind_signature_public_key | Either | None |

use crate::network::grpc::transport::GrpcTransport;

use crate::error::NetworkError;
use crate::proto::mailbox_v1::{
    AcknowledgeMessagesRequest, AcknowledgeMessagesResponse, CreateEphemeralMailboxRequest,
    CreateEphemeralMailboxResponse, GetBlindSignaturePublicKeyRequest,
    GetBlindSignaturePublicKeyResponse, GetMailboxChallengeRequest, GetMailboxChallengeResponse,
    JoinAnonCallRoomRequest, JoinAnonCallRoomResponse, PollEphemeralMailboxRequest,
    PollEphemeralMailboxResponse, SendToEphemeralMailboxRequest, SendToEphemeralMailboxResponse,
    ephemeral_mailbox_service_client::EphemeralMailboxServiceClient,
};

/// Anonymous mailbox client - NO auth interceptor.
///
/// This client is for mailbox operations that should NOT reveal user identity.
/// Authentication is done via blind signatures or challenge-response, NOT auth tokens.
pub struct AnonymousMailboxClient {
    /// Raw client without auth interceptor
    inner: EphemeralMailboxServiceClient<GrpcTransport>,
}

impl AnonymousMailboxClient {
    /// Create a new anonymous mailbox client from a raw channel.
    ///
    /// # Important
    ///
    /// Pass a raw `GrpcTransport`, NOT one wrapped with `AuthInterceptor`.
    pub fn new(channel: GrpcTransport) -> Self {
        Self {
            inner: EphemeralMailboxServiceClient::new(channel),
        }
    }

    /// Get the server's blind signature public key.
    ///
    /// Returns the RSA-2048 public key used for blind signatures.
    /// This is a public operation - no auth needed.
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
    /// This is anonymous - server doesn't know who is requesting the challenge.
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

    /// Create an ephemeral mailbox anonymously.
    ///
    /// Uses a blind signature token to prove the right to create a mailbox
    /// without revealing identity. Server cannot link this to the token request.
    ///
    /// # Arguments
    ///
    /// * `public_key` - The ephemeral public key (Ed25519)
    /// * `signature` - The unblinded blind signature (proves valid user)
    /// * `ttl_seconds` - Requested TTL in seconds
    /// * `message_randomizer` - Randomizer used during blinding
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

    /// Send a message to an ephemeral mailbox anonymously.
    ///
    /// Delivers an encrypted envelope to the destination mailbox.
    /// This is completely anonymous - no auth, no identity.
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

    /// Poll an ephemeral mailbox for messages anonymously.
    ///
    /// Retrieves messages using challenge-response authentication.
    /// Server verifies ownership via signature, not auth token.
    ///
    /// # Arguments
    ///
    /// * `public_key` - Public key of the mailbox
    /// * `challenge_signature` - Ed25519 signature over the challenge
    /// * `challenge` - The challenge that was signed
    /// * `after_message_id` - Only return messages after this ID
    /// * `limit` - Max messages to return
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

    /// Acknowledge messages from a mailbox anonymously.
    ///
    /// Deletes messages using challenge-response authentication.
    /// Server verifies ownership via signature, not auth token.
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

    /// Join an anonymous call room using a blind signature token.
    ///
    /// Proves authorization via blind signature without revealing identity.
    /// Returns LiveKit credentials for joining the call.
    ///
    /// # Arguments
    ///
    /// * `room_name` - The room name (must start with "anon-")
    /// * `public_key` - The ephemeral public key (Ed25519)
    /// * `signature` - The unblinded blind signature
    /// * `message_randomizer` - Randomizer used during blinding
    /// * `display_name` - Display name for this participant
    /// * `call_type` - Call type (1 = voice, 2 = video)
    pub async fn join_anon_call_room(
        &mut self,
        room_name: String,
        public_key: Vec<u8>,
        signature: Vec<u8>,
        message_randomizer: Vec<u8>,
        display_name: String,
        call_type: i32,
    ) -> Result<JoinAnonCallRoomResponse, NetworkError> {
        let request = JoinAnonCallRoomRequest {
            room_name,
            public_key,
            signature,
            message_randomizer,
            display_name,
            call_type,
        };

        self.inner
            .join_anon_call_room(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
