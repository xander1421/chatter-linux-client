use super::*;

impl UserService {
    // ========================================================================
    // Lookup Operations
    // ========================================================================

    /// Signal-style username lookup.
    ///
    /// `username` must be the full `nickname.discriminator` form (e.g.
    /// `alice.4711`); plain nicknames are rejected by libsignal's
    /// `Username::new`. Returns `Ok(None)` when no account is registered
    /// under the name.
    pub async fn lookup_by_username(
        &self,
        username: &str,
    ) -> Result<Option<crate::domain::UserId>, UserError> {
        // The lookup is an existence oracle. Only a complete handle may reach
        // the server — a caller that fires per keystroke must not turn it into
        // a prefix/enumeration probe.
        if !is_lookup_handle(username) {
            return Err(UserError::InvalidInput(
                "not a nickname.discriminator handle".into(),
            ));
        }
        let _ = self.settings_repo.current_user_id()?;
        let hash = crate::crypto::auth_crypto::hash_username_lookup(username)
            .map_err(|e| UserError::InvalidInput(format!("invalid username: {e}")))?;
        let mut user_client = self.user_client();
        match user_client.lookup_username(hash).await? {
            Some(id_str) => {
                let user_id = id_str.parse().ok();
                if let Some(id) = user_id.as_ref() {
                    if let Err(e) = self.user_repo.set_known_username(id, username) {
                        tracing::warn!(error = %e, "Could not remember looked-up peer handle");
                    }
                }
                Ok(user_id)
            }
            None => Ok(None),
        }
    }

    /// Read this account's "Who can find me by my username" setting from the
    /// server. `true` = Everyone, `false` = Nobody.
    pub async fn get_username_discoverability(&self) -> Result<bool, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let mut user_client = self.user_client();
        Ok(user_client.get_username_discoverability().await?)
    }

    /// Set this account's "Who can find me by my username" setting on the
    /// server. With `false` the server answers "no match" to everyone else's
    /// username lookup for this account; friend codes and username links
    /// still reach it. Returns the value the server persisted.
    pub async fn set_username_discoverability(
        &self,
        discoverable: bool,
    ) -> Result<bool, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let mut user_client = self.user_client();
        Ok(user_client
            .set_username_discoverability(discoverable)
            .await?)
    }
}
