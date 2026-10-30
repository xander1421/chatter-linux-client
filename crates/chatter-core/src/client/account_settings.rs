use super::*;

impl ChatterClient {
    /// Persisted manual presence mode ("auto" | "online" | "away" | "dnd" |
    /// "invisible"). `None` = never set, treat as "auto".
    pub fn presence_mode(
        &self,
    ) -> Result<Option<String>, crate::services::session_service::SessionError> {
        self.sessions()
            .get_string_setting(crate::storage::repositories::settings_repo::keys::PRESENCE_MODE)
    }

    /// Persist the manual presence mode chosen in the status picker.
    pub fn set_presence_mode(
        &self,
        mode: &str,
    ) -> Result<(), crate::services::session_service::SessionError> {
        self.sessions().set_string_setting(
            crate::storage::repositories::settings_repo::keys::PRESENCE_MODE,
            mode,
        )
    }

    /// Read the sealed-sender RECEIVING policy from the SERVER
    /// (users.unrestricted_unidentified_access). This is a receiving policy:
    /// `false` (default) = contacts-only (only holders of your UAK may reach
    /// you sealed), `true` = anyone may. Sealed *sending* is always on and is
    /// NOT affected by this. The value is also cached locally for offline
    /// display.
    pub async fn get_sealed_sender_policy(
        &self,
    ) -> Result<bool, crate::services::auth_service::AuthError> {
        let allow = self.auth().get_sealed_sender_policy().await?;
        // Best-effort local cache for display; failure to cache is non-fatal.
        let _ = self
            .sessions()
            .set_bool_setting("privacy.sealed_sender_allow_from_anyone", allow);
        Ok(allow)
    }

    /// Set the sealed-sender RECEIVING policy on the SERVER. Unlike a local
    /// bool this changes what the server DOES on inbound sealed messages: with
    /// `false` a non-contact's mismatched UAK is rejected; with `true` it is
    /// accepted. Returns the value the server persisted.
    pub async fn set_sealed_sender_policy(
        &self,
        allow_from_anyone: bool,
    ) -> Result<bool, crate::services::auth_service::AuthError> {
        let persisted = self
            .auth()
            .set_sealed_sender_policy(allow_from_anyone)
            .await?;
        let _ = self
            .sessions()
            .set_bool_setting("privacy.sealed_sender_allow_from_anyone", persisted);
        Ok(persisted)
    }

    /// Read "Who can find me by my username" from the SERVER
    /// (user_profiles.username_discoverable): `true` = Everyone, `false` =
    /// Nobody. The server enforces it in LookupUsername, so it holds on every
    /// device.
    pub async fn get_username_discoverability(
        &self,
    ) -> Result<bool, crate::services::user::UserError> {
        self.users().get_username_discoverability().await
    }

    /// Set "Who can find me by my username" on the SERVER. With `false`,
    /// everyone else's username lookup for this account answers "no match";
    /// friend codes and username links still reach it. Returns the value the
    /// server persisted.
    pub async fn set_username_discoverability(
        &self,
        discoverable: bool,
    ) -> Result<bool, crate::services::user::UserError> {
        self.users()
            .set_username_discoverability(discoverable)
            .await
    }

    /// Session-level current user id (None when logged out).
    pub fn session_current_user_id(&self) -> Option<crate::domain::ids::UserId> {
        self.sessions().current_user_id()
    }

    pub async fn list_sessions(
        &self,
    ) -> Result<
        Vec<crate::services::session_service::Session>,
        crate::services::session_service::SessionError,
    > {
        self.sessions().list_sessions().await
    }

    /// Revoke a session AND mark sender-key rotation for every owned channel,
    /// so the revoked device's cached sender keys cannot read future messages
    /// (rotation happens lazily on next send per channel).
    pub async fn revoke_session(
        &self,
        device_id: u32,
    ) -> Result<(), crate::services::session_service::SessionError> {
        let mut auth_client = self.services.grpc_pool.auth_client();
        self.devices()
            .revoke_and_rotate(&mut auth_client, device_id)
            .await
            .map_err(crate::services::session_service::SessionError::Device)
    }

    /// Sign out every device `Session::can_revoke` allows (all others but
    /// the primary). Ok when all of them went; a device that could not be
    /// revoked makes it `PartialRevocation`.
    pub async fn revoke_all_other_sessions(
        &self,
    ) -> Result<
        crate::services::session_service::RevokeAllOutcome,
        crate::services::session_service::SessionError,
    > {
        use crate::services::session_service::{RevokeAllOutcome, SessionError};
        let owner = self.session_current_user_id();
        let revision = self.auth_state().get().session_revision;
        let sessions = self.list_sessions().await?;
        let primary_kept = sessions.iter().any(|s| !s.is_current && !s.can_revoke);
        let mut revoked = 0;
        let mut failed = Vec::new();
        for session in sessions.into_iter().filter(|s| s.can_revoke) {
            if self.session_current_user_id() != owner
                || self.auth_state().get().session_revision != revision
            {
                return Err(SessionError::SessionChanged);
            }
            match self.revoke_session(session.device_id).await {
                Ok(()) => revoked += 1,
                Err(e) => failed.push((session.device_id, e.to_string())),
            }
        }
        if failed.is_empty() {
            Ok(RevokeAllOutcome {
                revoked,
                primary_kept,
            })
        } else {
            tracing::warn!(?failed, revoked, "revoke all other sessions: some devices failed");
            Err(SessionError::PartialRevocation { revoked, failed })
        }
    }

}
