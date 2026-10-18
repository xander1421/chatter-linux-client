use super::*;

impl AuthService {
    /// Create a new AuthService with all dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        identity_manager: Arc<IdentityKeyManager>,
        prekey_manager: Arc<PreKeyManager>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
        key_store: Arc<KeyStore>,
        auth_interceptor: AuthInterceptor,
    ) -> Self {
        Self {
            grpc_pool,
            identity_manager,
            prekey_manager,
            user_repo,
            settings_repo,
            key_store,
            auth_interceptor,
            recovery_login_grant: parking_lot::Mutex::new(None),
        }
    }

    // ========================================================================
    // Connection management (delegates to grpc_pool)
    // ========================================================================

    /// Get the current server endpoint URL.
    pub fn endpoint(&self) -> String {
        self.grpc_pool.endpoint()
    }

    /// Update the server URL. Disconnects any existing connection.
    pub fn set_server_url(&self, url: &str) -> Result<(), NetworkError> {
        self.grpc_pool.set_url(url)
    }

    /// Check if connected to the server.
    pub fn is_connected(&self) -> bool {
        self.grpc_pool.is_connected()
    }

    /// Connect to the server.
    pub async fn connect(&self) -> Result<(), NetworkError> {
        self.grpc_pool.connect().await
    }

    /// Disconnect from the server.
    pub fn disconnect(&self) {
        self.grpc_pool.disconnect();
    }

    /// Create an auth client for making gRPC calls.
    /// Returns None if not connected to server.
    pub fn try_auth_client(&self) -> Option<AuthClient> {
        self.grpc_pool
            .try_channel()
            .map(|channel| AuthClient::new(channel, self.grpc_pool.auth_clone()))
    }

    /// Check if a user is currently logged in.
    pub fn is_logged_in(&self) -> bool {
        if self
            .settings_repo
            .get_bytes(keys::DEVICE_AUTH_TOKEN)
            .ok()
            .flatten()
            .is_none()
        {
            return false;
        }
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .is_some()
    }

    /// Check if a user has ever registered on this device.
    ///
    /// Returns true if there's any user in the local database,
    /// indicating the user should see login screen instead of register.
    pub fn has_registered_before(&self) -> bool {
        // Check if there are any users stored locally
        match self.user_repo.count() {
            Ok(count) => count > 0,
            Err(_) => false,
        }
    }

    /// Get the current user ID and device ID if logged in.
    pub fn current_user(&self) -> Option<(UserId, u32)> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()??;
        let device_id: u32 = self.settings_repo.get(keys::CURRENT_DEVICE_ID).ok()??;

        let user_id = user_id_str.parse().ok()?;
        Some((user_id, device_id))
    }

    /// Restore the authentication session from saved credentials.
    ///
    /// Loads password-based (Basic auth) credentials from encrypted settings
    /// and configures the auth interceptor. This must be called on application
    /// startup.
    ///
    /// Returns true if a session was successfully restored.
    pub fn restore_session(&self) -> bool {
        self.restore_password_session()
    }

    /// Restore a password-based (Basic auth) session.
    fn restore_password_session(&self) -> bool {
        // Every miss below is logged. A silent `return false` here presents as
        // "registration worked but the app is logged out on next launch", with
        // nothing in the log to say which of the four keys was absent.
        macro_rules! need {
            ($key:expr, $get:expr) => {
                match $get {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        tracing::warn!("No saved session: {} is not set", $key);
                        return false;
                    }
                    Err(e) => {
                        tracing::warn!("No saved session: reading {} failed: {e}", $key);
                        return false;
                    }
                }
            };
        }

        let user_id_str = need!(
            keys::CURRENT_USER_ID,
            self.settings_repo.get_string(keys::CURRENT_USER_ID)
        );
        let device_id: u32 = need!(
            keys::CURRENT_DEVICE_ID,
            self.settings_repo.get(keys::CURRENT_DEVICE_ID)
        );
        let profile_key_bytes = need!(
            keys::PROFILE_KEY,
            self.settings_repo.get_bytes(keys::PROFILE_KEY)
        );
        let auth_token_bytes = need!(
            keys::DEVICE_AUTH_TOKEN,
            self.settings_repo.get_bytes(keys::DEVICE_AUTH_TOKEN)
        );

        if profile_key_bytes.len() != 32 || auth_token_bytes.len() != 32 {
            tracing::warn!("Saved auth credentials have invalid length");
            return false;
        }

        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&profile_key_bytes);

        let mut auth_token_arr = [0u8; 32];
        auth_token_arr.copy_from_slice(&auth_token_bytes);

        // Reconstruct credentials
        let uak = crate::crypto::device_credentials::derive_uak(&profile_key);
        let credentials = crate::crypto::device_credentials::DeviceCredentials::from_stored(
            device_id,
            auth_token_arr,
            uak,
        );

        // Configure auth interceptor
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id_str),
            credentials.auth_token_base64(),
        );

        tracing::info!(
            user_id = %user_id_str,
            device_id = %device_id,
            "Restored password authentication session"
        );

        true
    }

    /// Get current identity from the server.
    pub async fn get_my_identity(
        &self,
    ) -> Result<crate::proto::chat_v1::GetMyIdentityResponse, AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client.get_my_identity().await.map_err(AuthError::Network)
    }

    /// Read the sealed-sender RECEIVING policy (allow_from_anyone) from the server.
    pub async fn get_sealed_sender_policy(&self) -> Result<bool, AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client
            .get_sealed_sender_policy()
            .await
            .map(|r| r.allow_from_anyone)
            .map_err(AuthError::Network)
    }

    /// Set the sealed-sender RECEIVING policy on the server. Returns the value
    /// the server persisted (echoed back).
    /// Point the server's UAK at `profile_key` after a rotation.
    pub async fn set_unidentified_access_key(
        &self,
        profile_key: &[u8; 32],
    ) -> Result<(), AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client
            .set_unidentified_access_key(&crate::crypto::derive_uak(profile_key))
            .await
            .map_err(AuthError::Network)
    }

    pub async fn set_sealed_sender_policy(
        &self,
        allow_from_anyone: bool,
    ) -> Result<bool, AuthError> {
        let mut client = self.try_auth_client().ok_or(AuthError::NotLoggedIn)?;
        client
            .set_sealed_sender_policy(allow_from_anyone)
            .await
            .map(|r| r.allow_from_anyone)
            .map_err(AuthError::Network)
    }
}
