//! profile façade — thin delegates to services.

use std::sync::Arc;

use crate::domain::message::ConversationType;
use crate::state::ActiveConversation;
use super::ChatterClient;

/// One Friends › Blocked row (`blocked_users`). `anon` marks an anon-DM peer
/// block; `anon_chat` that its conversation is still stored locally, which is
/// what the row's "Delete chat" removes (`remove_blocked_contact`).
pub struct BlockedContact {
    pub id: crate::domain::ids::UserId,
    pub anon: bool,
    pub anon_chat: bool,
}

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
        let names: Vec<_> = users.iter().take(3)
            .map(|id| self.display_name_in(&kind, &channel_id, id)).collect();
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
            let group_id =
                crate::domain::anon_group::AnonGroupId::from_uuid(*conv_id.as_uuid());
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

    /// A peer's handle, if this device actually knows it. The local user
    /// directory fills `username` with the user id when the handle is unknown
    /// (the server stores only a lookup hash), and that id must not be shown
    /// as "@username".
    pub fn known_username(&self, user_id: &crate::domain::ids::UserId) -> Option<String> {
        self.get_user_local(user_id)
            .map(|u| u.username)
            .filter(|u| !u.is_empty() && u.parse::<uuid::Uuid>().is_err())
    }

    pub fn get_user_local(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::user::User> {
        self.users().get_user_local(user_id).ok().flatten()
    }

    /// Resolve a call participant's view data (display name, initials, avatar
    /// bytes) from local state, by LiveKit identity (a user_id string). The UI
    /// only decodes `avatar_bytes` to an image and renders — identity/profile
    /// resolution (decrypted name, the server-DM-profile-vs-anon-DM-row avatar
    /// fallback, initials) stays in core, not the UI. A non-user_id identity
    /// (unlikely) falls back to showing the raw identity.
    pub fn resolve_call_participant(
        &self,
        identity: &str,
    ) -> crate::ui::chat_data::CallParticipantInfo {
        use crate::ui::chat_data::{initials_from_name, CallParticipantInfo};
        match identity.parse::<crate::domain::ids::UserId>() {
            Ok(uid) => {
                let display_name = self.display_name(&uid);
                let avatar_bytes = self
                    .get_profile_local(&uid)
                    .and_then(|p| p.avatar_bytes)
                    .or_else(|| self.anon_dm_peer_avatar(&uid));
                CallParticipantInfo {
                    initials: initials_from_name(&display_name),
                    display_name,
                    avatar_bytes,
                }
            }
            Err(_) => CallParticipantInfo {
                initials: initials_from_name(identity),
                display_name: identity.to_string(),
                avatar_bytes: None,
            },
        }
    }

    pub fn generate_friend_code(&self) -> Result<String, crate::services::UserError> {
        self.users().generate_friend_code()
    }

    pub fn generate_private_friend_code(&self) -> Result<String, crate::services::UserError> {
        // Anon contact setup is only safe in shield mode — otherwise the code
        // generation/redemption and the resulting conversation run over the
        // identified/direct path, leaking what anon DM exists to hide.
        if !self.anon_mode_enabled() {
            return Err(crate::services::UserError::RequiresAnonMode);
        }
        self.users().generate_private_friend_code()
    }

    pub fn generate_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
        self.users().generate_friend_qr(size)
    }

    pub fn generate_private_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
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
        // A regular friend code ends in an identified friend request; shield
        // mode redeems private codes (`start_anon_dm_conversation_private`).
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

    pub async fn list_friend_requests(
        &self,
        filter: crate::services::user::FriendRequestFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friend_requests(filter, limit, offset).await
    }

    /// Convenience: incoming-only friend requests.
    pub async fn list_pending_friend_requests(
        &self,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friend_requests(
            crate::services::user::FriendRequestFilter::Received,
            100,
            0,
        ).await
    }

    /// Update the current user's profile and push it to every peer who can't
    /// fetch it from the server: each ready anon DM, and the anon groups this
    /// user owns. The push runs in the background — a slow or failing peer
    /// never holds up (or fails) the save itself.
    pub async fn update_user_profile(
        self: &std::sync::Arc<Self>,
        display_name: Option<String>,
        status_message: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        let profile = self.users().update_user_profile(display_name, status_message).await?;

        let display_name = Some(profile.display_name.clone());
        let bio = profile.status_message.as_ref().map(|m| m.text.clone());
        for conv in self.list_anon_dm_conversations().into_iter().filter(|c| c.is_ready()) {
            let client = self.clone();
            let (display_name, bio) = (display_name.clone(), bio.clone());
            tokio::spawn(async move {
                // The avatar rides inside the update as encrypted bytes; there
                // is no plaintext avatar URL to send.
                if let Err(e) = client.send_anon_dm_profile_update(&conv.id, display_name, None, bio).await {
                    tracing::warn!(conversation = %conv.id, error = %e, "profile update to anon DM failed");
                }
            });
        }
        let client = self.clone();
        tokio::spawn(async move {
            if let Err(e) = client.broadcast_profile_to_owned_groups().await {
                tracing::warn!(error = %e, "profile update to owned anon groups failed");
            }
        });

        Ok(profile)
    }

    pub fn list_anon_dm_conversations(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_conversations().unwrap_or_default()
    }

    pub async fn start_anon_dm_conversation_private(
        &self,
        friend_code: &crate::domain::PrivateFriendCode,
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        // Redeeming a private code + firing the ECIES init must ride the anon
        // transport (isolated Tor circuit, blind-token delivery). Outside shield
        // mode it would go over the identified/direct path — refuse.
        if !self.anon_mode_enabled() {
            return Err(crate::services::anon_dm::AnonDmError::RequiresAnonMode);
        }
        self.anon_dms().start_conversation_private(friend_code, intro_message, sender_display_name).await
    }

    pub fn list_anon_dm_ready_conversations(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_ready_conversations().unwrap_or_default()
    }

    pub fn list_anon_dm_message_requests(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_message_requests().unwrap_or_default()
    }

    pub fn list_anon_dm_outgoing_requests(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_outgoing_requests().unwrap_or_default()
    }

    pub fn get_latest_anon_dm_message(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<crate::domain::message::Message> {
        self.anon_dms().get_latest_message(conversation_id).ok().flatten()
    }

    /// Everyone this user blocked: anon-DM peers and server-side blocks, in
    /// one de-duplicated list. The Friends → Blocked tab renders exactly this.
    pub fn blocked_users(&self) -> Vec<BlockedContact> {
        let mut out: Vec<BlockedContact> = self
            .anon_dms()
            .get_blocked_users()
            .unwrap_or_default()
            .into_iter()
            .map(|id| BlockedContact {
                anon: true,
                anon_chat: self.find_anon_dm_by_peer(&id).is_some(),
                id,
            })
            .collect();
        // A chat can be blocked without its peer (a block whose second half,
        // the peer row, failed). Still blocked, so still listed — otherwise
        // nothing on screen could lift it.
        for dm in self.anon_dms().anon_dm_repo().list_conversations().unwrap_or_default() {
            if self.is_anon_dm_conversation_blocked(&dm.id) == Some(true)
                && !out.iter().any(|c| c.id == dm.peer_id)
            {
                out.push(BlockedContact { id: dm.peer_id, anon: true, anon_chat: true });
            }
        }
        for id in self.users().blocked_local() {
            if !out.iter().any(|c| c.id == id) {
                out.push(BlockedContact { id, anon: false, anon_chat: false });
            }
        }
        out
    }

    /// Received friend requests minus the ones from people we blocked: the
    /// server keeps them pending, the Requests tab never shows them (same rule
    /// as `listed_friends`).
    pub fn visible_friend_requests(
        &self,
        received: Vec<crate::inbox::events::FriendRequestInfo>,
    ) -> Vec<crate::inbox::events::FriendRequestInfo> {
        received
            .into_iter()
            .filter(|r| !r.user_id.parse().is_ok_and(|id| self.is_profile_user_blocked(&id)))
            .collect()
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
            .get_conversation_by_peer(user_id)
            .ok()
            .flatten()
            .map(|dm| dm.id)
    }

    /// Decrypted avatar bytes for an anonymous DM peer (the user directory has
    /// no profile for anon peers — their avatar lives on the anon DM row, same
    /// source the sidebar uses).
    pub fn anon_dm_peer_avatar(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<Vec<u8>> {
        self.anon_dms()
            .get_conversation_by_peer(user_id)
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

    // ── Unified block/unblock (routes anon vs server) ────────────────

    /// Resolve a profile identifier (may be a UserId or an AnonDmId
    /// stored as a UUID) into the correct blocking path and execute it.
    pub async fn block_profile_user(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        // 1. Check if id is a peer in an anon DM
        if let Some(conv_id) = self.find_anon_dm_by_peer(id) {
            self.anon_dms().block_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            self.anon_dms().block_user(id, Some("Blocked from profile"))
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            return Ok(());
        }
        // 2. Check if id is actually a conversation ID (Friends tab uses dm.id)
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if let Ok(Some(dm)) = self.anon_dms().get_conversation(&conv_id) {
            self.anon_dms().block_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            self.anon_dms().block_user(&dm.peer_id, Some("Blocked from profile"))
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            return Ok(());
        }
        // 3. Server-side friendship block
        self.users().block_user(id).await
            .map_err(|e| super::ClientError::Inbox(e.to_string()))
    }

    /// Reverse of [`block_profile_user`]. Re-creates the ephemeral
    /// mailbox so polling resumes for the unblocked conversation.
    pub async fn unblock_profile_user(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        let conv_id = if let Some(cid) = self.find_anon_dm_by_peer(id) {
            self.unblock_anon_dm_user(id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            Some(cid)
        } else {
            let cid = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
            if let Ok(Some(dm)) = self.anon_dms().get_conversation(&cid) {
                self.unblock_anon_dm_user(&dm.peer_id)
                    .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
                Some(cid)
            } else {
                None
            }
        };

        if conv_id.is_none() && self.anon_dms().is_user_blocked(id).unwrap_or(false) {
            // An anon peer whose chat was deleted (Friends › Blocked › Delete
            // chat): only the peer block is left to lift.
            return self.unblock_anon_dm_user(id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()));
        }

        if let Some(cid) = conv_id {
            self.unblock_anon_dm_conversation(&cid)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            // The local mailbox keys were preserved during block (only
            // polling was suspended). Setting status back to Active is
            // enough — the poller picks up the existing keys. If the
            // server-side mailbox expired during the block period, the
            // poll will fail and the next renewal cycle handles it.
            return Ok(());
        }

        self.users().unblock_user(id).await
            .map_err(|e| super::ClientError::Inbox(e.to_string()))
    }

    /// Friends › Blocked › Remove: end the friendship, then lift the block, so
    /// they become a stranger. Friendship first, while still blocked, so there
    /// is no moment where they are an unblocked friend. Not being friends
    /// (a stranger we blocked) is not an error.
    ///
    /// An anon-DM peer has no friendship to end. With a chat still stored the
    /// row reads "Delete chat" and does what Signal's Delete on a blocked chat
    /// does: the conversation and its history go, the block stays.
    pub async fn remove_blocked_contact(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        use crate::error::NetworkError;
        use crate::services::user::UserError;
        if let Some(conv_id) = self.find_anon_dm_by_peer(id) {
            return self
                .anon_dms()
                .delete_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()));
        }
        if !self.anon_dms().is_user_blocked(id).unwrap_or(false) {
            match self.users().remove_friend(id).await {
                Ok(()) => {}
                // RemoveFriend's only FailedPrecondition is ErrNotFriends.
                Err(UserError::Network(NetworkError::InvalidRequest(m)))
                    if m.starts_with("precondition failed") => {}
                Err(e) => return Err(super::ClientError::Inbox(e.to_string())),
            }
        }
        self.unblock_profile_user(id).await
    }

    /// Whether this user is an accepted friend (local friendships table).
    pub fn is_friend(&self, id: &crate::domain::ids::UserId) -> bool {
        self.users().is_friend_local(id)
    }

    /// Accepted friends to list as contacts — every place a friend is shown
    /// (DM sidebar, invite pickers) reads this. Blocking keeps the friendship,
    /// Signal-style, so this is where a blocked friend drops out; unblocking
    /// brings them back with their history. Friends › Blocked
    /// (`blocked_users`) is the only list that still shows them.
    pub fn listed_friends(&self) -> Vec<crate::domain::ids::UserId> {
        use crate::domain::user::FriendshipStatus;
        let Some(me) = self.auth_state().get().current_user_id else { return Vec::new() };
        let mut out = Vec::new();
        for f in self.users().get_friends_local().unwrap_or_default() {
            if f.status != FriendshipStatus::Accepted {
                continue;
            }
            // The row may be stored in either direction; the peer is whichever
            // party isn't us.
            let peer = if f.requester_id == me { f.addressee_id } else { f.requester_id };
            if !out.contains(&peer) && !self.is_profile_user_blocked(&peer) {
                out.push(peer);
            }
        }
        out
    }

    /// Peers that get a friend-DM row: every listed friend, then — outside
    /// Shield — anyone we have written to in a DM that still has history.
    /// Removing a contact never deletes the conversation (Signal): unfriending
    /// used to drop the row on BOTH sides, because only the friendship listed
    /// it. A DM we never replied to stays unlisted (a stranger's cold message
    /// is not a chat). "Delete chat" (hard-deletes the history) removes a
    /// non-friend's row. Shield lists friends only: a non-friend DM does not
    /// open there (`resolve_dm_conversation`).
    pub fn dm_peers(&self) -> Vec<crate::domain::ids::UserId> {
        let mut out = self.listed_friends();
        let Some(me) = self.auth_state().get().current_user_id else { return out };
        if self.anon_mode_enabled() {
            return out;
        }
        let accepted = self
            .services
            .conversation_repo
            .dm_conversations_sent_in(&me.to_string())
            .unwrap_or_default();
        for peer in accepted.iter().filter_map(|id| id.parse::<crate::domain::ids::UserId>().ok()) {
            if peer != me && !out.contains(&peer) && !self.is_profile_user_blocked(&peer) {
                out.push(peer);
            }
        }
        out
    }

    /// Whether a conversation is a DM with someone we blocked, so it is left
    /// out of folders and search like the sidebar leaves it out
    /// (`listed_friends`). A friend DM's id is the peer's user id.
    pub fn is_blocked_conversation(
        &self,
        kind: &crate::domain::message::ConversationType,
        id: &str,
    ) -> bool {
        *kind == crate::domain::message::ConversationType::ServerDm
            && id.parse().is_ok_and(|peer| self.is_profile_user_blocked(&peer))
    }

    /// Check if a profile identifier is blocked (anon or server-side).
    pub fn is_profile_user_blocked(&self, id: &crate::domain::ids::UserId) -> bool {
        // Anon: check by peer ID
        if self.anon_dms().is_user_blocked(id).unwrap_or(false) {
            return true;
        }
        // Anon: check by conversation ID
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if self.anon_dms().is_conversation_blocked(&conv_id).unwrap_or(false) {
            return true;
        }
        // Server-side: the durable table. `user_state`'s in-memory set is never
        // populated, so it answered "not blocked" for every server block.
        self.users().is_blocked_local(id)
    }

    pub async fn send_anon_dm_presence_update(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        presence: crate::domain::user::PresenceStatus,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().send_presence_update(conversation_id, presence).await
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
        self.anon_dms().get_messages_asc(conversation_id, before_sent_at, limit)
    }

    pub fn list_anon_dm_pinned(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Vec<crate::domain::message::Message> {
        self.anon_dms().list_pinned(conversation_id).unwrap_or_default()
    }

    /// Look up a single member of an anonymous group.
    pub fn get_anon_group_member(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::anon_group::AnonGroupMember> {
        self.anon_groups().get_member(group_id, user_id).ok().flatten()
    }

    /// Get an attachment record by id (local storage).
    pub fn get_attachment(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<crate::domain::attachment::Attachment> {
        self.attachments().get(attachment_id).ok().flatten()
    }

    pub fn get_attachment_metadata(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<crate::domain::attachment::Attachment> {
        self.attachments().get_metadata(attachment_id).ok().flatten()
    }

    pub fn decrypt_attachment_auto(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::crypto::secure_buffer::SecureBuffer, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_auto(attachment_id)
    }

    /// Open a random-access decryptor for streaming playback (video/audio).
    ///
    /// Authenticates the whole file, then serves decrypted ranges on demand so
    /// the player starts without a full decrypt-to-memory. See
    /// `AttachmentService::open_range_decryptor`.
    pub fn open_attachment_range(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::services::attachment::CbcRangeDecryptor, crate::services::attachment::AttachmentError> {
        self.attachments().open_range_decryptor(attachment_id)
    }

    pub async fn download_attachment_to_filesystem(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<(), crate::services::attachment::AttachmentError> {
        self.attachments().download_to_filesystem(attachment_id).await
    }

    pub fn decrypt_and_decode_gif_first_frame(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<Option<crate::services::attachment::GifDecodeResult>, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_and_decode_gif_first_frame(attachment_id)
    }

    pub fn decrypt_and_get_image_info(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::services::attachment::ImageDecodeResult, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_and_get_image_info(attachment_id)
    }

    /// Decrypt a video attachment and extract a thumbnail frame.
    ///
    /// Blocking (GStreamer) — call from a background thread. Used to derive a
    /// thumbnail for server-DM videos, which arrive without inline thumbnails.
    pub fn decrypt_and_extract_video_thumbnail(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::services::attachment::ImageDecodeResult, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_and_extract_video_thumbnail(attachment_id)
    }

    /// Decode every frame of a GIF from already-decrypted bytes.
    ///
    /// Blocking — call from a background thread (e.g. `spawn_blocking`).
    /// Thin façade over [`AttachmentService::decode_gif_all_frames`] so the UI
    /// never has to reach into the service directly.
    pub fn decode_gif_all_frames(
        &self,
        gif_bytes: &[u8],
    ) -> Result<crate::services::attachment::GifFullDecodeResult, crate::services::attachment::AttachmentError> {
        crate::services::attachment::AttachmentService::decode_gif_all_frames(gif_bytes)
    }

    pub fn update_attachment(
        &self,
        attachment: &crate::domain::attachment::Attachment,
    ) -> Result<(), crate::services::attachment::AttachmentError> {
        self.attachments().update(attachment)
    }

    pub fn get_attachment_preview_image(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.attachments().get_preview_image(attachment_id)
    }

    /// Fetch + decrypt the thumbnail sidecar for a received attachment, caching
    /// the bytes locally. Gives a preview without downloading the full blob.
    pub async fn ensure_attachment_thumbnail(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<Vec<u8>> {
        self.attachments().ensure_thumbnail(attachment_id).await
    }

    pub async fn upload_attachment_to_cdn_encrypted(
        &self,
        path: &std::path::Path,
    ) -> Result<crate::services::attachment::CdnUploadResult, crate::services::attachment::AttachmentError> {
        self.attachments().upload_to_cdn_encrypted(path).await
    }

    /// Encrypt/upload a recorded voice note, preserving voice metadata on every
    /// platform; the caller retains the recording for retry until sent/cancelled.
    pub async fn upload_voice_note(
        &self,
        note: &crate::services::voice_recording::RecordedVoiceNote,
    ) -> Result<crate::state::chat_state::PendingAttachment, String> {
        use crate::services::voice_recording::recording_context_is_current;
        if !recording_context_is_current(self.chat_state(), note.conversation) {
            return Err("Conversation changed; recording discarded".into());
        }
        let upload = self.upload_attachment_to_cdn_encrypted(note.file.path()).await
            .map_err(|e| e.to_string())?;
        if !recording_context_is_current(self.chat_state(), note.conversation) {
            return Err("Conversation changed; recording discarded".into());
        }
        let mut pointer = upload.to_attachment_pointer();
        pointer.voice_note = true;
        pointer.waveform = note.waveform.clone();
        pointer.duration_ms = note.duration_ms;
        pointer.file_name = "Voice message.wav".into();
        // The sender renders from the already-uploaded local attachment row;
        // receivers render from the pointer. Keep the same metadata on both.
        let mut attachment = self.attachments().get(&upload.attachment_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Uploaded voice attachment is missing from local storage".to_string())?;
        attachment.metadata.name = pointer.file_name.clone();
        attachment.metadata.waveform_samples = Some(pointer.waveform.clone());
        attachment.metadata.duration_seconds = Some(pointer.duration_ms as f64 / 1000.0);
        self.attachments().update(&attachment).map_err(|e| e.to_string())?;

        Ok(crate::state::chat_state::PendingAttachment::with_pointer(
            upload.attachment_id, std::path::PathBuf::new(), pointer.file_name.clone(),
            upload.plaintext_size, "audio/wav".into(), pointer,
        ))
    }

    pub async fn download_url_bytes(
        &self,
        url: &str,
    ) -> Result<Vec<u8>, crate::services::attachment::AttachmentError> {
        self.attachments().download_url_bytes(url).await
    }

    pub async fn download_and_upload_gif(
        &self,
        url: &str,
        gif_id: &str,
    ) -> Result<crate::services::attachment::CdnUploadResult, crate::services::attachment::AttachmentError> {
        self.attachments().download_and_upload_gif(url, gif_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message;
    use crate::domain::anon_dm::AnonDm;
    use crate::domain::ids::{ChannelId, UserId};

    fn test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (client, dir)
    }

    #[tokio::test]
    async fn typing_aggregate_survives_one_sender_stopping_and_expires() {
        let (client, _dir) = test_client();
        let channel = ChannelId::new();
        let alice = UserId::from_uuid(uuid::Uuid::from_u128(1));
        let bob = UserId::from_uuid(uuid::Uuid::from_u128(2));
        assert!(client.update_typing_indicator(channel, alice, 100, true));
        assert!(client.update_typing_indicator(channel, bob, 100, true));
        assert_eq!(client.typing_indicator_text(channel), format!(
            "{} and {} are typing…", client.display_name(&alice), client.display_name(&bob)));
        assert!(client.update_typing_indicator(channel, alice, 200, false));
        assert_eq!(client.typing_indicator_text(channel), format!("{} is typing…", client.display_name(&bob)));
        assert!(!client.update_typing_indicator(channel, alice, 150, true));
        assert!(client.update_typing_indicator(channel, alice, 300, true));
        assert!(client.update_typing_indicator(channel, UserId::new(), 300, true));
        assert_eq!(client.typing_indicator_text(channel), "Several people are typing…");
        client.chat_state().clear_typing(&channel);
        assert_eq!(client.typing_indicator_text(channel), "");
    }

    /// Nothing to change (no name, no status, no avatar) sends nothing: the
    /// server reads a maskless update as "every field". The test server is
    /// unreachable, so any RPC would fail this.
    #[tokio::test]
    async fn profile_update_with_nothing_to_change_sends_no_unmasked_update() {
        use crate::storage::repositories::settings_repo::keys;
        let (client, _dir) = test_client();
        let me = UserId::new();
        client.messages().settings_repo.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        client.messages().user_repo.ensure_user_exists(&me).unwrap();
        client.messages().user_repo.upsert_profile(&crate::domain::user::Profile::new(me, "me".into())).unwrap();
        let profile = client.users().update_user_profile(None, None).await.expect("no RPC");
        assert_eq!(profile.user_id, me);
    }

    /// Friends › Blocked › Delete chat on an anon peer: the conversation and
    /// its history go, the block stays, and the peer's friend requests stay
    /// hidden. Unblock afterwards lifts the peer block (no chat to go through).
    #[tokio::test]
    async fn delete_blocked_anon_chat_keeps_the_block() {
        let (client, _dir) = test_client();
        let peer = UserId::from_uuid(uuid::Uuid::from_u128(7));
        let dm = AnonDm::new_initiated(peer);
        client.anon_dms().anon_dm_repo().insert_conversation(&dm).unwrap();
        let conv: ChannelId = dm.id.into();
        let msg = Message::new_text(conv, ConversationType::AnonDm, Some(peer), "hi".into(), 1);
        client.messages().message_repo.insert(&msg).unwrap();
        client.block_profile_user(&peer).await.unwrap();

        let row = |c: &ChatterClient| c.blocked_users().into_iter().find(|b| b.id == peer).unwrap();
        assert!(row(&client).anon && row(&client).anon_chat);

        client.remove_blocked_contact(&peer).await.unwrap();
        assert!(client.find_anon_dm_by_peer(&peer).is_none());
        assert!(client.messages().message_repo.get(&msg.id).unwrap().is_none());
        assert!(client.is_profile_user_blocked(&peer));
        assert!(row(&client).anon && !row(&client).anon_chat);

        let req = |id: &UserId| crate::inbox::events::FriendRequestInfo {
            user_id: id.to_string(),
            display_name: String::new(),
            username: String::new(),
            encrypted_intro: Vec::new(),
        };
        let other = UserId::from_uuid(uuid::Uuid::from_u128(8));
        let shown = client.visible_friend_requests(vec![req(&peer), req(&other)]);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].user_id, other.to_string());

        client.unblock_profile_user(&peer).await.unwrap();
        assert!(!client.is_profile_user_blocked(&peer));
    }

    /// An anon chat blocked without its peer row (half a block) is still
    /// listed in Friends › Blocked, and Unblock there lifts it.
    #[tokio::test]
    async fn chat_only_anon_block_is_listed_and_unblockable() {
        let (client, _dir) = test_client();
        let peer = UserId::from_uuid(uuid::Uuid::from_u128(9));
        let dm = AnonDm::new_initiated(peer);
        client.anon_dms().anon_dm_repo().insert_conversation(&dm).unwrap();
        client.block_anon_dm_conversation(&dm.id).unwrap();

        let row = client.blocked_users().into_iter().find(|b| b.id == peer).unwrap();
        assert!(row.anon && row.anon_chat);

        client.unblock_profile_user(&peer).await.unwrap();
        assert_eq!(client.is_anon_dm_conversation_blocked(&dm.id), Some(false));
        assert!(client.blocked_users().iter().all(|b| b.id != peer));
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
