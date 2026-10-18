use super::*;

impl ChatterClient {
    pub async fn get_channel_members(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::channel::Membership>, crate::services::channel::ChannelError>
    {
        self.channels()
            .get_channel_members(channel_id, limit, offset)
            .await
    }

    pub async fn update_channel_member_role(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        new_role: crate::domain::channel::MemberRole,
    ) -> Result<crate::domain::channel::Membership, crate::services::channel::ChannelError> {
        self.channels()
            .update_member_role(channel_id, user_id, new_role)
            .await
    }

    pub async fn delete_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().delete_channel(channel_id).await
    }

    /// Keep only invite choices that are selected and currently eligible.
    pub fn eligible_channel_invite_targets<I>(&self, rows: I) -> Vec<crate::domain::ids::UserId>
    where
        I: IntoIterator<Item = (String, bool, bool, bool)>,
    {
        rows.into_iter()
            .filter_map(|(id, selected, already_member, banned)| {
                if selected && !already_member && !banned {
                    id.parse().ok()
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn invite_user_to_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        invitee_id: &crate::domain::ids::UserId,
        message: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels()
            .invite_user(channel_id, invitee_id, message)
            .await
    }

    /// List the current user's pending channel invitations (server scopes to
    /// PENDING + unexpired). Best-effort — empty on failure.
    pub async fn list_pending_channel_invites(
        &self,
    ) -> Vec<crate::proto::chat_v1::PendingChannelInvite> {
        let mut client = self.services.grpc_pool.channel_client();
        match client.list_pending_invites().await {
            Ok(mut resp) => {
                resp.invites.sort_by(|a, b| {
                    a.created_at
                        .cmp(&b.created_at)
                        .then_with(|| a.channel_id.cmp(&b.channel_id))
                });
                resp.invites
            }
            Err(e) => {
                tracing::debug!(error = %e, "list_pending_channel_invites failed");
                Vec::new()
            }
        }
    }

    /// Pending invitations with unknown inviter IDs hidden from the display.
    pub async fn channel_invite_rows(&self) -> Vec<ChannelInviteRow> {
        self.list_pending_channel_invites()
            .await
            .into_iter()
            .map(|invite| {
                let channel_name = if invite.channel_name.is_empty() {
                    "Channel invite".to_string()
                } else {
                    format!("#{}", invite.channel_name)
                };
                let invited_by = invite
                    .inviter_id
                    .parse::<crate::domain::ids::UserId>()
                    .ok()
                    .map(|user_id| self.display_name(&user_id))
                    .filter(|name| *name != invite.inviter_id)
                    .map(|name| format!("Invited by {name}"))
                    .unwrap_or_default();
                ChannelInviteRow {
                    channel_id: invite.channel_id,
                    channel_name,
                    invited_by,
                    created_at: invite.created_at,
                    note: invite.message,
                }
            })
            .collect()
    }

    /// Accept or decline a channel invitation. Returns the channel on accept,
    /// `None` on decline.
    ///
    /// Accepting is a join, so this finishes the join the same way
    /// [`ChatterClient::join_channel`] — the other way into a channel — does.
    /// The service writes the channel and membership rows; on top of that this
    /// nudges the live message stream and pulls the existing members' sender
    /// keys. Without those three the user was a member on the server with
    /// nothing locally: no channel in the sidebar and no live message in it
    /// until the next sync (OPEN-ITEMS §69).
    pub async fn respond_to_channel_invite(
        self: &Arc<Self>,
        channel_id: &crate::domain::ids::ChannelId,
        accept: bool,
    ) -> Result<Option<Channel>, crate::services::channel::ChannelError> {
        let Some(channel) = self
            .channels()
            .respond_to_invite(channel_id, accept)
            .await?
        else {
            return Ok(None);
        };

        // Nudge the stream FIRST, ahead of the slower calls below. A brand-new
        // invitee was in no channel when their client started, so the server
        // refused their global stream and the stream task is sitting in its
        // reconnect backoff with no subscription at all — the membership
        // change the server publishes on accept (§68) reaches nobody. Starting
        // the reconnect here lets its catch-up run while the sender-key work
        // below is still going, rather than inside the window the user is
        // waiting for their first message in.
        self.resubscribe_channel_messages();

        // The SKDM request below asks every member the LOCAL membership list
        // names, so the member list has to be local first.
        if let Err(e) = self.channels().sync_channel_members(channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to sync channel members after accepting an invite"
            );
        }

        // Pull SKDMs from the existing members so their next message decrypts
        // without the no-sender-key defer/retry round-trip.
        if let Err(e) = self
            .messages()
            .request_skdms_from_channel_members(channel_id)
            .await
        {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to request SKDMs after accepting an invite"
            );
        }

        Ok(Some(channel))
    }

    /// The invite picker's search: [`Self::list_channel_invite_candidates`]
    /// narrowed to `query` ([`InviteCandidate::matches`]), plus — when the
    /// query is a complete handle — the account that exact username resolves
    /// to (Signal-style: display names are profile-key-encrypted on the
    /// server, so only an exact username can reach a stranger).
    ///
    /// The lookup waits for typing to pause, so `alice.47` → `alice.471` →
    /// `alice.4711` does not probe every discriminator on the way: after the
    /// pause, `superseded()` is asked and a newer keystroke's search wins
    /// (`None`).
    pub async fn search_channel_invite_candidates(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        query: &str,
        superseded: impl Fn() -> bool,
    ) -> Option<Vec<InviteCandidate>> {
        let mut cands = self
            .list_channel_invite_candidates(channel_id)
            .await
            .unwrap_or_default();
        let trimmed = query.trim();
        cands.retain(|c| c.matches(trimmed));
        if !crate::services::user::is_lookup_handle(trimmed) {
            return Some(cands);
        }

        tokio::time::sleep(Self::INVITE_LOOKUP_PAUSE).await;
        if superseded() {
            return None;
        }
        match self.lookup_by_username(trimmed).await {
            Ok(Some(user_id)) if !cands.iter().any(|c| c.user_id == user_id) => {
                match self.get_user_profile(&user_id).await {
                    Ok(profile) => cands.push(InviteCandidate {
                        user_id,
                        display_name: profile.display_name,
                        username: self
                            .known_username(&user_id)
                            .unwrap_or_else(|| trimmed.to_string()),
                        avatar_bytes: profile.avatar_bytes.unwrap_or_default(),
                        // A non-contact found by exact username; if already a
                        // member or banned, the invite RPC rejects it server-side.
                        already_member: false,
                        banned: false,
                    }),
                    Err(e) => {
                        tracing::warn!(error = %e, %user_id, "lookup hit but profile fetch failed")
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, query = %trimmed, "username lookup failed"),
        }
        Some(cands)
    }

    /// How long typing must pause on a complete handle before it is looked up.
    const INVITE_LOOKUP_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

    /// Build the list of people the user can invite to `channel_id`: their DM
    /// contacts (regular server DMs + anon DMs — the same set shown in the
    /// sidebar), deduped, with `already_member` flagged for current members.
    pub async fn list_channel_invite_candidates(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<Vec<InviteCandidate>, crate::services::channel::ChannelError> {
        use crate::domain::ids::UserId;
        use std::collections::HashSet;

        // Full membership, paginated — an already-joined member past the
        // server's page size would otherwise show up as invitable.
        let members: HashSet<UserId> = self
            .channels()
            .get_all_channel_members(channel_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.user_id)
            .collect();
        // Active bans, visible to owner/admin only (`None` otherwise; the
        // server then refuses a banned invitee with its own reason).
        let banned: HashSet<UserId> = self
            .channels()
            .list_banned_users(channel_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.user_id)
            .collect();

        let mut seen: HashSet<UserId> = HashSet::new();
        let mut out: Vec<InviteCandidate> = Vec::new();

        let push = |out: &mut Vec<InviteCandidate>,
                    seen: &mut HashSet<UserId>,
                    user_id: UserId,
                    display_name: String| {
            if !seen.insert(user_id) {
                return;
            }
            let username = self.known_username(&user_id).unwrap_or_default();
            let avatar_bytes = self
                .get_profile_local(&user_id)
                .and_then(|p| p.avatar_bytes)
                .unwrap_or_default();
            out.push(InviteCandidate {
                already_member: members.contains(&user_id),
                banned: banned.contains(&user_id),
                user_id,
                display_name,
                username,
                avatar_bytes,
            });
        };

        // Regular DM peers = accepted friends (channel-free, keyed by
        // peer user_id). Skip in anon mode — they carry a server identity
        // link the sidebar also suppresses there.
        if !self.anon_mode_enabled() {
            for peer_id in self.listed_friends() {
                let display_name = self.display_name(&peer_id);
                push(&mut out, &mut seen, peer_id, display_name);
            }
        }

        // Anon DM peers.
        for dm in self.list_anon_dm_conversations() {
            let display = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| dm.peer_id.to_string());
            push(&mut out, &mut seen, dm.peer_id, display);
        }

        Ok(out)
    }

    pub async fn search_channels_filtered(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
        type_filter: Vec<crate::domain::channel::ChannelType>,
    ) -> Result<Vec<crate::domain::channel::Channel>, crate::services::channel::ChannelError> {
        self.channels()
            .search_channels(query, limit, offset, type_filter)
            .await
    }
}
