//! Anonymous clients.
//!
//! Clients for operations WITHOUT authentication headers.
//! Used for anonymous DM requests where sender identity must not be leaked.
//!
//! # Important
//!
//! These clients do NOT use AuthInterceptor. They send requests with only
//! the blind signature token as credential, ensuring true sender anonymity.

use crate::network::grpc::transport::GrpcTransport;

use crate::error::NetworkError;
use crate::proto::chat_v1::{
    auth_service_client::AuthServiceClient,
    key_transparency_service_client::KeyTransparencyServiceClient,
    message_service_client::MessageServiceClient, GetConsistencyProofRequest,
    GetConsistencyProofResponse, GetPreKeyBundleRequest, GetPreKeyBundleResponse, GetProofRequest,
    GetProofResponse, SealedSenderMessage, SendSealedMessageRequest,
    SendSealedMessageResponse,
};

/// Anonymous message client - NO auth interceptor.
///
/// This client is specifically for anonymous operations where the sender's
/// identity must not be revealed to the server. Unlike `MessageClient`,
/// this does not include any authentication headers.
pub struct AnonymousMessageClient {
    /// Raw client without auth interceptor
    inner: MessageServiceClient<GrpcTransport>,
}

impl AnonymousMessageClient {
    /// Create a new anonymous client from a raw channel.
    ///
    /// # Important
    ///
    /// Pass a raw `GrpcTransport`, NOT one wrapped with `AuthInterceptor`.
    pub fn new(channel: GrpcTransport) -> Self {
        Self {
            inner: MessageServiceClient::new(channel),
        }
    }

    /// Send a sealed message authorized by the recipient's Unidentified Access
    /// Key, with NO `authorization` header.
    ///
    /// This is Signal's sealed-sender transport: possession of the recipient's
    /// UAK proves the sender is authorized to deliver to them, without telling
    /// the server *who* the sender is. The server validates the UAK
    /// (`validateSealedSenderUAK`, internal/middleware/auth.go), sets
    /// `SealedSenderModeKey=true`, and — crucially — never sets `UserIDKey`, so
    /// `device_messages.source_user_id` stays NULL.
    ///
    /// # Why this lives on `AnonymousMessageClient`
    ///
    /// It MUST go out on a raw, interceptor-free channel. `MessageClient` is
    /// wrapped by `AuthInterceptor`, which would attach Basic auth alongside the
    /// UAK; the server rejects requests carrying more than one auth method
    /// ("provide exactly one auth method"). Sending here is the only way to
    /// guarantee the `authorization` header is absent.
    ///
    /// # Arguments
    ///
    /// * `destination_user_id` - Recipient user ID (server knows the recipient)
    /// * `destination_device_id` - Recipient device ID
    /// * `sealed_message` - The sealed sender envelope (content encrypted)
    /// * `uak` - The recipient's 16-byte Unidentified Access Key
    ///
    /// # Privacy Guarantees
    ///
    /// - Server knows: recipient user ID
    /// - Server does NOT know: sender identity
    /// - Server cannot read: message content (sealed sender encryption)
    pub async fn send_sealed_message_with_uak(
        &mut self,
        destination_user_id: &str,
        destination_device_id: u32,
        sealed_message: SealedSenderMessage,
        uak: &[u8; 16],
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        use base64::Engine;
        use tonic::Request;

        let proto_request = SendSealedMessageRequest {
            destination_user_id: destination_user_id.to_string(),
            destination_device_id,
            sealed_message: Some(sealed_message),
            client_request_id: uuid::Uuid::new_v4().to_string(),
            envelope_type: 0, // Default: server determines from content
        };

        // Create request WITHOUT any auth headers
        let mut request = Request::new(proto_request);

        // Unidentified-Access-Key: base64 of the 16 raw bytes.
        let uak_b64 = base64::engine::general_purpose::STANDARD.encode(uak);
        request.metadata_mut().insert(
            "unidentified-access-key",
            uak_b64
                .parse()
                .map_err(|_| NetworkError::InvalidRequest("invalid UAK encoding".into()))?,
        );

        // The server validates the UAK against THIS user and cross-checks it
        // against the protobuf destination, so the two must agree.
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
    #[allow(clippy::too_many_arguments)]
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
    inner: AuthServiceClient<GrpcTransport>,
}

impl AnonymousAuthClient {
    /// Create a new anonymous auth client from a raw channel.
    pub fn new(channel: GrpcTransport) -> Self {
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

/// Anonymous Key Transparency client — for fetching a peer's signed KT
/// inclusion proof WITHOUT an authenticated identity.
///
/// # Why this exists (Phase 3a)
///
/// The identified DM path fetches KT proofs through the authenticated
/// `AuthClient` (`get_key_transparency_proof`) and rejects a peer bundle whose
/// identity key is not the KT-committed one (see
/// `SignalService::verify_peer_key_transparency`). The ANONYMOUS-DM path has no
/// authenticated identity to make that call with, so before Phase 3a it trusted
/// peer identity keys blindly — a MITM hole on exactly the metadata-private
/// path that most needs protection.
///
/// `GetProof` is a PUBLIC method server-side (a KT proof is public, auditable
/// transparency data), so this client can fetch it over the isolated anonymous
/// channel — no auth interceptor, no identity leaked — and the anon path then
/// KT-gates session establishment exactly like the identified path.
///
/// # Privacy note (Phase 3b, not yet closed)
///
/// The lookup key is still the plaintext `user_id`, so the server sees WHICH
/// peer is queried. Hiding that (VRF-indexed lookup) is the remaining
/// query-privacy work.
pub struct AnonymousKtClient {
    inner: KeyTransparencyServiceClient<GrpcTransport>,
}

impl AnonymousKtClient {
    /// Create a new anonymous KT client from a raw channel (NO auth interceptor).
    pub fn new(channel: GrpcTransport) -> Self {
        Self {
            inner: KeyTransparencyServiceClient::new(channel),
        }
    }

    /// Fetch a peer's signed KT inclusion proof anonymously.
    ///
    /// Mirrors `AuthClient::get_key_transparency_proof`: returns `Ok(None)` when
    /// the server reports `NotFound` (a pre-KT user with no leaf — the caller
    /// decides the no-proof/TOFU policy) and `Err` on any other transport/server
    /// failure (a network error is NOT evidence of a missing leaf and must not be
    /// silently treated as "no proof").
    pub async fn get_proof(
        &mut self,
        user_id: &str,
    ) -> Result<Option<GetProofResponse>, NetworkError> {
        let request = GetProofRequest {
            user_id: user_id.to_string(),
        };
        match self.inner.get_proof(request).await {
            Ok(resp) => Ok(Some(resp.into_inner())),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(NetworkError::from_status(&status)),
        }
    }

    /// Fetch a consistency proof between two tree-head epochs.
    ///
    /// The client uses this to verify the log is APPEND-ONLY between an epoch it
    /// previously trusted and a newer one, and to REJECT/alarm on a fork (a
    /// server that rewrote history). Like `get_proof`, `GetConsistencyProof` is a
    /// public method, so it works over the anonymous channel with no identity
    /// leaked.
    pub async fn get_consistency_proof(
        &mut self,
        old_epoch: u64,
        new_epoch: u64,
    ) -> Result<GetConsistencyProofResponse, NetworkError> {
        let request = GetConsistencyProofRequest {
            old_epoch,
            new_epoch,
        };
        match self.inner.get_consistency_proof(request).await {
            Ok(resp) => Ok(resp.into_inner()),
            Err(status) => Err(NetworkError::from_status(&status)),
        }
    }
}
