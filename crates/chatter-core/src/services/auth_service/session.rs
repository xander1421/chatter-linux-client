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

    /// Retire this device before clearing local credentials.
    pub async fn logout(&self, auth_client: &mut AuthClient) -> Result<(), AuthError> {
        if !self.is_logged_in() {
            return Err(AuthError::NotLoggedIn);
        }

        let owner = self
            .settings_repo
            .current_user_id()
            .map_err(|error| match error {
                crate::services::current_user::CurrentUserError::Storage(error) => {
                    AuthError::Storage(error)
                }
                crate::services::current_user::CurrentUserError::NotLoggedIn => {
                    AuthError::NotLoggedIn
                }
            })?;
        super::cache_saved_messages_channel(&self.settings_repo, owner)?;

        let device_id = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)?
            .ok_or(AuthError::NotLoggedIn)?;
        auth_client.logout(device_id, None).await?;
        self.finish_logout()
    }

    pub(super) fn finish_logout(&self) -> Result<(), AuthError> {
        self.end_session()?;
        self.settings_repo.delete(keys::CURRENT_USER_ID)?;
        self.settings_repo.delete(keys::CURRENT_DEVICE_ID)?;
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

    pub async fn do_logout(&self) -> Result<(), AuthError> {
        let mut auth_client = self.try_auth_client().ok_or(NetworkError::Unavailable)?;
        self.logout(&mut auth_client).await
    }
}
