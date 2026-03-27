//! Authentication gRPC client.
//!
//! Client for authentication service operations including login, registration,
//! session management, and device management.
//!
//! # Privacy
//!
//! Usernames are hashed (SHA256) before being sent to the server. This prevents
//! the server from knowing actual usernames - it only stores/matches hashes.
//! If the database is compromised, attackers see hashes, not usernames.

use sha2::Sha256;
use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    auth_event, auth_service_client::AuthServiceClient, signal_pre_key_bundle,
    AccountDeletionEvent, AuthEvent, AuthResponse, ChangePasswordRequest,
    DeleteAccountRequest,
    GetActiveSessionsRequest, GetActiveSessionsResponse, GetPreKeyBundleRequest,
    GetPreKeyBundleResponse, GetPreKeyStatsRequest, GetPreKeyStatsResponse,
    GetSenderCertificateRequest, GetSenderCertificateResponse, InvalidateAllSessionsRequest,
    InvalidateSessionRequest, ListDevicesRequest, ListDevicesResponse, LoginRequest, LogoutRequest,
    RefreshPreKeysRequest, RegisterRequest, RegistrationResult, RenameDeviceRequest,
    RequestSenderCertificateRequest, RequestSenderCertificateResponse,
    ResendVerificationEmailRequest, RevokeDeviceRequest, SignalPreKeyBundle,
    UploadPreKeyBundleRequest, UseRecoveryCodeRequest, VerifyEmailRequest,
};
// NOTE: ServerTrustRootResponse removed - trust root is hardcoded in config.rs per Signal's security model

// ============================================================================
// Username Hashing
// ============================================================================

/// Hash a username with profile key for privacy before sending to server.
///
/// Uses HMAC-SHA256 with the profile key as the secret. The server never sees
/// the actual username - only this hash. This provides:
///
/// 1. **Privacy**: Username cannot be recovered from hash
/// 2. **Rainbow table resistance**: Profile key acts as unique salt
/// 3. **Unlinkability**: Same username with different profile keys = different hashes
///
/// # Security Model
///
/// - Username is for LOGIN only, not discovery
/// - Discovery uses QR codes / friend links with user_id
/// - Profile keys are exchanged via E2E encrypted messages
///
/// # Example
///
/// ```ignore
/// let hash = hash_username("alice", &profile_key);
/// // Unique hash that only this user can reproduce
/// ```
pub fn hash_username(username: &str, profile_key: &[u8; 32]) -> String {
    use hmac::{Hmac, Mac};

    type HmacSha256 = Hmac<Sha256>;

    // Normalize: lowercase and trim whitespace
    let normalized = username.trim().to_lowercase();

    // HMAC-SHA256 with profile_key as secret
    let mut mac = HmacSha256::new_from_slice(profile_key)
        .expect("HMAC can take key of any size");
    mac.update(normalized.as_bytes());
    let result = mac.finalize();

    // Hex encode for proto String compatibility
    hex::encode(result.into_bytes())
}

/// Auth service client wrapper.
///
/// Wraps the generated AuthServiceClient with ergonomic APIs and error mapping.
/// Create from GrpcClientPool components.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut auth = AuthClient::new(pool.channel(), pool.auth_clone());
/// let response = auth.login("alice", b"password", "linux-desktop").await?;
/// ```
pub struct AuthClient {
    inner: AuthServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl AuthClient {
    /// Create a new AuthClient from pool components.
    ///
    /// # Arguments
    ///
    /// * `channel` - Cloned channel from GrpcClientPool
    /// * `interceptor` - Cloned auth interceptor for credential injection
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: AuthServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Login with username and password.
    ///
    /// Returns session info and user profile on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn login(
        &mut self,
        username: &str,
        password_hash: &[u8],
        device_info: &str,
        svr_public_key: &[u8],
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
        device_id: Option<u32>,
    ) -> Result<AuthResponse, NetworkError> {
        let request = LoginRequest {
            // NOTE: Caller (auth_service) is responsible for hashing username
            username: username.to_string(),
            password_hash: password_hash.to_vec(),
            svr_public_key: svr_public_key.to_vec(),
            device_info: device_info.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            device_auth_token: device_auth_token.to_vec(),
            unidentified_access_key: unidentified_access_key.to_vec(),
            device_id: device_id.unwrap_or(0), // 0 = new device assignment
        };

        self.inner
            .login(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Logout current session.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The device ID to log out (0 for current device)
    pub async fn logout(&mut self, device_id: u32) -> Result<(), NetworkError> {
        let request = LogoutRequest { device_id };

        self.inner
            .logout(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Verify email with verification token.
    pub async fn verify_email(&mut self, verification_token: &str) -> Result<(), NetworkError> {
        let request = VerifyEmailRequest {
            verification_token: verification_token.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .verify_email(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Resend verification email.
    ///
    /// # Arguments
    ///
    /// * `email_or_username` - Email address or username to identify account
    pub async fn resend_verification_email(
        &mut self,
        email_or_username: &str,
    ) -> Result<(), NetworkError> {
        let request = ResendVerificationEmailRequest {
            email_or_username: email_or_username.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .resend_verification_email(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Request a registration challenge nonce from the server.
    ///
    /// # Arguments
    ///
    /// * `username` - Hashed username
    /// * `identity_public_key` - Identity public key (Curve25519)
    pub async fn request_registration_challenge(
        &mut self,
        username: &str,
        identity_public_key: &[u8],
    ) -> Result<crate::proto::chat_v1::RequestRegistrationChallengeResponse, NetworkError> {
        use crate::proto::chat_v1::RequestRegistrationChallengeRequest;

        tracing::debug!(
            "Requesting registration challenge - username: {}, identity_key_len: {}",
            username,
            identity_public_key.len()
        );

        let request = RequestRegistrationChallengeRequest {
            username: username.to_string(),
            identity_public_key: identity_public_key.to_vec(),
        };

        self.inner
            .request_registration_challenge(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| {
                tracing::error!("Registration challenge failed: {}", s);
                NetworkError::from_status(&s)
            })
    }

    /// Register a new user, returning stream of progress events.
    ///
    /// The stream yields AuthEvent messages with status updates:
    /// - AccountCreating
    /// - AuthzRelationsInitialized
    /// - RegistrationCompleted (with user_id and device_id)
    /// - RegistrationFailed (with error)
    ///
    /// # Arguments
    ///
    /// * `encrypted_display_name` - Display name encrypted with profile_key (AES-256-GCM)
    /// * `unidentified_access_key` - 16-byte UAK derived client-side from profile_key
    /// * `prekey_bundle` - Optional Signal Protocol prekey bundle for sealed sender
    /// * `initial_one_time_prekeys` - Initial batch of one-time prekeys (Signal sends 100)
    /// * `challenge_id` - Challenge ID from request_registration_challenge
    /// * `challenge_response` - Signature of challenge nonce
    ///
    /// # Security Note
    ///
    /// The profile_key NEVER leaves the client. Only the derived UAK (16 bytes) is sent.
    /// The display_name is encrypted client-side with profile_key before transmission.
    /// Server stores only ciphertext - it cannot read profile data.
    #[allow(clippy::too_many_arguments)]
    pub async fn register(
        &mut self,
        username: &str,
        password_hash: &[u8],
        encrypted_display_name: &[u8],
        svr_public_key: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
        _unidentified_access_key: &[u8; 16],
        prekey_bundle: Option<crate::proto::chat_v1::SignalPreKeyBundle>,
        initial_one_time_prekeys: Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>,
        challenge_id: &str,
        challenge_response: &[u8],
        registration_lock: &[u8; 32],
    ) -> Result<impl Stream<Item = Result<AuthEvent, NetworkError>>, NetworkError> {
        use base64::Engine;

        // Base64-encode encrypted display name for proto String field
        // Server stores this ciphertext - only clients with profile_key can decrypt
        let encoded_display_name =
            base64::engine::general_purpose::STANDARD.encode(encrypted_display_name);

        let request = RegisterRequest {
            username: username.to_string(),
            password_hash: password_hash.to_vec(),
            svr_public_key: svr_public_key.to_vec(),
            display_name: encoded_display_name,
            avatar_url: String::new(),
            device_info: device_info.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            device_auth_token: device_auth_token.to_vec(),
            registration_lock: registration_lock.to_vec(),
            prekey_bundle,
            initial_one_time_prekeys,
            challenge_response: challenge_response.to_vec(),
            challenge_id: challenge_id.to_string(),
        };

        let response = self
            .inner
            .register(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Register and wait for completion.
    ///
    /// Convenience method that consumes the event stream and returns
    /// the registration result with user_id and device_id on success.
    /// Note: Client already has device_auth_token and derives UAK from profile_key locally.
    ///
    /// # Arguments
    ///
    /// * `encrypted_display_name` - Display name encrypted with profile_key (AES-256-GCM)
    /// * `unidentified_access_key` - 16-byte UAK derived client-side from profile_key
    /// * `prekey_bundle` - Optional Signal Protocol prekey bundle for sealed sender
    /// * `initial_one_time_prekeys` - Initial batch of one-time prekeys (Signal sends 100)
    /// * `challenge_id` - Challenge ID from request_registration_challenge
    /// * `challenge_response` - Signature of challenge nonce
    ///
    /// # Security Note
    ///
    /// The profile_key NEVER leaves the client. Only the derived UAK (16 bytes) is sent.
    /// The display_name is encrypted client-side with profile_key before transmission.
    /// Server stores only ciphertext - it cannot read profile data.
    #[allow(clippy::too_many_arguments)]
    pub async fn register_and_wait(
        &mut self,
        username: &str,
        password_hash: &[u8],
        encrypted_display_name: &[u8],
        svr_public_key: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
        prekey_bundle: Option<crate::proto::chat_v1::SignalPreKeyBundle>,
        initial_one_time_prekeys: Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>,
        challenge_id: &str,
        challenge_response: &[u8],
        registration_lock: &[u8; 32],
    ) -> Result<RegistrationResult, NetworkError> {
        let stream = self
            .register(
                username,
                password_hash,
                encrypted_display_name,
                svr_public_key,
                device_info,
                device_auth_token,
                unidentified_access_key,
                prekey_bundle,
                initial_one_time_prekeys,
                challenge_id,
                challenge_response,
                registration_lock,
            )
            .await?;

        tokio::pin!(stream);

        while let Some(result) = stream.next().await {
            let event = result?;
            if let Some(payload) = event.payload {
                match payload {
                    auth_event::Payload::UserRegistered(_) => {
                        // Progress event, continue waiting for completion
                    }
                    auth_event::Payload::RegistrationComplete(result) => {
                        // Server returns user_id and device_id
                        // Client already has device_auth_token and UAK
                        return Ok(result);
                    }
                    auth_event::Payload::RegistrationError(error) => {
                        return Err(NetworkError::ServerError(error.message));
                    }
                }
            }
        }

        Err(NetworkError::Unknown(
            "registration stream ended without result".to_string(),
        ))
    }

    /// Change password for authenticated user.
    pub async fn change_password(
        &mut self,
        current_password_hash: &[u8],
        new_password_hash: &[u8],
    ) -> Result<(), NetworkError> {
        let request = ChangePasswordRequest {
            current_password_hash: current_password_hash.to_vec(),
            new_password_hash: new_password_hash.to_vec(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .change_password(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List all active sessions for current user.
    pub async fn list_sessions(&mut self) -> Result<GetActiveSessionsResponse, NetworkError> {
        let request = GetActiveSessionsRequest {};

        self.inner
            .list_sessions(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Revoke a specific session by device ID.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Signal device ID to invalidate
    pub async fn revoke_session(&mut self, device_id: u32) -> Result<(), NetworkError> {
        let request = InvalidateSessionRequest {
            device_id,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .revoke_session(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Revoke all sessions except current.
    pub async fn revoke_all_sessions(&mut self) -> Result<(), NetworkError> {
        let request = InvalidateAllSessionsRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .revoke_all_sessions(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List all devices for current user.
    pub async fn list_devices(&mut self) -> Result<ListDevicesResponse, NetworkError> {
        let request = ListDevicesRequest {};

        self.inner
            .list_devices(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Rename a device.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Signal device ID to rename
    /// * `device_name` - New name for the device
    pub async fn rename_device(
        &mut self,
        device_id: u32,
        device_name: &str,
    ) -> Result<(), NetworkError> {
        let request = RenameDeviceRequest {
            device_id,
            device_name: device_name.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .rename_device(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Revoke a device (unlink from account).
    ///
    /// # Arguments
    ///
    /// * `device_id` - Signal device ID to revoke
    pub async fn revoke_device(&mut self, device_id: u32) -> Result<(), NetworkError> {
        let request = RevokeDeviceRequest {
            device_id,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .revoke_device(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete account, returning stream of deletion events.
    ///
    /// This is an irreversible operation that streams progress updates.
    ///
    /// # Arguments
    ///
    /// * `password_hash` - Password for confirmation
    pub async fn delete_account(
        &mut self,
        password_hash: &[u8],
    ) -> Result<impl Stream<Item = Result<AccountDeletionEvent, NetworkError>>, NetworkError> {
        let request = DeleteAccountRequest {
            password_hash: password_hash.to_vec(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .delete_account(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    // ==================== Signal Protocol RPCs ====================

    /// Upload prekey bundle for this device.
    ///
    /// Called after registration and when prekeys need replenishment.
    /// Includes identity key, signed prekey, and optional one-time/kyber prekeys.
    ///
    /// # Arguments
    ///
    /// * `bundle` - Complete Signal prekey bundle for this device
    pub async fn upload_pre_key_bundle(
        &mut self,
        bundle: SignalPreKeyBundle,
    ) -> Result<(), NetworkError> {
        let request = UploadPreKeyBundleRequest {
            bundle: Some(bundle),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .upload_pre_key_bundle(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Fetch prekey bundle for establishing session with a user.
    ///
    /// Used during X3DH key agreement to encrypt first message.
    /// Specify device_id "*" to get bundles for all user's devices.
    ///
    /// # Arguments
    ///
    /// * `user_id` - Target user's UUID
    /// * `device_id` - Specific device ID or "*" for all devices
    pub async fn get_pre_key_bundle(
        &mut self,
        user_id: &str,
        device_id: &str,
    ) -> Result<GetPreKeyBundleResponse, NetworkError> {
        tracing::debug!(
            user_id,
            device_id,
            "AuthClient: fetching prekey bundle from server"
        );

        let request = GetPreKeyBundleRequest {
            user_id: user_id.to_string(),
            device_id: device_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let result = self.inner
            .get_pre_key_bundle(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| {
                tracing::error!(
                    user_id,
                    device_id,
                    error = %s,
                    "AuthClient: failed to fetch prekey bundle"
                );
                NetworkError::from_status(&s)
            })?;

        tracing::debug!(
            user_id,
            device_id,
            has_result = result.result.is_some(),
            "AuthClient: prekey bundle received"
        );
        Ok(result)
    }

    /// Fetch prekey bundles for all devices of a user.
    ///
    /// Convenience wrapper for get_pre_key_bundle with device_id="*".
    pub async fn get_pre_key_bundles_all_devices(
        &mut self,
        user_id: &str,
    ) -> Result<GetPreKeyBundleResponse, NetworkError> {
        self.get_pre_key_bundle(user_id, "*").await
    }

    /// Refresh one-time prekeys for this device.
    ///
    /// Called when prekey count drops below threshold.
    /// Typically uploads a fresh batch of 100 prekeys.
    ///
    /// # Arguments
    ///
    /// * `new_prekeys` - New batch of one-time prekeys to upload
    pub async fn refresh_pre_keys(
        &mut self,
        new_prekeys: Vec<signal_pre_key_bundle::PreKeyRecord>,
    ) -> Result<(), NetworkError> {
        let request = RefreshPreKeysRequest {
            new_one_time_prekeys: new_prekeys,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .refresh_pre_keys(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get prekey statistics for a device.
    ///
    /// Returns count of remaining one-time prekeys.
    /// Client should refresh when `should_refill` is true or count drops below ~10.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Device ID to check stats for (0 defaults to 1)
    pub async fn get_pre_key_stats(
        &mut self,
        device_id: u32,
    ) -> Result<GetPreKeyStatsResponse, NetworkError> {
        let request = GetPreKeyStatsRequest { device_id };

        self.inner
            .get_pre_key_stats(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Sealed Sender Certificate RPCs ====================

    /// Request a new sender certificate.
    ///
    /// Certificate is signed by server and used for sealed sender.
    /// Valid for up to 30 days (default 7 days). Cache and refresh before expiration.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Device ID for certificate
    /// * `validity_duration` - Optional custom validity period
    pub async fn request_sender_certificate(
        &mut self,
        device_id: u32,
        validity_duration: Option<std::time::Duration>,
    ) -> Result<RequestSenderCertificateResponse, NetworkError> {
        let request = RequestSenderCertificateRequest {
            device_id,
            validity_period: validity_duration.map(|d| prost_types::Duration {
                seconds: d.as_secs() as i64,
                nanos: d.subsec_nanos() as i32,
            }),
        };

        self.inner
            .request_sender_certificate(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get cached sender certificate if still valid.
    ///
    /// Returns certificate with `exists` and `expired` status flags.
    /// If expired or not exists, request a new certificate.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Device ID to get certificate for
    pub async fn get_sender_certificate(
        &mut self,
        device_id: u32,
    ) -> Result<GetSenderCertificateResponse, NetworkError> {
        let request = GetSenderCertificateRequest { device_id };

        self.inner
            .get_sender_certificate(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // NOTE: get_server_trust_root() removed - trust root is hardcoded in config.rs
    // per Signal's security model. See crate::config::TRUST_ROOT_PUBLIC_KEY

    // ==================== Account Recovery RPCs ====================

    /// Use a recovery code to reset password and SVR credentials.
    ///
    /// This is an unauthenticated RPC -- the recovery code itself is the credential.
    /// Returns the number of remaining unused recovery codes on success.
    /// After this succeeds, the user can login with the new password + new PIN.
    ///
    /// # Arguments
    ///
    /// * `username` - Hashed username
    /// * `recovery_code` - 8-character one-time recovery code
    /// * `new_password_hash` - SHA256 hash of the new password
    /// * `new_svr_public_key` - New SVR public key derived from new PIN
    /// * `new_registration_lock` - New registration lock derived from new PIN
    pub async fn use_recovery_code(
        &mut self,
        username: &str,
        recovery_code: &str,
        new_password_hash: &[u8],
        new_svr_public_key: &[u8],
        new_registration_lock: &[u8],
    ) -> Result<i32, NetworkError> {
        let request = UseRecoveryCodeRequest {
            username: username.to_string(),
            recovery_code: recovery_code.to_string(),
            new_password_hash: new_password_hash.to_vec(),
            new_svr_public_key: new_svr_public_key.to_vec(),
            new_registration_lock: new_registration_lock.to_vec(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .use_recovery_code(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.remaining_codes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_auth_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = AuthClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_request_id_generation() {
        // Verify we can generate valid request IDs
        let id = uuid::Uuid::new_v4().to_string();
        assert!(!id.is_empty());
        assert!(id.len() == 36); // UUID v4 string format
    }

    #[test]
    fn test_prekey_types_available() {
        // Verify Signal Protocol prekey types are accessible and constructible
        use crate::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};

        let signed = SignedPreKeyRecord {
            id: 1,
            public_key: vec![0u8; 32],
            signature: vec![0u8; 64],
            timestamp: 1234567890,
        };
        assert_eq!(signed.id, 1);
        assert_eq!(signed.public_key.len(), 32);
        assert_eq!(signed.signature.len(), 64);

        let otp = PreKeyRecord {
            id: 100,
            public_key: vec![0u8; 32],
        };
        assert_eq!(otp.id, 100);
        assert_eq!(otp.public_key.len(), 32);
    }

    #[test]
    fn test_signal_prekey_bundle_construction() {
        // Verify full SignalPreKeyBundle can be constructed
        use crate::proto::chat_v1::signal_pre_key_bundle::{PreKeyRecord, SignedPreKeyRecord};

        let bundle = SignalPreKeyBundle {
            identity_key: vec![0u8; 32],
            registration_id: 12345,
            device_id: 1,
            signed_prekey: Some(SignedPreKeyRecord {
                id: 1,
                public_key: vec![0u8; 32],
                signature: vec![0u8; 64],
                timestamp: 1234567890,
            }),
            one_time_prekey: Some(PreKeyRecord {
                id: 100,
                public_key: vec![0u8; 32],
            }),
            kyber_prekey: None,
            unidentified_access_key: Some(vec![0u8; 16]),
            unrestricted_unidentified_access: Some(false),
        };

        assert_eq!(bundle.registration_id, 12345);
        assert_eq!(bundle.device_id, 1);
        assert!(bundle.signed_prekey.is_some());
        assert!(bundle.one_time_prekey.is_some());
    }

    #[test]
    fn test_prekey_stats_response_fields() {
        // Verify GetPreKeyStatsResponse fields are accessible
        let stats = GetPreKeyStatsResponse {
            available_count: 95,
            should_refill: false,
            device_id: 1,
        };

        assert_eq!(stats.available_count, 95);
        assert!(!stats.should_refill);
        assert_eq!(stats.device_id, 1);
    }
}
