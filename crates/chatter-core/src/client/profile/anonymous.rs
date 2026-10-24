use super::*;

impl ChatterClient {
    pub fn list_anon_dm_conversations(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_conversations().unwrap_or_default()
    }

    pub async fn start_anon_dm_conversation_private(
        &self,
        friend_code: &crate::domain::PrivateFriendCode,
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        // The anon DM service requires the isolated onion channel for this
        // flow, including when Shield is off.
        self.anon_dms()
            .start_conversation_private(friend_code, intro_message, sender_display_name)
            .await
    }

    pub fn list_anon_dm_ready_conversations(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms()
            .list_ready_conversations()
            .unwrap_or_default()
    }

    pub fn list_anon_dm_message_requests(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_message_requests().unwrap_or_default()
    }

    pub fn reconcile_anon_dm_request_rows<T>(
        &self,
        current: impl IntoIterator<Item = T>,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        row_id: impl Fn(&T) -> &str,
        incoming: T,
    ) -> Vec<T> {
        self.anon_dms()
            .reconcile_request_rows(current, conversation_id, row_id, incoming)
    }

    pub fn list_anon_dm_outgoing_requests(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_outgoing_requests().unwrap_or_default()
    }

    pub fn get_latest_anon_dm_message(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<crate::domain::message::Message> {
        self.anon_dms()
            .get_latest_message(conversation_id)
            .ok()
            .flatten()
    }

    pub fn block_anon_dm_conversation(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().block_conversation(conversation_id)
    }

    pub fn block_anon_dm_user(
        &self,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().block_user(user_id, reason)
    }

    /// Check if a user ID belongs to an anonymous DM peer. Returns the
    /// conversation ID if found.
    pub fn find_anon_dm_by_peer(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::anon_dm::AnonDmId> {
        self.anon_dms()
            .get_live_conversation_by_peer(user_id)
            .ok()
            .flatten()
            .map(|dm| dm.id)
    }

    /// A secret DM's peer card. The local contact name wins; otherwise use
    /// the name shared inside the chat. The private avatar wins over an
    /// accepted friend's account avatar. `None` when the chat is unknown.
    pub fn anon_dm_peer_card(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<(String, Option<Vec<u8>>)> {
        let dm = self
            .anon_dms()
            .get_conversation(conversation_id)
            .ok()
            .flatten()?;
        let avatar = self.anon_dm_avatar(&dm);
        let name = self.anon_dm_peer_name(&dm);
        Some((name, avatar))
    }

    /// A locally named contact keeps that name in both the secret-chat row
    /// and header; other peers retain the name shared inside the chat.
    pub fn anon_dm_peer_name(&self, dm: &crate::domain::anon_dm::AnonDm) -> String {
        self.contact_name(&dm.peer_id)
            .or_else(|| dm.peer_display_name.clone().filter(|n| !n.is_empty()))
            .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id))
    }

    /// Prefer the avatar shared inside this secret chat. For an accepted
    /// friend, use their locally decrypted friend profile until their secret
    /// chat avatar arrives. Unknown private peers never borrow account data.
    pub fn anon_dm_avatar(&self, dm: &crate::domain::anon_dm::AnonDm) -> Option<Vec<u8>> {
        self.anon_dm_avatar_for_peer(&dm.peer_id, dm.peer_avatar.clone())
    }

    /// Resolve an incoming secret-chat event before its conversation row is
    /// necessarily committed, with the same rule as stored request rows.
    pub fn anon_dm_avatar_for_peer(
        &self,
        peer: &crate::domain::ids::UserId,
        shared_avatar: Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        shared_avatar
            .filter(|bytes| !bytes.is_empty())
            .or_else(|| self.is_friend(peer).then(|| self.user_avatar(peer)).flatten())
    }

    /// Decrypted avatar bytes for an anonymous DM peer (the user directory has
    /// no profile for anon peers — their avatar lives on the anon DM row, same
    /// source the sidebar uses).
    pub fn anon_dm_peer_avatar(&self, user_id: &crate::domain::ids::UserId) -> Option<Vec<u8>> {
        self.anon_dms()
            .get_live_conversation_by_peer(user_id)
            .ok()
            .flatten()
            .and_then(|dm| dm.peer_avatar)
    }

    pub fn unblock_anon_dm_conversation(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().unblock_conversation(conversation_id)
    }

    pub fn unblock_anon_dm_user(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().unblock_user(user_id)
    }

    /// Check if an anon DM conversation is blocked.
    pub fn is_anon_dm_conversation_blocked(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<bool> {
        self.anon_dms()
            .is_conversation_blocked(conversation_id)
            .ok()
    }

    pub async fn send_anon_dm_presence_update(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        presence: crate::domain::user::PresenceStatus,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms()
            .send_presence_update(conversation_id, presence)
            .await
    }

    pub async fn send_anon_dm_profile_update(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        display_name: Option<String>,
        avatar_url: Option<String>,
        bio: Option<String>,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms()
            .send_profile_update(conversation_id, display_name, avatar_url, bio)
            .await
    }

    pub fn get_anon_dm_messages_asc(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        before_sent_at: Option<u64>,
        limit: i32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms()
            .get_messages_asc(conversation_id, before_sent_at, limit)
    }

    pub fn list_anon_dm_pinned(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Vec<crate::domain::message::Message> {
        self.anon_dms()
            .list_pinned(conversation_id)
            .unwrap_or_default()
    }

    /// Look up a single member of an anonymous group.
    pub fn get_anon_group_member(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::anon_group::AnonGroupMember> {
        self.anon_groups()
            .get_member(group_id, user_id)
            .ok()
            .flatten()
    }

}
