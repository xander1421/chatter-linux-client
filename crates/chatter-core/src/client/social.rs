use super::*;

impl ChatterClient {
    // ── Friend / social operations ──────────────────────────────────

    /// Send a friend request.
    pub async fn send_friend_request(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
        note: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::user::UserError> {
        // A friend request is an identified server call naming both accounts.
        // Shield mode's anon path is a private friend code, never this.
        if self.anon_mode_enabled() {
            return Err(crate::services::user::UserError::ForbiddenInAnonMode);
        }
        // The service encrypts the intro note (ECIES to the recipient's
        // identity key) — it holds the prekey-fetch client. See
        // `UserService::send_friend_request`.
        let result = self.users().send_friend_request(user_id, note).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });
        Ok(result)
    }

    /// Decrypt an incoming friend-request envelope and return the sender's
    /// profile for the Received-requests tab. Opens the envelope in the
    /// Signal actor (the facade's only signal touch; the identity secret
    /// never leaves the actor) and delegates the protocol work — profile-key
    /// persistence, profile decrypt — to
    /// [`UserService::ingest_received_friend_request`].
    pub async fn ingest_received_friend_request(
        &self,
        sender_id: &crate::domain::ids::UserId,
        ciphertext: &[u8],
    ) -> ReceivedFriendRequestProfile {
        let plaintext = if ciphertext.is_empty() {
            None
        } else {
            self.signal()
                .actor
                .open_identity_hpke(
                    crate::crypto::ecies::IdentityHpkeUse::FriendRequest,
                    ciphertext.to_vec(),
                )
                .await
                .ok()
        };
        self.users()
            .ingest_received_friend_request(sender_id, plaintext)
            .await
    }

    /// Accept or decline a friend request.
    pub async fn respond_friend_request(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
        accept: bool,
    ) -> Result<crate::services::user::FriendResponseResult, crate::services::user::UserError> {
        let result = self.users().respond_friend_request(user_id, accept).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });

        // On accept, establish the DM Signal session in the background so the
        // first call offer / message skips first-contact Sesame bootstrap on
        // the call path. No-op on the higher-UUID side (Sesame tie-break).
        if accept {
            let client = Arc::clone(self);
            let peer = *user_id;
            tokio::spawn(async move {
                if let Err(e) = client.messages().prewarm_dm_session(&peer).await {
                    tracing::warn!(peer_id = %peer, error = %e, "prewarm DM session after friend accept failed");
                }
            });

            // Acceptance must exchange our profile key for every SDK caller,
            // including headless clients. The requester sends its half from
            // FriendAccepted; neither side waits for a visible message.
            let client = Arc::clone(self);
            let peer = *user_id;
            tokio::spawn(async move {
                if let Err(e) = client.messages().send_profile_key_handshake(&peer).await {
                    tracing::warn!(peer_id = %peer, error = %e, "Accept-request: profile-key handshake send failed");
                }
            });
        }

        Ok(result)
    }

    /// Remove a friend (also used to cancel a pending sent request).
    pub async fn remove_friend(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().remove_friend(user_id).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });
        Ok(())
    }

    /// Re-poll friend requests from the server and emit `FriendRequestsUpdated`
    /// so sidebar / profile modals refresh uniformly. Shared path for
    /// send/respond/remove callers so there's one "source of truth" shape.
    /// Re-poll friend requests and emit `FriendRequestsUpdated` so the UI's
    /// event handler rebuilds the Received/Sent tabs with decrypted sender
    /// profiles + notes. Call on load to populate immediately (the single
    /// source of truth — the UI must not build these rows itself, since the
    /// encrypted envelope only reaches it via this event).
    pub async fn refresh_friend_requests(&self) {
        self.refresh_and_emit_friend_requests().await;
    }

    pub(crate) async fn refresh_and_emit_friend_requests(&self) {
        use crate::inbox::events::FriendRequestInfo;
        use crate::proto::chat_v1::friend_requests_filter;

        let mut user_client = self.services.grpc_pool.user_client();
        let received = match user_client
            .list_friend_requests(friend_requests_filter::Filter::Received, 50, 0)
            .await
        {
            Ok(resp) => resp
                .requests
                .into_iter()
                .map(|r| FriendRequestInfo {
                    user_id: r.user_id,
                    display_name: r.display_name,
                    username: r.username,
                    encrypted_intro: r.encrypted_message,
                    created_at_ns: r.added_at,
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(error = %e, "refresh_and_emit_friend_requests: received fetch failed");
                return;
            }
        };
        let sent = match user_client
            .list_friend_requests(friend_requests_filter::Filter::Sent, 50, 0)
            .await
        {
            Ok(resp) => resp
                .requests
                .into_iter()
                .map(|r| FriendRequestInfo {
                    user_id: r.user_id,
                    display_name: r.display_name,
                    username: r.username,
                    // Sent requests: we're the sender; note is for the recipient.
                    encrypted_intro: Vec::new(),
                    created_at_ns: r.added_at,
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(error = %e, "refresh_and_emit_friend_requests: sent fetch failed");
                return;
            }
        };
        self.event_bus()
            .emit_domain(DomainEvent::FriendRequestsUpdated { received, sent });
    }

    /// Block a user.
    pub async fn block_user(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().block_user(user_id).await
    }

    /// Unblock a user.
    pub async fn unblock_user(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::user::UserError> {
        self.users().unblock_user(user_id).await
    }

    /// List friends with their profile data (display name, avatar, etc.).
    pub async fn list_friends_with_profiles(
        &self,
    ) -> Result<
        Vec<(
            crate::domain::user::Friendship,
            crate::domain::user::Profile,
        )>,
        crate::services::user::UserError,
    > {
        self.users().list_friends_with_profiles(100, 0).await
    }

    // ── Anon DM management ──────────────────────────────────────────

    /// Start an anonymous DM conversation with a user.
    pub async fn start_anon_dm(
        &self,
        peer_id: crate::domain::ids::UserId,
        message: Option<&str>,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().start_conversation(peer_id, message).await
    }

    /// Accept an incoming anon DM request.
    pub async fn accept_anon_dm(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().accept_request(*conversation_id).await
    }

    /// Decline an incoming anon DM request (tells the initiator over Tor).
    pub async fn decline_anon_dm(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().decline_request(conversation_id).await
    }

    // ── Anon group management ───────────────────────────────────────

    /// Create an anonymous group.
    pub async fn create_anon_group(
        &self,
        name: String,
        members: Vec<(
            crate::domain::ids::UserId,
            crate::domain::anon_group::AnonGroupRole,
        )>,
    ) -> Result<crate::domain::anon_group::AnonGroup, crate::services::anon_group::AnonGroupError>
    {
        self.anon_groups()
            .create_group_with_roles(name, members)
            .await
    }

    /// Invite members to an anonymous group.
    pub async fn invite_to_anon_group(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        members: Vec<(
            crate::domain::ids::UserId,
            crate::domain::anon_group::AnonGroupRole,
        )>,
    ) -> Result<(), crate::services::anon_group::AnonGroupError> {
        self.anon_groups().invite_members(group_id, members).await
    }

}
