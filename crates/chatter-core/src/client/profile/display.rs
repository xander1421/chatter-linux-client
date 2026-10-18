use super::*;

impl ChatterClient {
    // ── Display / profile façade ─────────────────────────────────────────

    /// Resolve a user's display name (checks profile → username → anon DM
    /// peer → anon group member → UUID fallback).
    pub fn display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        self.users().get_display_name(user_id)
    }

    /// Apply the event before rendering: the core state subscriber and UI
    /// subscriber run independently, so the UI must not race the former.
    pub fn update_typing_indicator(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        user_id: crate::domain::ids::UserId,
        timestamp: u64,
        started: bool,
    ) -> bool {
        let state = self.chat_state();
        if !state.accept_typing(channel_id, user_id, timestamp, started) {
            return false;
        }
        let mut users = state.get_typing_users(&channel_id);
        users.retain(|id| *id != user_id);
        if started {
            users.push(user_id);
        }
        state.set_typing_users(channel_id, users);
        true
    }

    /// Named typing text, with anonymous-group names resolved in their group.
    /// The UI's existing inactivity timer clears this conversation's aggregate.
    pub fn typing_indicator_text(&self, channel_id: crate::domain::ids::ChannelId) -> String {
        let group_id = crate::domain::anon_group::AnonGroupId::from_uuid(*channel_id.as_uuid());
        let kind = if self.get_anon_group(&group_id).is_some() {
            ConversationType::AnonGroup
        } else {
            self.conversation_type_of(&ActiveConversation::Channel(channel_id))
                .unwrap_or(ConversationType::Channel)
        };
        let me = self.auth_state().get().current_user_id;
        let mut users = self.chat_state().get_typing_users(&channel_id);
        users.retain(|id| Some(*id) != me);
        users.sort_by_key(|id| id.0);
        let names: Vec<_> = users
            .iter()
            .take(3)
            .map(|id| self.display_name_in(&kind, &channel_id, id))
            .collect();
        typing_text(&names)
    }

    /// Name for a friend-request row. The server's display_name for the
    /// peer is ciphertext without their profile key, so resolve locally, and
    /// when nothing local knows them use the plaintext username the server
    /// sends with the request rather than the `User <id8>` placeholder.
    pub fn friend_request_peer_name(
        &self,
        user_id: &crate::domain::ids::UserId,
        username: &str,
    ) -> String {
        let name = self.display_name(user_id);
        if !username.is_empty()
            && name == crate::services::user::friend_display_name_placeholder(user_id)
        {
            username.to_string()
        } else {
            name
        }
    }

    /// Outgoing requests do not disclose the recipient's encrypted profile.
    /// A server ID or placeholder is never a useful display name.
    pub fn outgoing_friend_request_peer_name(
        &self,
        user_id: &crate::domain::ids::UserId,
        username: &str,
    ) -> String {
        let name = self.display_name(user_id);
        let opaque = |value: &str| {
            value == user_id.to_string()
                || (value.len() >= 24 && value.chars().all(|c| c.is_ascii_hexdigit()))
        };
        if name != crate::services::user::friend_display_name_placeholder(user_id)
            && !opaque(&name)
        {
            return name;
        }
        let username = username.trim();
        if !username.is_empty() && !opaque(username)
        {
            return username.to_string();
        }
        "Pending request".to_string()
    }

    /// Resolve a display name *within a conversation*, so anonymous channels
    /// never leak the global profile name. In an anon group the per-group
    /// member name wins; everywhere else this is just `display_name`.
    ///
    /// `display_name` alone prefers the global profile, which defeats
    /// anonymity when the sender is also a known contact — this is what the
    /// message sender and reply-quote author must use in anon conversations.
    pub fn display_name_in(
        &self,
        conv_type: &crate::domain::message::ConversationType,
        conv_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> String {
        if *conv_type == crate::domain::message::ConversationType::AnonGroup {
            let group_id = crate::domain::anon_group::AnonGroupId::from_uuid(*conv_id.as_uuid());
            if let Some(name) = self
                .get_anon_group_member_display_name(&group_id, user_id)
                .filter(|n| !n.is_empty())
            {
                return name;
            }
        }
        self.display_name(user_id)
    }

    /// Our own avatar bytes (raw image data stored in settings).
    pub fn my_avatar(&self) -> Option<Vec<u8>> {
        self.anon_dms().get_my_avatar()
    }

    pub async fn list_friends(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friends(limit, offset).await
    }

    pub fn get_profile_local(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::user::Profile> {
        self.users().get_profile_local(user_id).ok().flatten()
    }

    /// A peer's complete handle, if this device actually knows it. The local
    /// directory can contain a bare login nickname or a placeholder user id;
    /// neither resolves through username lookup and neither belongs after `@`.
    pub fn known_username(&self, user_id: &crate::domain::ids::UserId) -> Option<String> {
        self.get_user_local(user_id)
            .map(|u| u.username)
            .filter(|u| crate::services::user::is_lookup_handle(u))
    }

    pub fn get_user_local(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::user::User> {
        self.users().get_user_local(user_id).ok().flatten()
    }

    /// Resolve a call participant's name and avatar bytes from local state,
    /// by LiveKit identity (a user_id string). A non-user_id identity
    /// (unlikely) falls back to showing the raw identity.
    pub fn resolve_call_participant(
        &self,
        identity: &str,
    ) -> crate::ui::chat_data::CallParticipantInfo {
        use crate::ui::chat_data::CallParticipantInfo;
        match identity.parse::<crate::domain::ids::UserId>() {
            Ok(uid) => {
                let display_name = self.display_name(&uid);
                let avatar_bytes = self
                    .get_profile_local(&uid)
                    .and_then(|p| p.avatar_bytes)
                    .or_else(|| self.anon_dm_peer_avatar(&uid));
                CallParticipantInfo {
                    display_name,
                    avatar_bytes,
                }
            }
            Err(_) => CallParticipantInfo {
                display_name: identity.to_string(),
                avatar_bytes: None,
            },
        }
    }

}

fn typing_text(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [name] => format!("{name} is typing…"),
        [first, second] => format!("{first} and {second} are typing…"),
        _ => "Several people are typing…".to_string(),
    }
}
