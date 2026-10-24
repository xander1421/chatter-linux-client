use super::*;

impl UserService {
    // ========================================================================
    // Local Query Methods
    // ========================================================================

    /// Get a user from local storage only.
    pub fn get_user_local(&self, user_id: &UserId) -> Result<Option<User>, UserError> {
        self.user_repo.get(user_id).map_err(UserError::Storage)
    }

    /// Get a user's profile from local storage only.
    /// Whether `user_id` is an accepted friend of the current user, from the
    /// local friendships table (either direction of the original request).
    pub fn is_friend_local(&self, user_id: &UserId) -> bool {
        use crate::domain::user::FriendshipStatus;
        let Ok(me) = self.settings_repo.current_user_id() else {
            return false;
        };
        [(me, *user_id), (*user_id, me)].iter().any(|(a, b)| {
            matches!(
                self.user_repo.get_friendship(a, b),
                Ok(Some(f)) if f.status == FriendshipStatus::Accepted
            )
        })
    }

    /// Server-side blocks this user made, from the durable friendships table
    /// (kept in step with the server by `UserSync::sync_blocked_users`).
    pub fn blocked_local(&self) -> Vec<UserId> {
        let Ok(me) = self.settings_repo.current_user_id() else {
            return Vec::new();
        };
        self.user_repo.list_blocked_by(&me).unwrap_or_default()
    }

    /// Whether this user blocked `user_id` server-side (durable table).
    pub fn is_blocked_local(&self, user_id: &UserId) -> bool {
        let Ok(me) = self.settings_repo.current_user_id() else {
            return false;
        };
        self.user_repo.is_blocked(&me, user_id).unwrap_or(false)
    }

    pub fn get_profile_local(&self, user_id: &UserId) -> Result<Option<Profile>, UserError> {
        self.user_repo
            .get_profile(user_id)
            .map_err(UserError::Storage)
    }

    /// Get a user's display name from local storage.
    ///
    /// Resolution order:
    /// 1. User directory profile (server DMs, channel members, contacts)
    /// 2. User directory username (any registered peer)
    /// 3. Anon DM peer (peer isn't in the user directory — stored on the
    ///    anon DM conversation row)
    /// 4. Anon group member (stored on the group's member row in any group)
    /// 5. The `User <id8>` placeholder every other screen shows
    pub fn get_display_name(&self, user_id: &UserId) -> String {
        self.get_display_names(std::slice::from_ref(user_id))
            .remove(user_id)
            .unwrap_or_else(|| friend_display_name_placeholder(user_id))
    }

    /// [`Self::get_display_name`] for many users at once: the profile name and
    /// the username of every id come from one reader query
    /// (`UserRepository::display_name_sources`); only an id that neither
    /// resolves walks the per-id anon fallbacks. Opening a conversation names
    /// all its senders through this.
    pub fn get_display_names(
        &self,
        user_ids: &[UserId],
    ) -> std::collections::HashMap<UserId, String> {
        let owner = self.settings_repo.current_user_id().ok();
        let sources = self
            .user_repo
            .display_name_sources(owner.as_ref(), user_ids)
            .unwrap_or_default();
        user_ids
            .iter()
            .map(|user_id| {
                let (contact_name, profile_name, username) = sources.get(user_id).cloned().unwrap_or_default();
                (
                    *user_id,
                    contact_name.filter(|name| !name.is_empty()).unwrap_or_else(|| self.resolve_display_name(user_id, profile_name, username)),
                )
            })
            .collect()
    }

    fn resolve_display_name(
        &self,
        user_id: &UserId,
        profile_name: Option<String>,
        username: Option<String>,
    ) -> String {
        // A profile row written while we had no profile key holds the
        // placeholder itself; that is not a name, so keep resolving (a known
        // username beats `User 1c242673` in Contacts).
        let placeholder = friend_display_name_placeholder(user_id);
        if let Some(name) = profile_name {
            if !name.is_empty() && name != placeholder {
                return name;
            }
        }
        if let Some(username) = username {
            // `ensure_user_exists` stores the id itself as a placeholder
            // username; that is not a name, so keep resolving.
            if !username.is_empty() && username != user_id.to_string() {
                return username;
            }
        }
        if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_live_conversation_by_peer(user_id) {
            if let Some(name) = anon_dm.peer_display_name {
                if !name.is_empty() {
                    return name;
                }
            }
        }
        // A blocked anon peer whose chat was deleted: the block row keeps
        // the name the conversation row carried.
        if let Ok(Some(name)) = self.anon_dm_repo.get_blocked_display_name(user_id) {
            if !name.is_empty() {
                return name;
            }
        }
        if let Ok(Some(name)) = self.anon_group_repo.find_display_name_for_user(user_id) {
            if !name.is_empty() {
                return name;
            }
        }
        placeholder
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, UserError> {
        self.settings_repo
            .current_user_id()
            .map_err(UserError::from)
    }
}
