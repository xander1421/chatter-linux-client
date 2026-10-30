use super::*;

impl ChatterClient {
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
        for dm in self
            .anon_dms()
            .anon_dm_repo()
            .list_conversations()
            .unwrap_or_default()
        {
            if self.is_anon_dm_conversation_blocked(&dm.id) == Some(true)
                && !out.iter().any(|c| c.id == dm.peer_id)
            {
                out.push(BlockedContact {
                    id: dm.peer_id,
                    anon: true,
                    anon_chat: true,
                });
            }
        }
        for id in self.users().blocked_local() {
            if !out.iter().any(|c| c.id == id) {
                out.push(BlockedContact {
                    id,
                    anon: false,
                    anon_chat: false,
                });
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
            .filter(|r| {
                !r.user_id
                    .parse()
                    .is_ok_and(|id| self.is_profile_user_blocked(&id))
            })
            .collect()
    }

    // ── Unified block/unblock (routes anon vs server) ────────────────

    /// Resolve a profile identifier (may be a UserId or an AnonDmId
    /// stored as a UUID) into the correct blocking path and execute it.
    pub async fn block_profile_user(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        // 1. Check if id is a peer in an anon DM. A secret-chat peer who is
        // also a friend or has a server DM is one person: block both paths, or
        // their ordinary DMs keep arriving. An anon-only peer stays off the
        // server so the block does not reveal the secret chat.
        if let Some(conv_id) = self.find_anon_dm_by_peer(id) {
            self.anon_dms()
                .block_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            self.anon_dms()
                .block_user(id, Some("Blocked from profile"))
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            if !self.has_server_relationship(id) {
                return Ok(());
            }
            return self
                .users()
                .block_user(id)
                .await
                .map_err(|e| super::ClientError::Inbox(e.to_string()));
        }
        // 2. Check if id is actually a conversation ID (Friends tab uses dm.id)
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if let Ok(Some(dm)) = self.anon_dms().get_conversation(&conv_id) {
            self.anon_dms()
                .block_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            self.anon_dms()
                .block_user(&dm.peer_id, Some("Blocked from profile"))
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            return Ok(());
        }
        // 3. Server-side friendship block
        self.users()
            .block_user(id)
            .await
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
            return self
                .unblock_anon_dm_user(id)
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
            if !self.users().is_blocked_local(id) {
                return Ok(());
            }
        }

        self.users()
            .unblock_user(id)
            .await
            .map_err(|e| super::ClientError::Inbox(e.to_string()))
    }

    /// Friends › Blocked › Remove: end the friendship, then lift the block, so
    /// they become a stranger. Friendship first, while still blocked, so there
    /// is no moment where they are an unblocked friend. Not being friends
    /// (a stranger we blocked) is not an error.
    ///
    /// An anon-DM peer has no friendship to end. With a chat still stored the
    /// row reads "Delete chat" and does what "Delete chat" does from the chat
    /// itself: the secret chat ends for both sides (`end_conversation`, which
    /// fails closed while the peer is reachable). The block stays.
    pub async fn remove_blocked_contact(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        use crate::error::NetworkError;
        use crate::services::user::UserError;
        if let Some(conv_id) = self.find_anon_dm_by_peer(id) {
            return self
                .anon_dms()
                .end_conversation(&conv_id)
                .await
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

    /// Whether the server already sees a tie to this person: a friendship or a
    /// server DM (whose channel id is the peer's user id).
    fn has_server_relationship(&self, id: &crate::domain::ids::UserId) -> bool {
        self.users().is_friend_local(id)
            || self
                .services
                .channel_repo
                .get(&(*id).into())
                .ok()
                .flatten()
                .is_some()
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
        let Some(me) = self.auth_state().get().current_user_id else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for f in self.users().get_friends_local().unwrap_or_default() {
            if f.status != FriendshipStatus::Accepted {
                continue;
            }
            // The row may be stored in either direction; the peer is whichever
            // party isn't us.
            let peer = if f.requester_id == me {
                f.addressee_id
            } else {
                f.requester_id
            };
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
        let Some(me) = self.auth_state().get().current_user_id else {
            return out;
        };
        if self.anon_mode_enabled() {
            return out;
        }
        let accepted = self
            .services
            .conversation_repo
            .dm_conversations_sent_in(&me.to_string())
            .unwrap_or_default();
        for peer in accepted
            .iter()
            .filter_map(|id| id.parse::<crate::domain::ids::UserId>().ok())
        {
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
            && id
                .parse()
                .is_ok_and(|peer| self.is_profile_user_blocked(&peer))
    }

    /// Check if a profile identifier is blocked (anon or server-side).
    pub fn is_profile_user_blocked(&self, id: &crate::domain::ids::UserId) -> bool {
        // Anon: check by peer ID
        if self.anon_dms().is_user_blocked(id).unwrap_or(false) {
            return true;
        }
        // Anon: check by conversation ID
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if self
            .anon_dms()
            .is_conversation_blocked(&conv_id)
            .unwrap_or(false)
        {
            return true;
        }
        // Server-side: the durable table. `user_state`'s in-memory set is never
        // populated, so it answered "not blocked" for every server block.
        self.users().is_blocked_local(id)
    }

    /// The blocked peer of a DM or secret chat, if this conversation cannot
    /// accept a reply until the person is unblocked.
    pub fn blocked_dm_peer(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> Option<crate::domain::ids::UserId> {
        let peer = match conversation {
            crate::state::ActiveConversation::ServerDm { peer_id } => peer_id,
            crate::state::ActiveConversation::AnonDm(id) => self.get_anon_dm_conversation(&id)?.peer_id,
            _ => return None,
        };
        self.is_profile_user_blocked(&peer).then_some(peer)
    }

}
