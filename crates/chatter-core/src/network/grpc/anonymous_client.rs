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
use chatter_proto::chat_v1::{
    GetPreKeyBundleRequest, GetPreKeyBundleResponse, KtSearchRequest, KtSearchResponse, SealedDelivery,
    SealedDeviceMessage, SendGroupMessageRequest, SendGroupMessageResponse,
    SendSealedMessageRequest, SendSealedMessageResponse,
    auth_service_client::AuthServiceClient,
    key_transparency_service_client::KeyTransparencyServiceClient,
    message_service_client::MessageServiceClient,
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

    /// One sealed send authorized by the recipient's Unidentified Access Key,
    /// with NO `authorization` header: every device's envelope in one request,
    /// queued all together or not at all (`MessageClient::send_sealed_messages`
    /// has the semantics of `delivery` and `client_request_id`).
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
    /// # Privacy Guarantees
    ///
    /// - Server knows: recipient user ID
    /// - Server does NOT know: sender identity
    /// - Server cannot read: message content (sealed sender encryption)
    pub async fn send_sealed_messages_with_uak(
        &mut self,
        destination_user_id: &str,
        messages: Vec<SealedDeviceMessage>,
        delivery: SealedDelivery,
        client_request_id: &str,
        uak: &[u8; 16],
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        use base64::Engine;
        use tonic::Request;

        let proto_request = SendSealedMessageRequest {
            destination_user_id: destination_user_id.to_string(),
            messages,
            client_request_id: client_request_id.to_string(),
            envelope_type: 0, // Default: server determines from content
            delivery: delivery as i32,
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
            .map_err(|s| NetworkError::from_status(&s))
            .and_then(|r| super::message_client::sealed_send_result(r.into_inner()))
    }

    /// One anonymous send to every member device of a private channel: an
    /// SSv2 multi-recipient message authorized by a group send token, with NO
    /// authorization header (the server refuses one). See
    /// `SendGroupMessageRequest` in message.proto.
    pub async fn send_group_message(
        &mut self,
        channel_id: &str,
        group_send_token: Vec<u8>,
        multi_recipient_message: Vec<u8>,
        client_request_id: &str,
    ) -> Result<SendGroupMessageResponse, NetworkError> {
        self.inner
            .send_group_message(SendGroupMessageRequest {
                channel_id: channel_id.to_string(),
                group_send_token,
                multi_recipient_message,
                client_request_id: client_request_id.to_string(),
            })
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// One sealed send under an anonymous credential (blind signature), with
    /// an explicit envelope type (4 = ECIES_INIT, 0 = server default).
    ///
    /// This is the core anonymous delivery method for:
    /// - Anonymous DM message requests
    /// - The DM bootstrap
    ///
    /// The server can verify the sender has a valid account (via blind signature)
    /// but cannot determine WHO the sender is. One credential covers the whole
    /// send, every device included.
    ///
    /// # Privacy Guarantees
    ///
    /// - Server knows: recipient user ID
    /// - Server does NOT know: sender identity (only that they're a valid user)
    /// - Server cannot read: message content (sealed sender encryption)
    /// - Server cannot link: multiple messages from same sender (one-time tokens)
    ///
    /// `client_request_id` is the server's idempotency key: a resend under the
    /// same id is queued at most once per device.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_anonymous_credential(
        &mut self,
        client_request_id: &str,
        destination_user_id: &str,
        messages: Vec<SealedDeviceMessage>,
        delivery: SealedDelivery,
        envelope_type: i32,
        public_key: &[u8],
        signature: &[u8],
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        use base64::Engine;
        use tonic::Request;

        let proto_request = SendSealedMessageRequest {
            destination_user_id: destination_user_id.to_string(),
            messages,
            client_request_id: client_request_id.to_string(),
            envelope_type,
            delivery: delivery as i32,
        };

        // Create request WITHOUT any auth headers
        let mut request = Request::new(proto_request);

        // Build anonymous credential JSON (blind signature token)
        let credential = serde_json::json!({
            "public_key": base64::engine::general_purpose::STANDARD.encode(public_key),
            "signature": base64::engine::general_purpose::STANDARD.encode(signature),
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
            .map_err(|s| NetworkError::from_status(&s))
            .and_then(|r| super::message_client::sealed_send_result(r.into_inner()))
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

/// Key transparency on the supplied route, without an auth interceptor.
pub struct AnonymousKtClient {
    inner: KeyTransparencyServiceClient<GrpcTransport>,
}

impl AnonymousKtClient {
    pub fn new(channel: GrpcTransport) -> Self {
        Self { inner: KeyTransparencyServiceClient::new(channel) }
    }

    pub async fn search(
        &mut self,
        request: KtSearchRequest,
    ) -> Result<Option<KtSearchResponse>, NetworkError> {
        match self.inner.search(request).await {
            Ok(response) => Ok(Some(response.into_inner())),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(NetworkError::from_status(&status)),
        }
    }
}
