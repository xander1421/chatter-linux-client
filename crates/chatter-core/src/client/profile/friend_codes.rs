use super::*;

impl ChatterClient {
    /// Return the code represented by the account's current stored identity.
    /// This reads existing key material and does not change the delivery token.
    pub fn current_friend_code(
        &self,
        private_mode: bool,
    ) -> Result<String, crate::services::UserError> {
        if private_mode {
            if !self.anon_mode_enabled() {
                return Err(crate::services::UserError::RequiresAnonMode);
            }
            self.users().current_private_friend_code()
        } else {
            self.users().current_friend_code()
        }
    }

    pub fn generate_friend_code(&self) -> Result<String, crate::services::UserError> {
        self.current_friend_code(false)
    }

    pub fn generate_private_friend_code(&self) -> Result<String, crate::services::UserError> {
        self.current_friend_code(true)
    }

    pub fn generate_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
        self.users().generate_friend_qr(size)
    }

    pub fn generate_private_friend_qr(
        &self,
        size: u32,
    ) -> Result<Vec<u8>, crate::services::UserError> {
        if !self.anon_mode_enabled() {
            return Err(crate::services::UserError::RequiresAnonMode);
        }
        self.users().generate_private_friend_qr(size)
    }

    pub async fn connect_via_friend_code(
        self: &Arc<Self>,
        code: &str,
        message: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::UserError> {
        // A regular friend code ends in an identified friend request, which
        // Shield mode must refuse.
        if self.anon_mode_enabled() {
            return Err(crate::services::UserError::ForbiddenInAnonMode);
        }
        let result = self.users().connect_via_friend_code(code, message).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });
        Ok(result)
    }

    /// Redeem either kind of friend code without relying on the current UI mode.
    pub async fn connect_friend_code(
        self: &Arc<Self>,
        code: &str,
        message: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> Result<FriendCodeConnection, FriendCodeConnectionError> {
        let code = code.trim();
        if crate::domain::PrivateFriendCode::is_private_code(code) {
            let private = crate::domain::PrivateFriendCode::parse(code)
                .map_err(|_| FriendCodeConnectionError::InvalidCode)?;
            if !self.secret_chats_available() {
                return Err(FriendCodeConnectionError::SecretChatUnavailable);
            }
            return match self
                .start_anon_dm_conversation_private(
                    &private,
                    Some(message.unwrap_or("Hello!")),
                    sender_display_name,
                )
                .await
            {
                Ok(dm) => Ok(FriendCodeConnection::SecretChat(dm)),
                Err(crate::services::anon_dm::AnonDmError::ConversationExists(peer)) => {
                    // The service excludes declined chats and replaces them on retry.
                    // Return the live chat's ID so the UI can open its existing row.
                    let existing = self.anon_dms().get_live_conversation_by_peer(&peer)?;
                    match existing {
                        Some(dm) => Err(FriendCodeConnectionError::ExistingSecretChat(dm.id)),
                        None => Err(FriendCodeConnectionError::SecretChat(
                            crate::services::anon_dm::AnonDmError::ConversationExists(peer),
                        )),
                    }
                }
                Err(error) => Err(error.into()),
            };
        }

        if !crate::domain::UsernameLink::is_username_link(code) {
            let candidate = code.strip_prefix('@').unwrap_or(code);
            if !candidate.is_empty()
                && candidate.len() <= 64
                && candidate
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
            {
                return Err(FriendCodeConnectionError::UsernameInsteadOfCode);
            }
            crate::services::user::UserService::parse_friend_code(code)
                .map_err(|_| FriendCodeConnectionError::InvalidCode)?;
        }
        if self.anon_mode_enabled() {
            return Err(FriendCodeConnectionError::ServerCodeInShield);
        }
        self.connect_via_friend_code(code, message)
            .await
            .map(FriendCodeConnection::FriendRequest)
            .map_err(Into::into)
    }

    pub async fn list_friend_requests(
        &self,
        filter: crate::services::user::FriendRequestFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users()
            .list_friend_requests(filter, limit, offset)
            .await
    }

    /// Convenience: incoming-only friend requests.
    pub async fn list_pending_friend_requests(
        &self,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users()
            .list_friend_requests(crate::services::user::FriendRequestFilter::Received, 100, 0)
            .await
    }

}
