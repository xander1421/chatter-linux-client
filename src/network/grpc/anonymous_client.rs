//! Anonymous clients.
//!
//! Clients for operations WITHOUT authentication headers.
//! Used for anonymous DM requests where sender identity must not be leaked.
//!
//! # Important
//!
//! These clients do NOT use AuthInterceptor. They send requests with only
//! the blind signature token as credential, ensuring true sender anonymity.

use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::proto::chat_v1::{
    auth_service_client::AuthServiceClient, message_service_client::MessageServiceClient,
    GetPreKeyBundleRequest, GetPreKeyBundleResponse, SealedSenderMessage, SendSealedMessageRequest,
    SendSealedMessageResponse,
};

/// Anonymous message client - NO auth interceptor.
///
/// This client is specifically for anonymous operations where the sender's
/// identity must not be revealed to the server. Unlike `MessageClient`,
/// this does not include any authentication headers.
pub struct AnonymousMessageClient {
    /// Raw client without auth interceptor
    inner: MessageServiceClient<Channel>,
}

impl AnonymousMessageClient {
    /// Create a new anonymous client from a raw channel.
    ///
    /// # Important
    ///
    /// Pass a raw `Channel`, NOT one wrapped with `AuthInterceptor`.
    pub fn new(channel: Channel) -> Self {
        Self {
            inner: MessageServiceClient::new(channel),
        }
    }

    /// Send a sealed message using anonymous credential (blind signature).
    ///
    /// This is the core anonymous delivery method for:
    /// - Anonymous DM message requests
    /// - Any other anonymous contact flows
    ///
    /// The server can verify the sender has a valid account (via blind signature)
    /// but cannot determine WHO the sender is.
    ///
    /// # Arguments
    ///
    /// * `destination_user_id` - Recipient user ID (server knows recipient)
    /// * `destination_device_id` - Recipient device ID (0 for all devices)
    /// * `sealed_message` - The sealed sender envelope (content encrypted)
    /// * `public_key` - Ed25519 public key from the blind-signed token
    /// * `signature` - Server's blind signature over the public key
    /// * `randomizer` - Optional message randomizer for signature verification
    ///
    /// # Privacy Guarantees
    ///
    /// - Server knows: recipient user ID
    /// - Server does NOT know: sender identity (only that they're a valid user)
    /// - Server cannot read: message content (sealed sender encryption)
    /// - Server cannot link: multiple messages from same sender (one-time tokens)
    pub async fn send_with_anonymous_credential(
        &mut self,
        destination_user_id: &str,
        destination_device_id: u32,
        sealed_message: SealedSenderMessage,
        public_key: &[u8],
        signature: &[u8],
        randomizer: Option<&[u8]>,
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        self.send_with_anonymous_credential_typed(
            destination_user_id,
            destination_device_id,
            sealed_message,
            public_key,
            signature,
            randomizer,
            0, // Default envelope type (server will use SEALED_SENDER)
        )
        .await
    }

    /// Send with anonymous credential and explicit envelope type.
    ///
    /// Use envelope_type=4 for ECIES_INIT (HPKE-encrypted initial contact).
    pub async fn send_with_anonymous_credential_typed(
        &mut self,
        destination_user_id: &str,
        destination_device_id: u32,
        sealed_message: SealedSenderMessage,
        public_key: &[u8],
        signature: &[u8],
        randomizer: Option<&[u8]>,
        envelope_type: i32,
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        use base64::Engine;
        use tonic::Request;

        let proto_request = SendSealedMessageRequest {
            destination_user_id: destination_user_id.to_string(),
            destination_device_id,
            sealed_message: Some(sealed_message),
            client_request_id: uuid::Uuid::new_v4().to_string(),
            envelope_type,
        };

        // Create request WITHOUT any auth headers
        let mut request = Request::new(proto_request);

        // Build anonymous credential JSON (blind signature token)
        let credential = serde_json::json!({
            "public_key": base64::engine::general_purpose::STANDARD.encode(public_key),
            "signature": base64::engine::general_purpose::STANDARD.encode(signature),
            "randomizer": randomizer
                .map(|r| base64::engine::general_purpose::STANDARD.encode(r))
                .unwrap_or_default(),
        });

        // Add ONLY the anonymous credential - no auth token
        request.metadata_mut().insert(
            "x-anonymous-credential",
            credential
                .to_string()
                .parse()
                .map_err(|_| NetworkError::InvalidRequest("invalid credential encoding".into()))?,
        );

        // Add destination header (still needed to know where to deliver)
        request.metadata_mut().insert(
            "x-destination-user-id",
            destination_user_id
                .parse()
                .map_err(|_| NetworkError::InvalidRequest("invalid user ID".into()))?,
        );

        self.inner
            .send_sealed_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}

/// Anonymous auth client - for prekey fetching without revealing identity.
///
/// Uses blind signature tokens instead of auth headers.
pub struct AnonymousAuthClient {
    inner: AuthServiceClient<Channel>,
}

impl AnonymousAuthClient {
    /// Create a new anonymous auth client from a raw channel.
    pub fn new(channel: Channel) -> Self {
        Self {
            inner: AuthServiceClient::new(channel),
        }
    }

    /// Fetch prekey bundle anonymously using blind signature credential.
    ///
    /// This allows establishing a Signal session without revealing who
    /// is requesting the keys.
    ///
    /// # Privacy Guarantees
    ///
    /// - Server knows: which user's keys are being fetched
    /// - Server does NOT know: who is fetching the keys
    pub async fn get_pre_key_bundle_anonymous(
        &mut self,
        user_id: &str,
        device_id: &str,
        public_key: &[u8],
        signature: &[u8],
        randomizer: Option<&[u8]>,
    ) -> Result<GetPreKeyBundleResponse, NetworkError> {
        use base64::Engine;
        use tonic::Request;

        let proto_request = GetPreKeyBundleRequest {
            user_id: user_id.to_string(),
            device_id: device_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let mut request = Request::new(proto_request);

        // Build anonymous credential JSON (blind signature token)
        let credential = serde_json::json!({
            "public_key": base64::engine::general_purpose::STANDARD.encode(public_key),
            "signature": base64::engine::general_purpose::STANDARD.encode(signature),
            "randomizer": randomizer
                .map(|r| base64::engine::general_purpose::STANDARD.encode(r))
                .unwrap_or_default(),
        });

        // Add ONLY the anonymous credential - no auth token
        request.metadata_mut().insert(
            "x-anonymous-credential",
            credential
                .to_string()
                .parse()
                .map_err(|_| NetworkError::InvalidRequest("invalid credential encoding".into()))?,
        );

        self.inner
            .get_pre_key_bundle(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
