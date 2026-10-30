use super::*;

impl AuthService {
    /// End authorization without retiring this device or touching its local data.
    pub(crate) fn end_session(&self) -> Result<(), AuthError> {
        self.auth_interceptor.clear();
        self.settings_repo.delete(keys::DEVICE_AUTH_TOKEN)?;
        self.settings_repo.delete(keys::PUSH_TOPIC)?;
        self.settings_repo.delete(keys::DEVICE_WAKE_TOPIC)?;
        // PIN authorization must be re-derived from user input as well.
        self.settings_repo.delete(keys::PIN_FINGERPRINT)?;
        Ok(())
    }

    /// Logout the current user.
    ///
    /// Orchestrates:
    /// 1. Server logout notification
    /// 2. Auth interceptor clearing
    /// 3. Local auth state clearing (keeps identity keys for re-login)
    pub async fn logout(&self, auth_client: &mut AuthClient) -> Result<(), AuthError> {
        // Check is logged in
        if !self.is_logged_in() {
            return Err(AuthError::NotLoggedIn);
        }

        // Get current device ID
        let device_id: u32 = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)?
            .unwrap_or(0);

        // Notify server of logout (ignore errors - we logout locally regardless)
        let _ = auth_client.logout(device_id).await;

        // Clear auth interceptor state
        self.auth_interceptor.clear();

        // Clear auth state from settings (but keep identity keys)
        self.settings_repo.delete(keys::CURRENT_USER_ID)?;
        self.settings_repo.delete(keys::CURRENT_DEVICE_ID)?;
        self.settings_repo.delete(keys::DEVICE_AUTH_TOKEN)?;
        self.settings_repo.delete(keys::PUSH_TOPIC)?;
        self.settings_repo.delete(keys::DEVICE_WAKE_TOPIC)?;

        // Clear the local PIN fingerprint — re-derived on next login from
        // user-supplied PIN. Leaving stale fingerprints behind would let a
        // linked-device approve flow run after logout if the modal stayed
        // open in another window.
        let _ = self.settings_repo.delete(keys::PIN_FINGERPRINT);

        Ok(())
    }

    // ==================== Simplified Wrappers ====================

    /// Simplified wrapper for login that creates the client internally.
    pub async fn do_login(
        &self,
        username: &str,
        password: &str,
        device_name: &str,
        pin: Option<&str>,
        on_verifying: &(dyn Fn() + Send + Sync),
    ) -> Result<LoginResult, AuthError> {
        let mut auth_client = self.try_auth_client().ok_or_else(|| {
            AuthError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".to_string(),
            ))
        })?;
        self.login(
            &mut auth_client,
            username,
            password,
            device_name,
            pin,
            on_verifying,
        )
        .await
    }

    /// Simplified wrapper for register that creates the client internally.
    ///
    /// # Arguments
    ///
    /// * `avatar` - Optional avatar image bytes (stored locally, encrypted when shared)
    pub async fn do_register(
        &self,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, AuthError> {
        let mut auth_client = self
            .try_auth_client()
            .ok_or_else(|| AuthError::RegistrationFailed("Not connected to server".to_string()))?;
        self.register(
            &mut auth_client,
            username,
            password,
            display_name,
            device_name,
            avatar,
            pin,
        )
        .await
    }

    /// Simplified wrapper for logout that creates the client internally.
    pub async fn do_logout(&self) -> Result<(), AuthError> {
        let mut auth_client = self.try_auth_client().ok_or_else(|| {
            AuthError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".to_string(),
            ))
        })?;
        self.logout(&mut auth_client).await
    }
}
