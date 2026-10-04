//! Authentication gRPC client.
//!
//! Client for authentication service operations including login, registration,
//! session management, and device management.
//!
//! # Privacy
//!
//! Usernames are hashed before being sent to the server
//! (`auth_crypto::login_identifier`), and passwords never are: every password
//! check is an OPAQUE exchange (`crypto::opaque`).

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use crate::network::grpc::transport::GrpcTransport;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    auth_event, auth_service_client::AuthServiceClient,
    key_transparency_service_client::KeyTransparencyServiceClient, signal_pre_key_bundle,
    AccountDeletionEvent, AuthEvent, AuthResponse, ChangePasswordFinishRequest,
    ChangePasswordStartRequest, ChangePasswordStartResponse, DeleteAccountRequest,
    GetActiveSessionsRequest, GetActiveSessionsResponse, GetPreKeyBundleRequest,
    GetConsistencyProofRequest, GetConsistencyProofResponse, GetPreKeyBundleResponse,
    GetPreKeyStatsRequest, GetPreKeyStatsResponse, GetProofRequest,
    GetProofResponse, GetSealedSenderPolicyRequest, GetSealedSenderPolicyResponse,
    SetSealedSenderPolicyRequest, SetSealedSenderPolicyResponse, SetUnidentifiedAccessKeyRequest,
    InvalidateAllSessionsRequest,
    InvalidateSessionRequest, ListDevicesRequest, ListDevicesResponse, LoginFinishRequest,
    LoginStartRequest, LoginStartResponse, LogoutRequest, PasswordProof, PasswordProofStartRequest,
    PasswordProofStartResponse,
    RefreshPreKeysRequest, RegenerateRecoveryCodesRequest, RegisterRequest, RegistrationResult,
    RenameDeviceRequest, RequestSenderCertificateRequest, RequestSenderCertificateResponse,
    RepublishIdentityKeyRequest, RepublishIdentityKeyResponse,
    RevokeDeviceRequest, SignalPreKeyBundle,
    UploadPreKeyBundleRequest, UseRecoveryCodeFinishRequest, UseRecoveryCodeFinishResponse, UseRecoveryCodeStartRequest,
    UseRecoveryCodeStartResponse,
};
// NOTE: ServerTrustRootResponse removed - trust root is hardcoded in config.rs per Signal's security model

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
/// let started = auth.login_start(&identifier, &ke1, None, &[]).await?;
/// ```
pub struct AuthClient {
    inner: AuthServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
    /// Key Transparency client, built on the same channel + auth interceptor.
    /// Used to fetch a peer's signed inclusion proof before trusting the
    /// identity key in their prekey bundle (Phase 2 KT verification).
    kt: KeyTransparencyServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl AuthClient {
    /// Create a new AuthClient from pool components.
    ///
    /// # Arguments
    ///
    /// * `channel` - Cloned channel from GrpcClientPool
    /// * `interceptor` - Cloned auth interceptor for credential injection
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: AuthServiceClient::with_interceptor(channel.clone(), interceptor.clone()),
            kt: KeyTransparencyServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Fetch the Key Transparency inclusion proof for `user_id`.
    ///
    /// Returns `Ok(Some(proof))` when the user has a leaf in the KT log,
    /// `Ok(None)` when the server reports `NotFound` (a pre-KT user with no
    /// leaf — the caller decides the no-proof policy), and `Err` on any other
    /// transport/server failure (which must NOT be silently treated as "no
    /// proof": a network error is not evidence of a missing leaf).
    pub async fn get_key_transparency_proof(
        &mut self,
        user_id: &str,
    ) -> Result<Option<GetProofResponse>, NetworkError> {
        let request = GetProofRequest {
            user_id: user_id.to_string(),
        };
        match self.kt.get_proof(request).await {
            Ok(resp) => Ok(Some(resp.into_inner())),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(NetworkError::from_status(&status)),
        }
    }

    /// Fetch a consistency proof between two tree-head epochs.
    ///
    /// Used by `services::signal::kt_anchor` to prove the KT log only GREW
    /// between the head this device pinned and a newer one. Mirrors
    /// `AnonymousKtClient::get_consistency_proof` for the identified path.
    pub async fn get_consistency_proof(
        &mut self,
        old_epoch: u64,
        new_epoch: u64,
    ) -> Result<GetConsistencyProofResponse, NetworkError> {
        let request = GetConsistencyProofRequest {
            old_epoch,
            new_epoch,
        };
        match self.kt.get_consistency_proof(request).await {
            Ok(resp) => Ok(resp.into_inner()),
            Err(status) => Err(NetworkError::from_status(&status)),
        }
    }

    /// OPAQUE login, first round trip: KE1 for the login identifier. The
    /// answer is a KE2, or a `pow_challenge` to repeat the call with: `pow`
    /// is `(challenge token, solution)` for that repeat, `None` first.
    /// `login_grant` is a recovery's grant (empty for none).
    pub async fn login_start(
        &mut self,
        identifier: &str,
        ke1: &[u8],
        pow: Option<(&[u8], u64)>,
        login_grant: &[u8],
    ) -> Result<LoginStartResponse, NetworkError> {
        let (pow_token, pow_solution) = pow.map_or((Vec::new(), 0), |(t, s)| (t.to_vec(), s));
        let request = LoginStartRequest {
            username: identifier.to_string(),
            ke1: ke1.to_vec(),
            pow_token,
            pow_solution,
            login_grant: login_grant.to_vec(),
        };
        self.inner
            .login_start(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// OPAQUE login, second round trip: KE3 plus the device fields the
    /// session is created with. Returns session info and user profile.
    pub async fn login_finish(
        &mut self,
        login_id: &str,
        ke3: &[u8],
        device_info: &str,
        svr_public_key: &[u8],
        device_auth_token: &[u8],
        device_id: Option<u32>,
    ) -> Result<AuthResponse, NetworkError> {
        let request = LoginFinishRequest {
            login_id: login_id.to_string(),
            ke3: ke3.to_vec(),
            svr_public_key: svr_public_key.to_vec(),
            device_info: device_info.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            device_auth_token: device_auth_token.to_vec(),
            device_id: device_id.unwrap_or(0), // 0 = new device assignment
        };
        self.inner
            .login_finish(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Open a password re-confirmation against the caller's own record.
    pub async fn password_proof_start(
        &mut self,
        ke1: &[u8],
    ) -> Result<PasswordProofStartResponse, NetworkError> {
        self.inner
            .password_proof_start(PasswordProofStartRequest { ke1: ke1.to_vec() })
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Spend a password proof on nothing but the check itself.
    pub async fn verify_password(&mut self, proof: PasswordProof) -> Result<(), NetworkError> {
        self.inner
            .verify_password(proof)
            .await
            .map(|_| ())
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

    /// Get current identity from the server.
    pub async fn get_my_identity(
        &mut self,
    ) -> Result<crate::proto::chat_v1::GetMyIdentityResponse, NetworkError> {
        let request = crate::proto::chat_v1::GetMyIdentityRequest {};

        self.inner
            .get_my_identity(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Read the caller's sealed-sender RECEIVING policy from the server
    /// (users.unrestricted_unidentified_access). `allow_from_anyone == false`
    /// means contacts-only; `true` means anyone may send sealed messages.
    pub async fn get_sealed_sender_policy(
        &mut self,
    ) -> Result<GetSealedSenderPolicyResponse, NetworkError> {
        let request = GetSealedSenderPolicyRequest {};

        self.inner
            .get_sealed_sender_policy(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Set the caller's sealed-sender RECEIVING policy on the server. This
    /// changes what the server DOES on inbound sealed messages — it is not a
    /// local-only flag. Sealed *sending* is unaffected (always on).
    pub async fn set_sealed_sender_policy(
        &mut self,
        allow_from_anyone: bool,
    ) -> Result<SetSealedSenderPolicyResponse, NetworkError> {
        let request = SetSealedSenderPolicyRequest { allow_from_anyone };

        self.inner
            .set_sealed_sender_policy(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Replace our UAK after a profile-key rotation (the receiving policy
    /// flag is kept server-side).
    pub async fn set_unidentified_access_key(&mut self, uak: &[u8; 16]) -> Result<(), NetworkError> {
        let request = SetUnidentifiedAccessKeyRequest { unidentified_access_key: uak.to_vec() };

        self.inner
            .set_unidentified_access_key(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Request a registration challenge nonce from the server.
    ///
    /// # Arguments
    ///
    /// * `username` - Login identifier (`auth_crypto::login_identifier`)
    /// * `identity_public_key` - Identity public key (Curve25519)
    /// * `opaque_registration_request` - OPAQUE RegistrationRequest; the
    ///   response carries the server's RegistrationResponse
    pub async fn request_registration_challenge(
        &mut self,
        username: &str,
        identity_public_key: &[u8],
        opaque_registration_request: &[u8],
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
            opaque_registration_request: opaque_registration_request.to_vec(),
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
    /// * `challenge_token` - `challenge.token` from request_registration_challenge, echoed
    /// * `challenge_response` - Signature of challenge nonce
    /// * `pow_solution` - Proof of work for the challenge's difficulty
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
        opaque_registration_record: &[u8],
        encrypted_display_name: &[u8],
        svr_public_key: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
        prekey_bundle: Option<crate::proto::chat_v1::SignalPreKeyBundle>,
        initial_one_time_prekeys: Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>,
        challenge_token: &[u8],
        challenge_response: &[u8],
        pow_solution: u64,
        registration_lock: &[u8; 32],
        username_lookup_hash: &[u8; 32],
        username_proof: &[u8],
    ) -> Result<impl Stream<Item = Result<AuthEvent, NetworkError>>, NetworkError> {
        use base64::Engine;

        // Base64-encode encrypted display name for proto String field
        // Server stores this ciphertext - only clients with profile_key can decrypt
        let encoded_display_name =
            base64::engine::general_purpose::STANDARD.encode(encrypted_display_name);

        let request = RegisterRequest {
            username: username.to_string(),
            opaque_registration_record: opaque_registration_record.to_vec(),
            svr_public_key: svr_public_key.to_vec(),
            display_name: encoded_display_name,
            device_info: device_info.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            device_auth_token: device_auth_token.to_vec(),
            registration_lock: registration_lock.to_vec(),
            prekey_bundle,
            initial_one_time_prekeys,
            challenge_response: challenge_response.to_vec(),
            challenge_token: challenge_token.to_vec(),
            pow_solution,
            username_lookup_hash: username_lookup_hash.to_vec(),
            username_proof: username_proof.to_vec(),
            unidentified_access_key: unidentified_access_key.to_vec(),
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
    /// * `challenge_token` - `challenge.token` from request_registration_challenge, echoed
    /// * `challenge_response` - Signature of challenge nonce
    /// * `pow_solution` - Proof of work for the challenge's difficulty
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
        opaque_registration_record: &[u8],
        encrypted_display_name: &[u8],
        svr_public_key: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
        prekey_bundle: Option<crate::proto::chat_v1::SignalPreKeyBundle>,
        initial_one_time_prekeys: Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>,
        challenge_token: &[u8],
        challenge_response: &[u8],
        pow_solution: u64,
        registration_lock: &[u8; 32],
        username_lookup_hash: &[u8; 32],
        username_proof: &[u8],
    ) -> Result<RegistrationResult, NetworkError> {
        let stream = self
            .register(
                username,
                opaque_registration_record,
                encrypted_display_name,
                svr_public_key,
                device_info,
                device_auth_token,
                unidentified_access_key,
                prekey_bundle,
                initial_one_time_prekeys,
                challenge_token,
                challenge_response,
                pow_solution,
                registration_lock,
                username_lookup_hash,
                username_proof,
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

    /// Password change, first round trip: KE1 against the current record and
    /// the new password's RegistrationRequest.
    pub async fn change_password_start(
        &mut self,
        ke1: &[u8],
        registration_request: &[u8],
    ) -> Result<ChangePasswordStartResponse, NetworkError> {
        let request = ChangePasswordStartRequest {
            ke1: ke1.to_vec(),
            registration_request: registration_request.to_vec(),
        };
        self.inner
            .change_password_start(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Password change, second round trip: KE3 for the current password and
    /// the new RegistrationRecord.
    pub async fn change_password_finish(
        &mut self,
        flow_id: &str,
        ke3: &[u8],
        registration_record: &[u8],
    ) -> Result<(), NetworkError> {
        let request = ChangePasswordFinishRequest {
            flow_id: flow_id.to_string(),
            ke3: ke3.to_vec(),
            registration_record: registration_record.to_vec(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        self.inner
            .change_password_finish(request)
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
    /// * `encrypted_name` - Serialized `DeviceName` from
    ///   [`crate::crypto::device_name::encrypt_device_name`]; the server only
    ///   stores it, it cannot read it
    pub async fn rename_device(
        &mut self,
        device_id: u32,
        encrypted_name: Vec<u8>,
    ) -> Result<(), NetworkError> {
        let request = RenameDeviceRequest {
            device_id,
            encrypted_name,
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
    /// * `password_proof` - fresh OPAQUE proof of the current password
    pub async fn delete_account(
        &mut self,
        password_proof: PasswordProof,
    ) -> Result<impl Stream<Item = Result<AccountDeletionEvent, NetworkError>>, NetworkError> {
        let request = DeleteAccountRequest {
            password_proof: Some(password_proof),
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

    /// Replace this account's ENTIRE Signal key material with the bundle from
    /// this device.
    ///
    /// The one RPC by which an account's identity key may change, and the
    /// missing half of "I lost my phone, restore with my PIN":
    ///
    ///   - After a PIN restore the identity key is the ORIGINAL one, but the
    ///     private halves of every prekey the account ever published died with
    ///     the old device. Those rows must be replaced or the account keeps
    ///     handing out prekeys nobody can complete a handshake with.
    ///   - When no backup existed, the user explicitly accepted a NEW identity
    ///     key. Publishing it is what lets peers see a safety-number change
    ///     instead of an account that silently stopped working.
    ///
    /// The server applies it atomically and drops every other device's bundle,
    /// because they are signed by key material this device does not hold.
    ///
    /// Never call this from a background task: it changes every contact's
    /// safety number, which is a user-visible trust event.
    pub async fn republish_identity_key(
        &mut self,
        bundle: SignalPreKeyBundle,
        one_time_prekeys: Vec<signal_pre_key_bundle::PreKeyRecord>,
        kyber_one_time_prekeys: Vec<signal_pre_key_bundle::KyberPreKeyRecord>,
    ) -> Result<RepublishIdentityKeyResponse, NetworkError> {
        let request = RepublishIdentityKeyRequest {
            bundle: Some(bundle),
            one_time_prekeys,
            kyber_one_time_prekeys,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .republish_identity_key(request)
            .await
            .map(|r| r.into_inner())
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
        self.refresh_pre_keys_with_rotation(new_prekeys, None, None, Vec::new()).await
    }

    /// Upload a batch of classical OPKs and PQOPKs in a single RPC. Per
    /// PQXDH §3.2 a PQOPK is consumed every time a classical OPK is, so
    /// the two pools refill together.
    pub async fn refresh_pre_keys_with_kyber_otpk(
        &mut self,
        new_prekeys: Vec<signal_pre_key_bundle::PreKeyRecord>,
        new_kyber_one_time_prekeys: Vec<signal_pre_key_bundle::KyberPreKeyRecord>,
    ) -> Result<(), NetworkError> {
        self.refresh_pre_keys_with_rotation(new_prekeys, None, None, new_kyber_one_time_prekeys).await
    }

    /// Refresh prekeys with optional signed + Kyber prekey rotation.
    ///
    /// Extends `refresh_pre_keys` to support signed prekey rotation (every 48h)
    /// alongside one-time prekey replenishment. The signed and Kyber prekeys
    /// must be rotated together per PQXDH.
    pub async fn refresh_pre_keys_with_rotation(
        &mut self,
        new_prekeys: Vec<signal_pre_key_bundle::PreKeyRecord>,
        new_signed_prekey: Option<signal_pre_key_bundle::SignedPreKeyRecord>,
        new_kyber_prekey: Option<signal_pre_key_bundle::KyberPreKeyRecord>,
        new_kyber_one_time_prekeys: Vec<signal_pre_key_bundle::KyberPreKeyRecord>,
    ) -> Result<(), NetworkError> {
        let request = RefreshPreKeysRequest {
            new_one_time_prekeys: new_prekeys,
            request_id: uuid::Uuid::new_v4().to_string(),
            new_signed_prekey,
            new_kyber_prekey,
            new_kyber_one_time_prekeys,
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
    /// Certificate is signed by server and used for sealed sender. The server
    /// fixes its lifetime (24h) and says when to refresh (`refresh_after`,
    /// Unix NANOSECONDS). Cache and refresh before expiration.
    ///
    /// The server reads the requesting device id from the authenticated
    /// session, so the request carries nothing. Letting the client supply a
    /// device id was a sender-impersonation primitive (cf. Signal-Server
    /// `CertificateController` which reads `auth.deviceId()`).
    pub async fn request_sender_certificate(
        &mut self,
    ) -> Result<RequestSenderCertificateResponse, NetworkError> {
        let request = RequestSenderCertificateRequest {};

        self.inner
            .request_sender_certificate(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // NOTE: get_sender_certificate() removed for privacy — sealed sender
    // certificates are stateless and never stored server-side. Use
    // request_sender_certificate() and cache the result locally.

    // NOTE: get_server_trust_root() removed - trust root is hardcoded in config.rs
    // per Signal's security model. See crate::config::TRUST_ROOT_PUBLIC_KEY

    // ==================== Account Recovery RPCs ====================

    /// Recovery, first round trip: the code plus the new password's OPAQUE
    /// RegistrationRequest. Unauthenticated: the recovery code is the
    /// credential AND the account identifier (the server looks the account up
    /// by a digest of the code). The code is not spent until
    /// [`Self::use_recovery_code_finish`].
    pub async fn use_recovery_code_start(
        &mut self,
        recovery_code: &str,
        registration_request: &[u8],
    ) -> Result<UseRecoveryCodeStartResponse, NetworkError> {
        let request = UseRecoveryCodeStartRequest {
            recovery_code: recovery_code.to_string(),
            registration_request: registration_request.to_vec(),
        };
        self.inner
            .use_recovery_code_start(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Recovery, second round trip: spends the code and installs the new
    /// RegistrationRecord, SVR key and registration lock. Returns the number
    /// of unused recovery codes left.
    pub async fn use_recovery_code_finish(
        &mut self,
        recovery_id: &str,
        registration_record: &[u8],
        new_svr_public_key: &[u8],
        new_registration_lock: &[u8],
    ) -> Result<UseRecoveryCodeFinishResponse, NetworkError> {
        let request = UseRecoveryCodeFinishRequest {
            recovery_id: recovery_id.to_string(),
            registration_record: registration_record.to_vec(),
            new_svr_public_key: new_svr_public_key.to_vec(),
            new_registration_lock: new_registration_lock.to_vec(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        let response = self
            .inner
            .use_recovery_code_finish(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;
        Ok(response)
    }

    /// Regenerate recovery codes for the authenticated user.
    ///
    /// Requires a fresh password proof. Returns newly generated plaintext codes.
    pub async fn regenerate_recovery_codes(
        &mut self,
        password_proof: PasswordProof,
    ) -> Result<Vec<String>, NetworkError> {
        let request = RegenerateRecoveryCodesRequest {
            password_proof: Some(password_proof),
        };

        let response = self
            .inner
            .regenerate_recovery_codes(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.recovery_codes)
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
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
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
