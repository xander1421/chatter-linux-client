use super::*;

impl UserService {
    // ========================================================================
    // Friend Management
    // ========================================================================

    /// Send a friend request to another user.
    pub async fn send_friend_request(
        &self,
        friend_id: &UserId,
        note: Option<&str>,
    ) -> Result<Friendship, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        // E2E friend-request envelope, ECIES-encrypted to the recipient's
        // long-term identity key (same primitive as anon-DM init). No shared
        // key exists pre-friendship, so we encrypt to their public identity
        // key; only they can decrypt. Always sent — it carries OUR profile key
        // so the recipient can render our real name + avatar before accepting.
        // The optional note rides inside the same envelope.
        let envelope = self
            .encrypt_friend_request_envelope(friend_id, note.map(str::trim).unwrap_or(""))
            .await?;

        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .send_friend_request(&friend_id_str, Some(&envelope))
            .await?;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status_response)) = event.event
            {
                break;
            }
        }

        let friendship = Friendship {
            requester_id: current_user,
            addressee_id: *friend_id,
            status: FriendshipStatus::Pending,
            created_at: Utc::now(),
        };

        // A peer known only by friend code has no users row yet, so without
        // this the pending insert FK-failed silently.
        self.user_repo.ensure_user_exists(friend_id).ok();
        let _ = self.user_repo.insert_friendship(&friendship);

        Ok(friendship)
    }

    /// Build + ECIES-encrypt the friend-request envelope to `recipient`'s
    /// long-term identity key. Reuses the anon-DM init ECIES primitive; server
    /// never sees plaintext.
    ///
    /// Plaintext layout: `has_key (1) || [profile_key (32)] || note (UTF-8,
    /// may be empty)`. The profile key lets the recipient decrypt our display
    /// name + avatar before accepting (Signal-style first-contact profile
    /// reveal); it rides only when the visibility setting lets a non-friend
    /// hold it ([`profile_visibility::shareable_profile_key`]).
    ///
    /// [`profile_visibility::shareable_profile_key`]: crate::services::profile_visibility::shareable_profile_key
    async fn encrypt_friend_request_envelope(
        &self,
        recipient: &UserId,
        note: &str,
    ) -> Result<Vec<u8>, UserError> {
        use crate::crypto::ecies;

        let profile_key = crate::services::profile_visibility::shareable_profile_key(
            &self.settings_repo,
            &self.user_repo,
            recipient,
        );

        // All-devices fetch: the identity key is account-wide, but a peer's
        // live device ids are unknown here (device 1 may be a retired
        // install), so never name one.
        let resp = self
            .grpc_pool
            .auth_client()
            .get_pre_key_bundles_all_devices(&recipient.to_string())
            .await?;
        let identity_key =
            libsignal_protocol::IdentityKey::decode(&friend_request_identity_key(resp)?)
                .map_err(|e| UserError::InvalidInput(format!("identity key decode failed: {e}")))?;

        let mut plaintext = Vec::with_capacity(1 + 32 + note.len());
        plaintext.push(u8::from(profile_key.is_some()));
        if let Some(key) = profile_key.as_ref() {
            plaintext.extend_from_slice(key);
        }
        plaintext.extend_from_slice(note.as_bytes());

        ecies::seal_to_identity(
            &identity_key,
            ecies::IdentityHpkeUse::FriendRequest,
            &plaintext,
        )
        .map_err(|e| UserError::InvalidInput(format!("envelope encryption failed: {e}")))
    }

    /// Resolve the sender's profile from an opened friend-request envelope.
    /// Protocol logic (profile-key extraction, key persistence, profile
    /// decrypt) lives here in the service; the facade opens the envelope in
    /// the Signal actor (the identity secret never leaves it) and delegates.
    ///
    /// `plaintext` = `has_key (1) || [profile_key (32)] || note (UTF-8)`
    /// (see `encrypt_friend_request_envelope`); `None` when there was no
    /// envelope or it would not open. Everything is best-effort: unopened or
    /// malformed → all-`None` (never surface ciphertext).
    pub async fn ingest_received_friend_request(
        &self,
        sender_id: &UserId,
        plaintext: Option<Vec<u8>>,
    ) -> ReceivedFriendRequestProfile {
        use zeroize::Zeroize;

        let mut out = ReceivedFriendRequestProfile::default();
        let Some(plaintext) = plaintext else {
            return out;
        };
        let (profile_key, note) = match plaintext.split_first() {
            Some((1, rest)) if rest.len() >= 32 => {
                let (key, note) = rest.split_at(32);
                (Some(<[u8; 32]>::try_from(key).expect("32 bytes")), note)
            }
            Some((0, note)) => (None, note), // their visibility setting withheld it
            _ => return out,                 // malformed
        };
        out.note = String::from_utf8(note.to_vec())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(mut profile_key) = profile_key else {
            return out;
        };

        // Persist the sender's key, then decrypt their real name + avatar.
        if self
            .store_friend_request_sender_key(sender_id, &profile_key)
            .is_ok()
        {
            if let Ok(profile) = self.get_user_profile(sender_id).await {
                // Resolve after persistence so a name saved on this device
                // takes precedence over the newly decrypted profile name.
                out.display_name = Some(self.get_display_name(sender_id))
                    .filter(|name| name != &friend_display_name_placeholder(sender_id));
                out.avatar_bytes = profile.avatar_bytes;
            }
        }
        profile_key.zeroize();
        out
    }

    /// Persist a friend-request sender's profile key BEFORE we accept them, so
    /// their real display name + avatar can be decrypted for the Received tab.
    /// Also creates a `Pending` local friendship row keyed `(me, sender)`;
    /// accepting later updates the row's status.
    pub fn store_friend_request_sender_key(
        &self,
        sender: &UserId,
        profile_key: &[u8; 32],
    ) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        self.user_repo.ensure_user_exists(sender)?;

        let pending = Friendship {
            requester_id: current_user,
            addressee_id: *sender,
            status: FriendshipStatus::Pending,
            created_at: Utc::now(),
        };
        // Row may already exist (repeated poll / prior request) — keep whatever
        // status it has.
        let _ = self.user_repo.insert_friendship(&pending);

        self.user_repo
            .set_friend_profile_key(&current_user, sender, profile_key)
            .map(|_| ())
            .map_err(UserError::Storage)
    }

    /// Respond to a friend request.
    pub async fn respond_friend_request(
        &self,
        friend_id: &UserId,
        accept: bool,
    ) -> Result<FriendResponseResult, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let status = if accept {
            FriendStatus::Accepted
        } else {
            FriendStatus::Declined
        };

        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .respond_friend_request(&friend_id_str, status, None)
            .await?;

        // DMs are channel-free (Signal-style) — the server no longer
        // returns a DM channel_id here, just the terminal friend status.
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref status_response)) = event.event {
                let status = status_response.status();
                tracing::debug!("Friend response event: status={:?}", status);

                if status != FriendStatus::Pending {
                    tracing::info!("Friend response final status: {:?}", status);
                    break;
                }
            }
        }

        if accept {
            // Persist locally via the shared helper, which ensures the peer's
            // user row exists first (the friendships FK). A plain insert here
            // FK-failed silently when the accepter had no local user row for
            // the requester (added via friend code), leaving the DM out of the
            // sidebar.
            self.record_accepted_friend_local(friend_id)?;
            // Resolve their name before the caller rebuilds the sidebar from
            // storage, so the new DM row never starts as `User <id8>`
            // (OPEN-ITEMS §119). `get_user_profile` decrypts with the key the
            // request envelope carried and upserts; a miss keeps the stored
            // name. Best-effort: the friendship is already accepted.
            if let Err(e) = self.get_user_profile(friend_id).await {
                tracing::warn!(peer_id = %friend_id, error = %e, "Accept: friend profile fetch failed");
            }
            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: *friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };
            Ok(FriendResponseResult {
                friendship: Some(friendship),
            })
        } else {
            let _ = self.user_repo.delete_friendship(&current_user, friend_id);
            Ok(FriendResponseResult { friendship: None })
        }
    }

    /// List accepted friends.
    pub async fn list_friends(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Friendship>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let response = user_client
            .list_friends(limit as i32, offset as i32)
            .await?;

        let mut friendships = Vec::with_capacity(response.friends.len());
        for friend in &response.friends {
            let friend_id: UserId = friend
                .user_id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid friend user ID".to_string()))?;

            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            friendships.push(friendship);
        }

        Ok(friendships)
    }

    /// List accepted friends with their profile data.
    ///
    /// `ListFriends` supplies membership only: its embedded profiles can be
    /// stale contact-cache snapshots. Resolve each profile through the same
    /// authoritative fetch, persistence and notification path as DM open.
    pub async fn list_friends_with_profiles(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<(Friendship, Profile)>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let response = user_client
            .list_friends(limit as i32, offset as i32)
            .await?;

        let mut friendships = Vec::with_capacity(response.friends.len());
        for friend in &response.friends {
            let friend_id: UserId = friend
                .user_id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid friend user ID".to_string()))?;

            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            // FK on users(id); then record the friendship without lifting a
            // block — the server lists blocked friends too.
            self.user_repo.ensure_user_exists(&friend_id)?;
            self.user_repo
                .accept_friendship_unless_blocked(&current_user, &friend_id)?;

            friendships.push(friendship);
        }

        // Profile fetches make an RPC per friend. Bound their concurrency so
        // opening Contacts does not wait for every network round trip in turn.
        let mut results = Vec::with_capacity(friendships.len());
        for batch in friendships.chunks(8) {
            let profiles = futures::future::join_all(
                batch
                    .iter()
                    .map(|friend| self.get_user_profile(&friend.addressee_id)),
            )
            .await;
            for (friendship, profile) in batch.iter().zip(profiles) {
                results.push((friendship.clone(), profile?));
            }
        }

        Ok(results)
    }

    /// List friend requests with optional filtering.
    pub async fn list_friend_requests(
        &self,
        filter: FriendRequestFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Friendship>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let proto_filter = filter.to_proto();

        let mut user_client = self.user_client();
        let response = user_client
            .list_friend_requests(proto_filter, limit as i32, offset as i32)
            .await?;

        let mut friendships = Vec::with_capacity(response.requests.len());
        for request in &response.requests {
            let friend_id: UserId = request
                .user_id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid request user ID".to_string()))?;

            let status = proto_friend_status_to_domain(request.status);

            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: friend_id,
                status,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            friendships.push(friendship);
        }

        Ok(friendships)
    }

    /// Get friends from local storage only.
    pub fn get_friends_local(&self) -> Result<Vec<Friendship>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        self.user_repo
            .list_friends(&current_user)
            .map_err(UserError::Storage)
    }

    /// Usernames of accepted friends, best effort (local data only).
    /// Feeds the Jarvis app-context bridge.
    pub fn friend_usernames_local(&self) -> Vec<String> {
        let Ok(current) = self.settings_repo.current_user_id() else {
            return Vec::new();
        };
        let Ok(friends) = self.get_friends_local() else {
            return Vec::new();
        };
        friends
            .iter()
            .filter(|f| matches!(f.status, FriendshipStatus::Accepted))
            .filter_map(|f| {
                let other = if f.requester_id == current {
                    f.addressee_id
                } else {
                    f.requester_id
                };
                // A peer's local username can be a placeholder user id; never
                // hand that to Jarvis as a handle.
                self.user_repo
                    .get(&other)
                    .ok()
                    .flatten()
                    .map(|u| u.username)
                    .filter(|u| !u.is_empty() && u.parse::<uuid::Uuid>().is_err())
            })
            .take(20)
            .collect()
    }

    /// Persist an accepted friendship locally with us as the requester.
    ///
    /// The accepter records the friendship inline in `respond_friend_request`,
    /// but the ORIGINAL requester only learns of acceptance asynchronously via
    /// the `FriendAccepted` user-event. Since the channel-free DM sidebar is
    /// built from the local friends list, the requester must write this row or
    /// the peer never appears. Idempotent (update-then-insert).
    pub fn record_accepted_friend_local(&self, friend_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        // The friendships row has a FOREIGN KEY on users(id); the requester may
        // not have a local user row for a peer it only learned about via the
        // friend code. Ensure it exists before inserting (same as
        // list_friends_with_profiles does).
        self.user_repo.ensure_user_exists(friend_id).ok();
        let friendship = Friendship {
            requester_id: current_user,
            addressee_id: *friend_id,
            status: FriendshipStatus::Accepted,
            created_at: Utc::now(),
        };
        if self.user_repo.update_friendship(&friendship).is_err() {
            self.user_repo
                .insert_friendship(&friendship)
                .map_err(UserError::Storage)?;
        }
        Ok(())
    }

    /// Remove a friend.
    pub async fn remove_friend(&self, friend_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        user_client.remove_friend(&friend_id.to_string()).await?;

        let _ = self.user_repo.delete_friendship(&current_user, friend_id);

        Ok(())
    }
}
