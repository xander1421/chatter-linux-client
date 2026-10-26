use super::*;

impl ChatterClient {
    /// Update the current user's profile and push it to every peer who can't
    /// fetch it from the server: each ready anon DM, and the anon groups this
    /// user owns. The push runs in the background — a slow or failing peer
    /// never holds up (or fails) the save itself.
    pub async fn update_user_profile(
        self: &std::sync::Arc<Self>,
        display_name: Option<String>,
        status_message: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        let auth = self.auth_state().get();
        let user = auth
            .current_user_id
            .ok_or(crate::services::UserError::NotLoggedIn)?;
        let alive = std::sync::atomic::AtomicBool::new(true);
        let profile = self
            .with_profile_owner(user, auth.session_revision, &alive, || async {
                let profile = self
                    .bounded_profile_operation(
                        user,
                        auth.session_revision,
                        &alive,
                        std::time::Duration::from_secs(30),
                        self.users()
                            .update_user_profile(display_name, status_message),
                    )
                    .await?;
                self.profile_response_in_session(profile, user, auth.session_revision, false)
            })
            .await?;
        self.propagate_profile_in_session(user, auth.session_revision, &profile);
        Ok(profile)
    }

    /// Save one admitted own-profile edit. Serialize storage/RPC and propagation
    /// against login/logout, not merely its eventual UI repaint.
    pub async fn save_own_profile_in_session(
        self: &Arc<Self>,
        user: crate::domain::ids::UserId,
        revision: u64,
        edit_alive: Arc<std::sync::atomic::AtomicBool>,
        avatar: Option<Vec<u8>>,
        display_name: String,
        status: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        use crate::services::UserError;
        use crate::storage::repositories::settings_repo::keys;
        let profile = self
            .with_profile_owner(user, revision, &edit_alive, || async {
                // The encrypted profile blob adds a 12-byte nonce and 16-byte tag.
                if avatar.as_ref().is_some_and(|b| {
                    b.is_empty()
                        || b.len()
                            .checked_add(28)
                            .is_none_or(|n| n > crate::domain::anon_dm_init::MAX_AVATAR_SIZE)
                }) {
                    return Err(UserError::InvalidInput(
                        "that picture is too large.".into(),
                    ));
                }
                let repositories = self.messages();
                let settings = &repositories.settings_repo;
                let previous = if avatar.is_some() {
                    settings.get_bytes(keys::AVATAR)?
                } else {
                    None
                };
                if let Some(bytes) = &avatar {
                    self.set_avatar_bytes(bytes)
                        .map_err(|e| UserError::InvalidInput(e.to_string()))?;
                }
                let result = self
                    .bounded_profile_operation(
                        user,
                        revision,
                        &edit_alive,
                        std::time::Duration::from_secs(30),
                        self.users().update_user_profile(Some(display_name), status),
                    )
                    .await
                    .and_then(|profile| {
                        self.profile_response_in_session(profile, user, revision, avatar.is_some())
                    });
                if result.is_err() && avatar.is_some() && self.profile_owner_current(user, revision)
                {
                    // Restore only this owner's previous local selection. A cancelled
                    // already-submitted RPC may still commit remotely; this isn't a
                    // distributed transaction, and no success/propagation is claimed.
                    match previous {
                        Some(bytes) => settings.set_bytes(keys::AVATAR, &bytes)?,
                        None => {
                            settings.delete(keys::AVATAR)?;
                        }
                    }
                }
                result
            })
            .await?;
        self.auth_state().update_display_name_if_current(
            revision,
            user,
            profile.display_name.clone(),
        );
        self.propagate_profile_in_session(user, revision, &profile);
        Ok(profile)
    }

    pub(super) fn profile_response_in_session(
        &self,
        profile: crate::domain::user::Profile,
        user: crate::domain::ids::UserId,
        revision: u64,
        avatar_expected: bool,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        if !self.profile_owner_current(user, revision)
            || profile.user_id != user
            || (avatar_expected && profile.avatar_bytes.is_none())
        {
            return Err(crate::services::UserError::InvalidInput(
                "Profile response was not confirmed for the originating account/session".into(),
            ));
        }
        Ok(profile)
    }

    fn profile_owner_current(&self, user: crate::domain::ids::UserId, revision: u64) -> bool {
        let auth = self.auth_state().get();
        auth.current_user_id == Some(user)
            && auth.session_revision == revision
            && self.messages().settings_repo.current_user_id().ok() == Some(user)
    }

    pub(super) async fn with_profile_owner<T, F, Fut>(
        &self,
        user: crate::domain::ids::UserId,
        revision: u64,
        alive: &std::sync::atomic::AtomicBool,
        operation: F,
    ) -> Result<T, crate::services::UserError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, crate::services::UserError>>,
    {
        let _lifecycle = tokio::select! {
            biased;
            _ = self.profile_operation_cancelled(user, revision, alive) => {
                return Err(crate::services::UserError::InvalidInput("Profile edit session expired".into()));
            }
            guard = tokio::time::timeout(std::time::Duration::from_secs(30), self.auth_lifecycle_lock.lock()) => {
                guard.map_err(|_| crate::services::UserError::InvalidInput("Profile update waiting for account operation timed out".into()))?
            }
        };
        if !alive.load(std::sync::atomic::Ordering::SeqCst)
            || !self.profile_owner_current(user, revision)
        {
            return Err(crate::services::UserError::InvalidInput(
                "Profile edit session expired".into(),
            ));
        }
        operation().await
    }

    pub(super) async fn bounded_profile_operation<T>(
        &self,
        user: crate::domain::ids::UserId,
        revision: u64,
        alive: &std::sync::atomic::AtomicBool,
        budget: std::time::Duration,
        operation: impl std::future::Future<Output = Result<T, crate::services::UserError>>,
    ) -> Result<T, crate::services::UserError> {
        tokio::select! {
            biased;
            _ = self.profile_operation_cancelled(user, revision, alive) => Err(crate::services::UserError::InvalidInput("Profile edit cancelled; submitted update may still reach the server".into())),
            result = tokio::time::timeout(budget, operation) => result.unwrap_or_else(|_| Err(
                crate::services::UserError::InvalidInput("Profile update timed out; submitted update may still reach the server".into()))),
        }
    }

    async fn profile_operation_cancelled(
        &self,
        user: crate::domain::ids::UserId,
        revision: u64,
        alive: &std::sync::atomic::AtomicBool,
    ) {
        loop {
            if !alive.load(std::sync::atomic::Ordering::SeqCst)
                || !self.profile_owner_current(user, revision)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    pub(super) async fn with_profile_snapshot<T, F, Fut>(
        &self,
        user: crate::domain::ids::UserId,
        revision: u64,
        alive: &std::sync::atomic::AtomicBool,
        profile: &crate::domain::user::Profile,
        operation: F,
    ) -> Result<T, crate::services::UserError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, crate::services::UserError>>,
    {
        self.with_profile_owner(user, revision, alive, || async {
            let current = self.get_profile_local(&user);
            if current.as_ref().is_none_or(|p| {
                p.display_name != profile.display_name
                    || p.status_message != profile.status_message
                    || p.avatar_bytes != profile.avatar_bytes
            }) {
                return Err(crate::services::UserError::InvalidInput(
                    "Profile propagation superseded by a newer edit".into(),
                ));
            }
            operation().await
        })
        .await
    }

    fn propagate_profile_in_session(
        self: &Arc<Self>,
        user: crate::domain::ids::UserId,
        revision: u64,
        profile: &crate::domain::user::Profile,
    ) {
        let client = self.clone();
        let profile = profile.clone();
        let display_name = profile.display_name.clone();
        let bio = profile.status_message.as_ref().map(|m| m.text.clone());
        tokio::spawn(async move {
            let alive = std::sync::atomic::AtomicBool::new(true);
            // Capture/re-read conversations only after acquiring the auth lock.
            // One bounded job prevents queued propagation adopting new credentials.
            let result = client.with_profile_snapshot(user, revision, &alive, &profile, || async {
                client.bounded_profile_operation(user, revision, &alive, std::time::Duration::from_secs(30), async {
                    for conv in client.list_anon_dm_conversations().into_iter().filter(|c| c.is_ready()) {
                        if let Err(error) = client.send_anon_dm_profile_update(&conv.id,
                            Some(display_name.clone()), None, bio.clone()).await {
                            tracing::warn!(%error, "session-owned anonymous profile update failed");
                        }
                    }
                    client.broadcast_profile_to_owned_groups().await
                        .map_err(|e| crate::services::UserError::InvalidInput(e.to_string()))
                }).await
            }).await;
            if let Err(error) = result {
                tracing::warn!(%error, "profile propagation retired or failed");
            }
        });
    }

}
