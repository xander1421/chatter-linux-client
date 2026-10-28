//! Channel CRUD and membership operations.

use std::sync::Arc;

use super::ChatterClient;
use crate::domain::channel::{Channel, ChannelType};

/// Complete or partial channel roster returned by the core paging policy.
#[derive(Debug)]
pub struct ChannelMemberLists {
    pub members: Vec<crate::domain::channel::MemberWithProfile>,
    pub bans: Option<Vec<crate::domain::channel::BannedMember>>,
    pub complete: bool,
    pub failure_stage: Option<ChannelMemberFailure>,
}

#[derive(Debug)]
pub enum ChannelMemberFailure {
    Members(String),
    MembersPartial(String),
    MembersLimit,
    Bans(String),
}

fn sort_channel_member_roster(members: &mut [crate::domain::channel::MemberWithProfile]) {
    use crate::domain::channel::MemberRole;
    let rank = |role| match role {
        MemberRole::Owner => 0,
        MemberRole::Admin => 1,
        MemberRole::Moderator => 2,
        MemberRole::Member => 3,
    };
    members.sort_by(|a, b| {
        rank(a.membership.role)
            .cmp(&rank(b.membership.role))
            .then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
            .then_with(|| a.membership.user_id.to_string().cmp(&b.membership.user_id.to_string()))
    });
}

/// User-facing wording for a channel lifecycle event.
pub enum ChannelEventNotice<'a> {
    Moderation { action: &'a str, reason: Option<&'a str> },
    OperationFailed { code: &'a str, message: &'a str },
}

impl ChatterClient {
    pub fn channel_event_notice(event: ChannelEventNotice<'_>) -> String {
        match event {
            ChannelEventNotice::Moderation { action, reason } => match (action, reason) {
                ("BAN", Some(r)) => format!("You were banned from this channel: {r}"),
                ("BAN", None) => "You were banned from this channel.".into(),
                ("KICK", Some(r)) => format!("You were removed from this channel: {r}"),
                ("KICK", None) => "You were removed from this channel.".into(),
                ("MUTE", _) => "You were muted in this channel.".into(),
                ("UNMUTE", _) => "You were unmuted in this channel.".into(),
                ("UNBAN", _) => "Your ban was lifted.".into(),
                _ => "A moderation action was applied to your account.".into(),
            },
            ChannelEventNotice::OperationFailed { code, message } => {
                if message.is_empty() { format!("Channel operation failed ({code}).") }
                else { format!("Channel operation failed: {message}") }
            }
        }
    }

    /// Fetch all roster pages, preserving a partial roster when a later page fails.
    /// Bans are fetched after members because the member fetch refreshes our role.
    pub async fn channel_member_lists(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> ChannelMemberLists {
        const PAGE: u32 = 100;
        const MAX_PAGES: u32 = 100;
        let mut members = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut page = 0;
        loop {
            let rows = match self
                .list_members_with_profiles(channel_id, PAGE, page * PAGE)
                .await
            {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(%error, page, "Failed to list channel members");
                    sort_channel_member_roster(&mut members);
                    let failure_stage = if members.is_empty() {
                        ChannelMemberFailure::Members(error.to_string())
                    } else {
                        ChannelMemberFailure::MembersPartial(error.to_string())
                    };
                    return ChannelMemberLists {
                        members,
                        bans: None,
                        complete: false,
                        failure_stage: Some(failure_stage),
                    };
                }
            };
            let last = (rows.len() as u32) < PAGE;
            members.extend(
                rows.into_iter()
                    .filter(|member| seen.insert(member.membership.user_id)),
            );
            page += 1;
            if last {
                break;
            }
            if page == MAX_PAGES {
                sort_channel_member_roster(&mut members);
                return ChannelMemberLists {
                    members,
                    bans: None,
                    complete: false,
                    failure_stage: Some(ChannelMemberFailure::MembersLimit),
                };
            }
        }
        sort_channel_member_roster(&mut members);
        match self.list_banned_members(channel_id).await {
            Ok(bans) => ChannelMemberLists {
                members,
                bans,
                complete: true,
                failure_stage: None,
            },
            Err(error) => {
                tracing::warn!(%error, "Failed to list channel bans");
                ChannelMemberLists {
                    members,
                    bans: None,
                    complete: true,
                    failure_stage: Some(ChannelMemberFailure::Bans(error.to_string())),
                }
            }
        }
    }

    // ── Conversation listing ────────────────────────────────────────

    // ── Channel operations ────────────────────────────────────────

    /// Tell the live channel-message stream that this device's channel
    /// membership changed, so it reconnects and the server re-runs its
    /// membership-based auto-subscribe.
    ///
    /// Replaces the old per-channel `subscribe_channel_messages`, which
    /// spawned one `StreamMessageEvents` task per channel from exactly two
    /// call sites, never re-subscribed at login, and returned for good on the
    /// first stream error (INFRA-1). Delivery now lives in
    /// `services::channel_stream::ChannelStreamService`.
    pub fn resubscribe_channel_messages(self: &Arc<Self>) {
        self.services.channel_stream_service.request_resubscribe();
    }

    /// Create a channel and subscribe to its message stream.
    pub async fn create_channel(
        self: &Arc<Self>,
        name: &str,
        channel_type: ChannelType,
        config: Option<crate::domain::channel::ChannelConfig>,
        description: Option<&str>,
        icon: Option<&[u8]>,
        password: Option<&str>,
        initial_members: Vec<crate::domain::ids::UserId>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let channel = self
            .channels()
            .create_channel(
                name,
                channel_type,
                config,
                description,
                icon,
                password,
                initial_members,
            )
            .await?;
        self.services.channel_event_service.record_own_create(&channel.id);
        // The global message stream subscribes from the server's membership
        // list; nudge it so the channel we just created is picked up now
        // rather than at the next reconnect.
        self.resubscribe_channel_messages();
        Ok(channel)
    }

    /// Create a channel, reading the icon file from disk if one is
    /// provided. UI may pass `icon_path` and let core handle the read +
    /// size validation. The bytes are sent inline in the channel proto
    /// — server stores them plaintext alongside the channel name.
    ///
    /// If the icon read fails or the file exceeds the 64KB cap, the
    /// channel is not created.
    pub async fn create_channel_with_icon(
        self: &Arc<Self>,
        name: &str,
        channel_type: ChannelType,
        config: Option<crate::domain::channel::ChannelConfig>,
        description: Option<&str>,
        icon_path: Option<&std::path::Path>,
        password: Option<&str>,
        initial_members: Vec<crate::domain::ids::UserId>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let icon_bytes: Option<Vec<u8>> = match icon_path {
            Some(path) => match tokio::fs::read(path).await {
                Ok(bytes) => Some(Self::fit_icon_bytes(&bytes)?),
                Err(e) => {
                    return Err(crate::services::channel::ChannelError::InvalidInput(
                        format!("icon read failed: {}", e),
                    ));
                }
            },
            None => None,
        };
        self.create_channel(
            name,
            channel_type,
            config,
            description,
            icon_bytes.as_deref(),
            password,
            initial_members,
        )
        .await
    }

    /// Join a channel and request sender keys from its existing members.
    pub async fn join_channel(
        self: &Arc<Self>,
        channel_id: &crate::domain::ids::ChannelId,
        password: Option<&str>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let channel = self.channels().join_channel(channel_id, password).await?;
        self.services.channel_event_service.record_own_join(channel_id);
        self.resubscribe_channel_messages();

        // Pull SKDMs from existing members so we can decrypt their next
        // messages without the no-sender-key defer/retry round-trip.
        if let Err(e) = self
            .messages()
            .request_skdms_from_channel_members(channel_id)
            .await
        {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to request SKDMs on join"
            );
        }

        Ok(channel)
    }

    /// Kick a user from a channel and re-distribute sender keys.
    ///
    /// After kicking, a new SKDM is distributed so the kicked user
    /// can no longer decrypt future messages. Message sending is blocked
    /// during this operation and unblocked only after SKDM distribution.
    pub async fn kick_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels()
            .kick_user_complete(channel_id, user_id, reason)
            .await
    }

    /// Ban a user from a channel (server-side) and re-distribute sender keys.
    ///
    /// Like kick, ban removes local membership and rotates the sender key.
    /// Permanent until [`Self::unban_user`] (no UI offers a timed ban).
    pub async fn ban_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels()
            .ban_user_complete(channel_id, user_id, reason)
            .await
    }

    /// Unban a user — clears the server-side ban. The user can rejoin via
    /// invite or public-channel flow afterwards.
    pub async fn unban_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().unban_user(channel_id, user_id).await
    }

    /// Make `new_owner` the channel's owner; we become an admin and may then
    /// leave. Only the owner may (the server checks it too).
    pub async fn transfer_channel_ownership(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        new_owner: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels()
            .transfer_ownership(channel_id, new_owner)
            .await
    }

    /// Mute a member for `duration`, or until unmuted when `None`.
    /// Server-enforced: their sends are refused until it is lifted or lapses.
    pub async fn mute_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        duration: Option<std::time::Duration>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels()
            .mute_user(channel_id, user_id, duration)
            .await
    }

    /// Lift a mute.
    pub async fn unmute_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().unmute_user(channel_id, user_id).await
    }

    /// The notice that replaces the composer when we may not post in this
    /// channel, or "" when we may. Read from our stored membership, which a
    /// removal event deletes and a member fetch refreshes.
    pub fn channel_send_notice(&self, channel_id: &crate::domain::ids::ChannelId) -> String {
        if let Some(gate) = self.secret_chat_gate(&crate::domain::anon_dm::AnonDmId::from_uuid(
            *channel_id.as_uuid(),
        )) {
            return gate.notice().to_string();
        }
        if !self.is_moderated_channel(channel_id) {
            return String::new();
        }
        let Some(me) = self.session_current_user_id() else {
            return String::new();
        };
        let members = self.list_channel_members_local(channel_id);
        let own = members.iter().find(|m| m.user_id == me);
        crate::domain::channel::SendBlock::for_viewer(
            own,
            !members.is_empty(),
            self.former_channel_state(channel_id),
        )
        .map(|b| b.notice())
        .unwrap_or_default()
    }

    /// `Some(deleted)` for a channel we are no longer in (left, removed,
    /// banned, deleted), which stays listed read-only; `None` otherwise.
    fn former_channel_state(&self, channel_id: &crate::domain::ids::ChannelId) -> Option<bool> {
        self.channels()
            .channel_repo
            .former_state(channel_id)
            .ok()
            .flatten()
    }

    /// A channel we are no longer in, listed read-only: its row offers
    /// "Delete chat" (a live channel is left, not deleted — a membership
    /// outlives a local delete, the next sync brings it back) and it is no
    /// forward destination.
    /// Whether a channel we are no longer in may be joined again from its
    /// chat: a public channel that still exists (Telegram's "Join" bar). The
    /// server still decides — a ban refuses the join.
    pub fn can_rejoin_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        self.is_former_channel(channel_id)
            && self.former_channel_state(channel_id) == Some(false)
            && self.get_channel_local(channel_id).is_some_and(|c| {
                c.channel_type == crate::domain::channel::ChannelType::Public
            })
    }

    /// Whether this device holds our membership of the channel: a search hit
    /// for it opens the chat instead of joining again.
    pub fn is_channel_member(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        let Some(me) = self.session_current_user_id() else {
            return false;
        };
        self.channels()
            .channel_repo
            .get_membership(channel_id, &me)
            .ok()
            .flatten()
            .is_some()
    }

    pub fn is_former_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        let Some(me) = self.session_current_user_id() else {
            return false;
        };
        self.former_channel_state(channel_id).is_some()
            && self
                .channels()
                .channel_repo
                .get_membership(channel_id, &me)
                .ok()
                .flatten()
                .is_none()
    }

    /// Whether to offer "Leave channel": we are still a member and not the
    /// owner (see [`crate::domain::channel::Channel::can_user_leave`]). An
    /// unsynced roster counts as membership, as in [`Self::channel_send_notice`].
    pub fn can_leave_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        let (Some(channel), Some(me)) = (
            self.get_channel_local(channel_id),
            self.session_current_user_id(),
        ) else {
            return false;
        };
        let members = self.list_channel_members_local(channel_id);
        let own = members.iter().find(|m| m.user_id == me);
        let still_member = !matches!(
            crate::domain::channel::SendBlock::for_viewer(
                own,
                !members.is_empty(),
                self.former_channel_state(channel_id)
            ),
            Some(
                crate::domain::channel::SendBlock::Removed
                    | crate::domain::channel::SendBlock::Deleted
            )
        );
        channel.can_user_leave(&me, still_member)
    }

    /// [`Self::channel_send_notice`] after re-reading the roster from the
    /// server: on channel open and after a refused send, in case a live
    /// `MemberUpdated` was missed. A failed fetch keeps the local
    /// answer.
    // refetches the full roster; a "my membership" RPC if that
    // shows up in large channels.
    pub async fn refresh_channel_send_notice(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> String {
        // A secret chat's notice is local state, and a channel we left has no
        // roster we may read (the server refuses it): nothing to refresh.
        if !self.is_moderated_channel(channel_id) || self.is_former_channel(channel_id) {
            return self.channel_send_notice(channel_id);
        }
        if let Err(e) = self.channels().get_all_channel_members(channel_id).await {
            tracing::debug!(%channel_id, error = %e, "Roster refresh for send notice failed");
        }
        self.channel_send_notice(channel_id)
    }

    /// Saved Messages has no moderation (the server refuses it).
    fn is_moderated_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        self.get_channel_local(channel_id)
            .is_some_and(|c| !matches!(c.channel_type, ChannelType::SavedMessages))
    }

    /// A channel's active bans shaped for display, or `None` when our role
    /// may not see them (only owner/admin can unban). Call after a member
    /// fetch so our local role is current.
    pub async fn list_banned_members(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<
        Option<Vec<crate::domain::channel::BannedMember>>,
        crate::services::channel::ChannelError,
    > {
        let Some(bans) = self.channels().list_banned_users(channel_id).await? else {
            return Ok(None);
        };
        let names = self.users().get_display_names(
            &bans.iter().map(|ban| ban.user_id).collect::<Vec<_>>(),
        );
        Ok(Some(
            bans.into_iter()
                .map(|b| crate::domain::channel::BannedMember {
                    display_name: names.get(&b.user_id).cloned().unwrap_or_default(),
                    avatar_bytes: self
                        .get_profile_local(&b.user_id)
                        .and_then(|p| p.avatar_bytes),
                    ..b
                })
                .collect(),
        ))
    }

    /// Update a channel's name, description and/or icon.
    ///
    /// `icon` is raw image bytes; oversized images are auto-resized and
    /// re-encoded via `fit_icon_bytes` to the 64KB inline-proto cap (same
    /// path as channel creation). `None` leaves the current icon unchanged.
    pub async fn update_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        name: Option<&str>,
        description: Option<&str>,
        icon: Option<&[u8]>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let fitted = icon.map(Self::fit_icon_bytes).transpose()?;
        let old = self.get_channel_local(channel_id);
        let old_name = old.as_ref().map(|c| c.name.clone());
        let old_icon = old.and_then(|c| c.icon).unwrap_or_default();
        let updated = self
            .channels()
            .update_channel(channel_id, name, description, fitted.as_deref(), None)
            .await?;
        // Our own rename or photo: the stream echo may find them already merged
        // and file nothing, so file the notices here too (keyed, so an echo
        // that lands first is not doubled).
        if let (Some(icon), Some(me)) = (fitted.as_deref(), self.session_current_user_id()) {
            if !icon.is_empty() && icon != old_icon.as_slice() {
                self.services
                    .channel_event_service
                    .record_channel_photo_changed(channel_id, me, icon, 0);
            }
        }
        if let (Some(old_name), Some(me)) = (old_name, self.session_current_user_id()) {
            if !old_name.is_empty() && old_name != updated.name {
                self.services.channel_event_service.record_channel_renamed(
                    channel_id,
                    me,
                    old_name,
                    updated.name.clone(),
                    0,
                );
            }
        }
        Ok(updated)
    }

    /// Archive or unarchive a channel.
    pub async fn archive_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        archive: bool,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        if archive {
            self.channels().archive_channel(channel_id).await
        } else {
            self.channels().unarchive_channel(channel_id).await
        }
    }

    /// Leave a channel.
    ///
    /// Before leaving, rotates the sender key and distributes the new SKDM
    /// to remaining members. This ensures forward secrecy — the departed
    /// member can no longer decrypt future messages.
    pub async fn leave_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().leave_channel_complete(channel_id).await?;
        self.services
            .channel_event_service
            .record_own_leave(channel_id);
        Ok(())
    }

    /// Search public channels.
    pub async fn search_channels(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels()
            .search_channels(query, limit, 0, vec![ChannelType::Public])
            .await
    }

    const MAX_ICON_BYTES: usize = 65_536;

    fn fit_icon_bytes(raw: &[u8]) -> Result<Vec<u8>, crate::services::channel::ChannelError> {
        if raw.len() <= Self::MAX_ICON_BYTES {
            return Ok(raw.to_vec());
        }

        let img = image::load_from_memory(raw).map_err(|e| {
            crate::services::channel::ChannelError::InvalidInput(format!("icon decode failed: {e}"))
        })?;

        let mut img = img;
        for max_dim in [256u32, 192, 128, 96, 64] {
            if img.width() > max_dim || img.height() > max_dim {
                img = img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3);
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::WebP)
                .map_err(|e| {
                    crate::services::channel::ChannelError::InvalidInput(format!(
                        "icon encode failed: {e}"
                    ))
                })?;
            let encoded = buf.into_inner();
            if encoded.len() <= Self::MAX_ICON_BYTES {
                tracing::info!(
                    original_bytes = raw.len(),
                    final_bytes = encoded.len(),
                    final_dim = max_dim,
                    "auto-converted icon to WebP"
                );
                return Ok(encoded);
            }
        }

        Err(crate::services::channel::ChannelError::InvalidInput(
            "icon still too large after conversion to 64×64 WebP".into(),
        ))
    }
}
