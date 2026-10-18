use super::*;

impl ChatterClient {
    // ── Profile ─────────────────────────────────────────────────────

    /// Store the user's avatar bytes locally (never uploaded).
    pub fn set_avatar_bytes(
        &self,
        bytes: &[u8],
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.anon_dms()
            .set_my_avatar_bytes(bytes)
            .map_err(|e| crate::services::auth_service::AuthError::InvalidInput(e.to_string()))
    }

    /// Change password.
    pub async fn change_password(
        &self,
        current: &str,
        new: &str,
    ) -> Result<(), crate::services::auth_service::AuthError> {
        self.auth().change_password(current, new).await
    }

    /// Permanently delete the current user's account.
    ///
    /// IRREVERSIBLE. Hits `AuthService/DeleteAccount` on the server (verifying
    /// the supplied password), consumes the deletion event stream to completion,
    /// and on success clears local auth state (auth interceptor + current
    /// user/device ids) so the app drops to a logged-out state. The UI MUST gate
    /// this behind an explicit confirmation.
    ///
    /// Façade wrapper (feedback_service_layer_architecture): chatter-ui reaches
    /// account deletion only through this method, never the raw AccountService.
    /// The service is built on demand from the same components AuthService holds
    /// (grpc pool, settings repo, auth interceptor) — mirroring the on-demand
    /// construction used for `svr_backup()` / `call_signaling()`.
    pub async fn delete_account(
        &self,
        password: &str,
    ) -> Result<(), crate::services::account_service::AccountError> {
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        let revision = self.services.auth_state.get().session_revision;
        #[cfg(feature = "mesh")]
        let former_owner = self.session_current_user_id();
        let auth = self.auth();
        let account = crate::services::account_service::AccountService::new(
            self.services.grpc_pool.clone(),
            auth.settings_repo.clone(),
            auth.auth_interceptor.clone(),
        );
        account.delete_acct(password).await?;
        #[cfg(feature = "mesh")]
        if let Some(owner) = former_owner {
            self.mesh()
                .remove_account_bindings(owner)
                .map_err(crate::services::account_service::AccountError::Storage)?;
        }
        self.clear_composer_session();
        // The account service clears persisted credentials. Retire the old
        // session before publishing logout, just as normal sign-out does.
        self.shutdown_session(revision, false).await;
        self.services.auth_state.set_logged_out_if_current(revision);
        Ok(())
    }

    /// Get a user's display name.
    pub fn get_display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        self.users().get_display_name(user_id)
    }

    /// Signal-style exact username lookup (returns `None` when no account).
    pub async fn lookup_by_username(
        &self,
        username: &str,
    ) -> Result<Option<crate::domain::ids::UserId>, crate::services::user::UserError> {
        // An authenticated lookup ties "who is this account looking for" to
        // our identity — exactly what shield mode exists to hide.
        if self.anon_mode_enabled() {
            return Err(crate::services::user::UserError::HandleLookupForbiddenInAnonMode);
        }
        self.users().lookup_by_username(username).await
    }

    // ==================== Username links (Signal-style, L6) ====================

    /// Create (or rotate) this account's username link; returns the shareable
    /// `chatter://u/{link_id}#{entropy}` URL. Rotation kills old URLs.
    pub async fn create_username_link(&self) -> Result<String, crate::services::user::UserError> {
        self.users().create_username_link().await
    }

    /// The locally stored username-link URL, if one exists on this device.
    pub fn username_link_local(&self) -> Result<Option<String>, crate::services::user::UserError> {
        self.users().username_link_local()
    }

    /// Render a username-link URL as a square PNG QR code.
    pub fn username_link_qr(
        &self,
        url: &str,
        size: u32,
    ) -> Result<Vec<u8>, crate::services::user::UserError> {
        crate::services::user::UserService::username_link_qr(url, size)
    }

    /// Resolve someone's username link to their (decrypted username, account
    /// id). The decryption happens locally with the URL-fragment entropy —
    /// the server never learns the username. The account id is the link
    /// owner's, so a link reaches them even with username discovery off.
    pub async fn resolve_username_link(
        &self,
        url: &str,
    ) -> Result<(String, crate::domain::ids::UserId), crate::services::user::UserError> {
        // An identified server request, like `lookup_by_username`.
        if self.anon_mode_enabled() {
            return Err(crate::services::user::UserError::HandleLookupForbiddenInAnonMode);
        }
        self.users().resolve_username_link(url).await
    }

    /// Delete this account's username link (server + local).
    pub async fn delete_username_link(&self) -> Result<(), crate::services::user::UserError> {
        self.users().delete_username_link().await
    }

    /// Get a user's profile.
    pub async fn get_user_profile(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::Profile, crate::services::user::UserError> {
        self.users().get_user_profile(user_id).await
    }

    /// Refetch a peer's profile when it is stale and, if refetched, emit
    /// `UserProfileUpdated` so every view patches name, bio and avatar. A
    /// peer's profile edit does not rotate their profile key, so nothing else
    /// tells us about it. Call on DM open and profile open (Signal-Desktop's
    /// `throttledGetProfiles`); throttled in `UserService`, so cheap to call.
    /// `conversation` is where the peer is being looked at: see
    /// [`may_refresh_global_profile`].
    pub async fn refresh_profile_if_stale(
        &self,
        user_id: &crate::domain::ids::UserId,
        conversation: &crate::state::ActiveConversation,
    ) {
        if !may_refresh_global_profile(self.anon_mode_enabled(), conversation) {
            return;
        }
        match self.users().refresh_profile_if_stale(user_id).await {
            Ok(Some(profile)) => {
                self.event_bus()
                    .emit_domain(DomainEvent::UserProfileUpdated {
                        user_id: *user_id,
                        display_name: profile.display_name,
                        status_message: profile.status_message.map(|s| s.text),
                        avatar_bytes: profile.avatar_bytes,
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                    });
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(peer_id = %user_id, error = %e, "profile refresh failed"),
        }
    }

    /// Refetch OUR profile (throttled) and restore the local copies the UI
    /// reads — name, bio, avatar — decrypted with our own profile key
    /// (`UserService::restore_own_profile`). Only login did this, so a LINKED
    /// device, or one signed in before that fix, showed initials and a
    /// fallback name while every peer saw the photo. `Some` when refetched.
    /// Not in Shield: it is an identified fetch.
    pub async fn refresh_own_profile(&self) -> Option<crate::domain::user::Profile> {
        if self.anon_mode_enabled() {
            return None;
        }
        let me = self.session_current_user_id()?;
        let revision = self.auth_state().get().session_revision;
        match self.users().refresh_profile_if_stale(&me).await {
            Ok(Some(profile)) => {
                self.auth_state().update_display_name_if_current(
                    revision,
                    me,
                    profile.display_name.clone(),
                );
                Some(profile)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "own profile refresh failed");
                None
            }
        }
    }

    /// Search for users by exact `nickname.discriminator` username.
    ///
    /// Backs the friends-modal user search (F-UI-3). Because display names are
    /// profile-key-encrypted server-side, only an EXACT username resolves to an
    /// account — this mirrors the invite-member search (Signal's discovery
    /// model). Returns at most one hit (the exact match), enriched with the
    /// decrypted display name and whether they are already a friend. An empty
    /// or no-match query yields an empty vec (never an error the UI must show).
    pub async fn search_users(
        &self,
        query: &str,
    ) -> Result<Vec<UserSearchResult>, crate::services::user::UserError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(vec![]);
        }

        let user_id = match self.lookup_by_username(trimmed).await? {
            Some(id) => id,
            None => return Ok(vec![]),
        };

        // Don't surface ourselves as a search hit.
        if self.session_current_user_id().as_ref() == Some(&user_id) {
            return Ok(vec![]);
        }

        // Best-effort profile fetch: a stranger found by exact username has no
        // profile key exchanged yet, so the display name may not decrypt. Still
        // surface the hit (fall back to the username) rather than dropping it.
        let display_name = match self.get_user_profile(&user_id).await {
            Ok(p) if !p.display_name.is_empty() => p.display_name,
            _ => trimmed.to_string(),
        };
        let username = self
            .known_username(&user_id)
            .unwrap_or_else(|| trimmed.to_string());

        // is_friend: the peer id appears in any of our friendship rows.
        let friend_ids: std::collections::HashSet<crate::domain::ids::UserId> = self
            .list_friends(500, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .flat_map(|f| [f.requester_id, f.addressee_id])
            .collect();

        Ok(vec![UserSearchResult {
            is_friend: friend_ids.contains(&user_id),
            user_id,
            display_name,
            username,
        }])
    }

    // ── Message history ─────────────────────────────────────────────

    /// Get recent messages for any conversation type.
    ///
    /// Returns messages in chronological order (oldest first).
    pub fn get_messages(
        &self,
        conversation: &ActiveConversation,
        limit: u32,
    ) -> Result<Vec<Message>, crate::services::message::MessageError> {
        match conversation {
            ActiveConversation::Channel(id) => {
                // Resolve from the channel row (`Channel`, `ServerDm` or
                // `SavedMessages`); the send path stores under the same value.
                let conv_type = crate::services::message::conversion::conversation_type_for_channel(
                    &self.services.channel_repo,
                    id,
                );
                self.messages()
                    .get_messages_local(id, &conv_type, None, limit)
            }
            ActiveConversation::ServerDm { peer_id } => {
                let id: crate::domain::ids::ChannelId = (*peer_id).into();
                self.messages()
                    .get_messages_local(&id, &ConversationType::ServerDm, None, limit)
            }
            ActiveConversation::AnonDm(id) => {
                // Anon DM messages live in the anon_dm service's own store;
                // delegate so bridge consumers (Android, etc.) don't have to
                // branch on conversation type.
                self.anon_dms()
                    .get_messages(id, None, limit as i32)
                    .map_err(|e| {
                        crate::services::message::MessageError::Storage(
                            crate::error::StorageError::InvalidData(e.to_string()),
                        )
                    })
            }
            ActiveConversation::AnonGroup(id) => self
                .anon_groups()
                .get_messages(id, None, limit as i32)
                .map_err(|e| {
                    crate::services::message::MessageError::Storage(
                        crate::error::StorageError::InvalidData(e.to_string()),
                    )
                }),
            _ => Ok(vec![]),
        }
    }

    /// Get the current user's display name from settings (same one used for
    /// outgoing messages / calls). Falls back to an empty option.
    pub fn my_display_name(&self) -> Option<String> {
        self.anon_dms().get_my_display_name()
    }

}
