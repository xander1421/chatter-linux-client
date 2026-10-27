use super::*;

impl ChannelEventService {
    /// Handle a member joining a channel.
    ///
    /// Creates a system message and syncs membership to local storage.
    /// Returns (message, display_name) for UI updates, or `None` when this
    /// event's notice is already filed (see [`member_notice_id`]).
    pub fn handle_member_joined(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        membership: &Membership,
        added_by: Option<UserId>,
        event_ts: u64,
    ) -> Result<Option<(Message, String)>, ChannelEventError> {
        // Sync membership (insert or ignore if already exists)
        self.channel_repo.insert_or_ignore_membership(membership)?;

        // Our own join is filed twice over — by the join RPC (`record_own_join`,
        // keyed on joined_at) and by the verified log change (keyed on its own
        // time) — with no shared id: keep the first, as for our own leave.
        let is_me = self.settings_repo.current_user_id().ok() == Some(*user_id);
        if is_me && self.recently_recorded_own_join(channel_id, user_id) {
            return Ok(None);
        }

        // Someone else signed this member in: "{by} added {name}", else a join.
        let (content, sender) = match added_by.filter(|by| by != user_id) {
            Some(by) => {
                let added_name = if is_me { Some("you".to_string()) } else { self.known_name(user_id) };
                (SystemMessage::UserAdded { by, added: *user_id, added_name }, by)
            }
            None => (SystemMessage::UserJoined(*user_id), *user_id),
        };
        let message = self.create_system_message(
            member_notice_id(channel_id, user_id, "joined", event_ts),
            channel_id,
            content,
            Some(sender),
            event_ts,
        );
        if !self.message_repo.insert_or_ignore(&message)? {
            return Ok(None);
        }

        Ok(Some((message, self.get_display_name(user_id))))
    }

    /// [`Self::handle_member_joined`], then tell the UI.
    pub(super) fn publish_member_joined(
        &self,
        membership: &Membership,
        added_by: Option<UserId>,
        event_ts: u64,
    ) {
        let (channel_id, user_id) = (membership.channel_id, membership.user_id);
        let Ok(Some((message, display_name))) =
            self.handle_member_joined(&channel_id, &user_id, membership, added_by, event_ts)
        else {
            return;
        };
        tracing::info!(%channel_id, %user_id, %display_name, "Channel member joined");
        let event = DomainEvent::ChannelMemberJoined {
            channel_id,
            user_id,
            display_name,
            message,
        };
        if let Err(e) = self.event_bus.domain_sender().send(event) {
            tracing::warn!("Failed to publish ChannelMemberJoined event: {}", e);
        }
    }

    /// A roster sync found `membership` on the server but not locally. The
    /// roster is display only: the user is shown, but unless the verified
    /// state has them ACTIVE they are an unsigned add — ignored for keys and
    /// surfaced (`MembershipLog::apply_unsigned`). The notice is keyed on the
    /// server's `joined_at`, so every sync that sees this join files it once.
    pub fn record_synced_join(&self, membership: &Membership) {
        self.classify_unsigned(&membership.channel_id, &membership.user_id, true);
        self.publish_member_joined(membership, None, membership_ts(membership));
    }

    fn classify_unsigned(&self, channel_id: &ChannelId, user_id: &UserId, added: bool) {
        use crate::services::channel::verified_state::UnsignedEvent;
        let user_id = user_id.to_string();
        let event = if added {
            UnsignedEvent::Added { user_id }
        } else {
            UnsignedEvent::Removed { user_id }
        };
        if let Err(e) = self.membership_log.apply_unsigned(channel_id, event) {
            tracing::warn!(%channel_id, error = %e, "Could not apply an unsigned roster change");
        }
    }

    /// A sync found `membership` gone from the server and no signed change
    /// explains it (the sync caught the log up first, and a signed removal
    /// would already have dropped the row). An unsigned removal: they leave
    /// the distribution set at once (fail-safe), stay in the verified state
    /// pending a signed change, and the notice states no reason — the server's
    /// word is not one. Keyed on the membership that ended, so every sync that
    /// sees it files it once.
    pub fn record_synced_departure(&self, membership: &Membership) {
        self.classify_unsigned(&membership.channel_id, &membership.user_id, false);
        self.publish_member_removed(
            &membership.channel_id,
            &membership.user_id,
            MemberRemoval::Unexplained,
            membership_ts(membership),
        );
    }

    /// A roster sync's member list for `channel_id`, reconciled with the
    /// local roster. A change found here is one whose live event we missed.
    /// The signed log is caught up FIRST: a verified removal files its own
    /// notice with the signed reason ("Y removed X", "Y banned X", "X left")
    /// and drops the row, so the diff is left with only what no signed change
    /// explains (F79). A failed catch-up falls back to the unsigned diff.
    /// Returns (added_or_updated, removed).
    pub async fn reconcile_roster(
        &self,
        channel_id: &ChannelId,
        channel_type: crate::domain::channel::ChannelType,
        memberships: &[Membership],
        last_sync: Option<chrono::DateTime<Utc>>,
    ) -> Result<(usize, usize), StorageError> {
        use crate::domain::channel::ChannelType;
        // Saved Messages has no signed membership log (the server never
        // stores a genesis for it); asking for one only earned a NotFound
        // warning on every sync.
        if matches!(channel_type, ChannelType::Public | ChannelType::Private)
            && let Err(e) = self.membership_log.catch_up(channel_id).await
        {
            tracing::warn!(%channel_id, error = %e, "Membership log catch-up failed; roster changes shown without reasons");
        }
        let me = self.settings_repo.current_user_id().ok();
        let own_joined = memberships
            .iter()
            .find(|m| Some(m.user_id) == me)
            .map(|m| m.joined_at);
        let refresh = self
            .channel_repo
            .replace_memberships(channel_id, memberships)?;
        for member in &refresh.removed {
            if Some(member.user_id) != me {
                self.record_synced_departure(member);
            }
        }
        for membership in &refresh.added {
            if Some(membership.user_id) != me
                && join_is_news(membership.joined_at, last_sync, own_joined)
            {
                self.record_synced_join(membership);
            }
        }
        Ok((refresh.added.len(), refresh.removed.len()))
    }

    /// A sync found us (`own`) no longer listed in a channel; `departure` is
    /// the server's `GetDeparture` answer. The log is closed to a former
    /// member, so the server hands over the signed change that removed us:
    /// its reason and author are shown only once it verifies
    /// ([`MembershipLog::verify_own_removal`]). The channel stays listed,
    /// read-only, with its history (Signal keeps an ended group).
    pub async fn record_departure(
        &self,
        own: &Membership,
        departure: &crate::proto::chat_v1::Departure,
    ) -> Result<(), crate::services::channel::membership_log::LogError> {
        use crate::proto::chat_v1::departure::Reason;
        match Reason::try_from(departure.reason) {
            // The listing moved under our paging; nothing was lost.
            Ok(Reason::Member) => {}
            Ok(Reason::ChannelDeleted) => self.record_channel_deleted(&own.channel_id),
            _ => {
                let removal = match &departure.change {
                    Some(change) => {
                        self.membership_log
                            .verify_own_removal(
                                &own.channel_id,
                                change,
                                departure.prev_state.as_ref(),
                            )
                            .await?
                    }
                    None => None,
                };
                tracing::info!(channel_id = %own.channel_id, ?removal, "No longer in a channel the sync missed the removal of");
                self.record_own_departure(own, removal);
            }
        }
        Ok(())
    }

    /// Our removal, as `removal` (a verified signed change) says. The notice
    /// is keyed on the change's timestamp, as the live path keys it, so
    /// hearing both files it once. `None` (no change, or it did not verify)
    /// reads "You are no longer a member", keyed on our membership.
    fn record_own_departure(
        &self,
        own: &Membership,
        removal: Option<crate::services::channel::membership_log::OwnRemoval>,
    ) {
        use crate::proto::chat_v1::remove_member::Reason;
        let author = removal
            .as_ref()
            .and_then(|r| r.author.parse::<uuid::Uuid>().ok())
            .map(UserId::from);
        let (reason, ts) = match (removal, author) {
            (Some(r), Some(by)) => (
                match r.reason {
                    Reason::Kicked => MemberRemoval::Kicked(by),
                    Reason::Banned => MemberRemoval::Banned(by),
                    _ => MemberRemoval::Left,
                },
                r.timestamp,
            ),
            _ => (MemberRemoval::Unexplained, membership_ts(own)),
        };
        self.publish_member_removed(&own.channel_id, &own.user_id, reason, ts);
    }

    /// The channel was deleted, heard live or found by a sync: we are out of
    /// it, but it stays listed read-only with its history and "{owner} deleted
    /// the channel", as Signal keeps an ended group. One notice per channel
    /// however many times it is heard. A channel never stored here is ignored.
    pub fn record_channel_deleted(&self, channel_id: &ChannelId) {
        let (Ok(Some(channel)), Ok(me)) = (
            self.channel_repo.get(channel_id),
            self.settings_repo.current_user_id(),
        ) else {
            return;
        };
        if let Err(e) = self.apply_channel_deleted(&channel, &me) {
            tracing::warn!(%channel_id, error = %e, "Failed to record channel deletion");
        }
    }

    fn apply_channel_deleted(
        &self,
        channel: &crate::domain::channel::Channel,
        me: &UserId,
    ) -> Result<(), ChannelEventError> {
        match self.channel_repo.delete_membership(&channel.id, me) {
            Ok(()) | Err(StorageError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.channel_repo.mark_former(&channel.id, true)?;
        let id = MessageId(uuid::Uuid::new_v5(
            &uuid::Uuid::nil(),
            format!("channel_deleted_notice:{}", channel.id).as_bytes(),
        ));
        let message = self.create_system_message(
            id,
            &channel.id,
            SystemMessage::ChannelDeleted,
            Some(channel.owner_id),
            0,
        );
        if !self.message_repo.insert_or_ignore(&message)? {
            return Ok(());
        }
        tracing::info!(channel_id = %channel.id, "Channel deleted");
        let event = DomainEvent::ChannelMemberLeft {
            channel_id: channel.id,
            user_id: *me,
            display_name: self.get_display_name(me),
            message,
        };
        if let Err(e) = self.event_bus.domain_sender().send(event) {
            tracing::warn!("Failed to publish channel deletion: {}", e);
        }
        Ok(())
    }

    /// The owner handed the channel to `to` (a verified
    /// `TransferOwnershipAction`): record the new owner and roles and file
    /// "{from} made {to} the owner" once per change.
    pub(super) fn record_ownership_transferred(
        &self,
        channel_id: &ChannelId,
        from: &UserId,
        to: &UserId,
        event_ts: u64,
    ) {
        if let Err(e) = crate::services::channel::apply_ownership_transfer(
            &self.channel_repo,
            channel_id,
            from,
            to,
        ) {
            tracing::warn!(%channel_id, error = %e, "Failed to record ownership transfer");
            return;
        }
        let content = if self.settings_repo.current_user_id().ok() == Some(*to) {
            SystemMessage::MadeYouOwner
        } else {
            SystemMessage::OwnershipTransferred {
                to: *to,
                to_name: self.known_name(to),
            }
        };
        let id = member_notice_id(channel_id, to, "owner", event_ts);
        let message = self.create_system_message(id, channel_id, content, Some(*from), event_ts);
        match self.message_repo.insert_or_ignore(&message) {
            Ok(true) => {
                if let Err(e) = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::MessageReceived { message })
                {
                    tracing::warn!("Failed to publish ownership notice: {}", e);
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(%channel_id, error = %e, "Failed to file ownership notice"),
        }
    }

    /// File "X renamed the channel from A to B", once however many paths
    /// deliver the rename: the renamer's RPC reply and the stream fan-out
    /// race, and carry different timestamps, so the id is keyed on the
    /// rename itself. `event_ts` 0 (this device's own rename) files it now.
    // keyed without time: renaming A→B again later files no second notice.
    pub fn record_channel_renamed(
        &self,
        channel_id: &ChannelId,
        actor: UserId,
        old_name: String,
        new_name: String,
        event_ts: u64,
    ) {
        let key = format!("channel_rename_notice:{channel_id}:{actor}:{old_name}\0{new_name}");
        let id = MessageId(uuid::Uuid::new_v5(&uuid::Uuid::nil(), key.as_bytes()));
        let content = SystemMessage::ChannelRenamed { old_name, new_name };
        let message = self.create_system_message(id, channel_id, content, Some(actor), event_ts);
        match self.message_repo.insert_or_ignore(&message) {
            Ok(true) => {
                if let Err(e) = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::MessageReceived { message })
                {
                    tracing::warn!("Failed to publish rename notice: {}", e);
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(%channel_id, error = %e, "Failed to file rename notice"),
        }
    }

    /// File "{actor} changed the channel photo". Keyed on the new icon's
    /// bytes, as a rename notice is on its names: our own change files it from
    /// the RPC and again from the stream echo, which can land first.
    pub fn record_channel_photo_changed(
        &self,
        channel_id: &ChannelId,
        actor: UserId,
        icon: &[u8],
        event_ts: u64,
    ) {
        let mut key = format!("channel_photo_notice:{channel_id}:{actor}:").into_bytes();
        key.extend_from_slice(icon);
        let id = MessageId(uuid::Uuid::new_v5(&uuid::Uuid::nil(), &key));
        let message = self.create_system_message(
            id,
            channel_id,
            SystemMessage::ChannelPhotoChanged,
            Some(actor),
            event_ts,
        );
        match self.message_repo.insert_or_ignore(&message) {
            Ok(true) => {
                if let Err(e) = self
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::MessageReceived { message })
                {
                    tracing::warn!("Failed to publish photo notice: {}", e);
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(%channel_id, error = %e, "Failed to file photo notice"),
        }
    }

    /// Handle a member leaving/being removed from a channel.
    ///
    /// Creates a system message and removes membership from local storage.
    /// Returns (message, display_name) for UI updates, or `None` when this
    /// event's notice is already filed (see [`member_notice_id`]).
    pub fn handle_member_removed(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: MemberRemoval,
        event_ts: u64,
    ) -> Result<Option<(Message, String)>, ChannelEventError> {
        let display_name = self.get_display_name(user_id);
        let is_self = self.settings_repo.current_user_id().ok() == Some(*user_id);

        // `sender_id` is the subject of the rendered "{sender} {description}"
        // line: the leaver for a leave, the moderator for a removal.
        let (content, subject) = match reason {
            MemberRemoval::Left => (SystemMessage::UserLeft(*user_id), *user_id),
            MemberRemoval::Kicked(by) if is_self => (SystemMessage::SelfRemoved { by }, by),
            MemberRemoval::Banned(by) if is_self => (SystemMessage::SelfBanned { by }, by),
            MemberRemoval::Unexplained if is_self => (SystemMessage::NoLongerMember, *user_id),
            MemberRemoval::Unexplained => (SystemMessage::UserGone(*user_id), *user_id),
            MemberRemoval::Kicked(by) => (
                SystemMessage::UserRemoved {
                    by,
                    removed: *user_id,
                    removed_name: self.known_name(user_id),
                },
                by,
            ),
            MemberRemoval::Banned(by) => (
                SystemMessage::UserBanned {
                    by,
                    banned: *user_id,
                    banned_name: self.known_name(user_id),
                },
                by,
            ),
        };
        let id = member_notice_id(channel_id, user_id, "removed", event_ts);
        let message = self.create_system_message(id, channel_id, content, Some(subject), event_ts);

        // Remove membership from local storage. Already gone is fine: our own
        // leave deletes it first, and a roster never fetched has no row.
        match self.channel_repo.delete_membership(channel_id, user_id) {
            Ok(()) | Err(StorageError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        // Ours: the channel stays listed, read-only, as Signal keeps a group
        // you left or were removed from.
        if is_self {
            self.channel_repo.mark_former(channel_id, false)?;
        }

        // Our own leave reaches us twice with unrelated timestamps: the leave
        // RPC (`record_own_leave`) and the server's broadcast on the channel
        // stream we are still subscribed to. Neither can key off the other.
        if is_self
            && reason == MemberRemoval::Left
            && self.recently_recorded_own_leave(channel_id, user_id)
        {
            return Ok(None);
        }

        if !self.message_repo.insert_or_ignore(&message)? {
            return Ok(None);
        }

        Ok(Some((message, display_name)))
    }

    /// [`Self::handle_member_removed`], then tell the UI, which appends the
    /// notice to the open chat.
    pub(super) fn publish_member_removed(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        reason: MemberRemoval,
        event_ts: u64,
    ) {
        let Ok(Some((message, display_name))) =
            self.handle_member_removed(channel_id, user_id, reason, event_ts)
        else {
            return;
        };
        tracing::info!(%channel_id, %user_id, %display_name, "Channel member left");
        let event = DomainEvent::ChannelMemberLeft {
            channel_id: *channel_id,
            user_id: *user_id,
            display_name,
            message,
        };
        if let Err(e) = self.event_bus.domain_sender().send(event) {
            tracing::warn!("Failed to publish ChannelMemberLeft event: {}", e);
        }
    }

    /// File "You created the channel" in a channel we just created, as
    /// Signal's "You created the group". Only the creator's device files it;
    /// members who join later see their own join instead.
    pub fn record_own_create(&self, channel_id: &ChannelId) {
        let Ok(me) = self.settings_repo.current_user_id() else {
            tracing::warn!(%channel_id, "No current user; create notice not filed");
            return;
        };
        let message = self.create_system_message(
            MessageId::new(),
            channel_id,
            SystemMessage::ChannelCreated,
            Some(me),
            0,
        );
        if let Err(e) = self.message_repo.insert_or_ignore(&message) {
            tracing::warn!(%channel_id, error = %e, "Create notice not filed");
        }
    }

    /// File "You joined the channel" after our own join succeeded. This device
    /// subscribes to the channel only after joining, so it misses the server's
    /// broadcast, and the first roster sync backfills without announcing.
    /// Keyed on the server's `joined_at`, as a roster-found join is.
    pub fn record_own_join(&self, channel_id: &ChannelId) {
        let membership = self
            .settings_repo
            .current_user_id()
            .ok()
            .and_then(|me| self.channel_repo.get_membership(channel_id, &me).ok().flatten());
        match membership {
            Some(membership) => self.publish_member_joined(&membership, None, membership_ts(&membership)),
            None => tracing::warn!(%channel_id, "No own membership; join notice not filed"),
        }
    }

    /// File "You left the channel" after our own leave succeeded. The server
    /// only broadcasts the departure to the members who remain, so this
    /// device never hears it on the event stream.
    pub fn record_own_leave(&self, channel_id: &ChannelId) {
        match self.settings_repo.current_user_id() {
            Ok(me) => self.publish_member_removed(channel_id, &me, MemberRemoval::Left, 0),
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "No current user; leave notice not filed")
            }
        }
    }

    /// Whether our newest membership notice in `channel_id` is a join (or an
    /// "added you") filed in the last two minutes.
    fn recently_recorded_own_join(&self, channel_id: &ChannelId, me: &UserId) -> bool {
        let conv_type = crate::services::message::conversion::conversation_type_for_channel(
            &self.channel_repo,
            channel_id,
        );
        let Ok(recent) = self.message_repo.list(channel_id, &conv_type, None, 20) else {
            return false;
        };
        let newest_own = recent.iter().find_map(|m| match &m.content {
            MessageContent::System(SystemMessage::UserJoined(u)) if u == me => Some(Some(m.created_at)),
            MessageContent::System(SystemMessage::UserAdded { added, .. }) if added == me => {
                Some(Some(m.created_at))
            }
            MessageContent::System(SystemMessage::UserLeft(u)) if u == me => Some(None),
            _ => None,
        });
        matches!(newest_own, Some(Some(at)) if Utc::now() - at < chrono::Duration::minutes(2))
    }

    /// Whether our newest membership notice in `channel_id` is a leave filed
    /// in the last two minutes: then a second own-leave is the same one.
    // time window, because the RPC and the broadcast carry no shared
    // id; a server-issued departure id on both would make this exact.
    fn recently_recorded_own_leave(&self, channel_id: &ChannelId, me: &UserId) -> bool {
        let conv_type = crate::services::message::conversion::conversation_type_for_channel(
            &self.channel_repo,
            channel_id,
        );
        let Ok(recent) = self.message_repo.list(channel_id, &conv_type, None, 20) else {
            return false;
        };
        let newest_own = recent.iter().find_map(|m| match &m.content {
            MessageContent::System(SystemMessage::UserJoined(u)) if u == me => Some(None),
            MessageContent::System(SystemMessage::UserLeft(u)) if u == me => {
                Some(Some(m.created_at))
            }
            _ => None,
        });
        matches!(newest_own, Some(Some(at)) if Utc::now() - at < chrono::Duration::minutes(2))
    }

    /// Get display name for a user, falling back to user ID string.
    fn get_display_name(&self, user_id: &UserId) -> String {
        match self.user_repo.get(user_id) {
            Ok(Some(user)) => user.username,
            Ok(None) => user_id.to_string(),
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    error = %e,
                    "Failed to get user for display name, falling back to ID"
                );
                user_id.to_string()
            }
        }
    }

    /// A real name for `user_id` from the local stores — profile display name,
    /// then a handle — in `UserService::get_display_name`'s order. `None` when
    /// only the id-placeholder username is known, so the caller can word the
    /// notice generically instead of printing a UUID.
    fn known_name(&self, user_id: &UserId) -> Option<String> {
        if let Ok(Some(profile)) = self.user_repo.get_profile(user_id) {
            if !profile.display_name.is_empty() {
                return Some(profile.display_name);
            }
        }
        self.user_repo
            .get(user_id)
            .ok()
            .flatten()
            .map(|user| user.username)
            .filter(|name| !name.is_empty() && *name != user_id.to_string())
    }

    /// Create a system message for the given channel.
    fn create_system_message(
        &self,
        id: MessageId,
        channel_id: &ChannelId,
        content: SystemMessage,
        subject: Option<UserId>,
        event_ts: u64,
    ) -> Message {
        // Membership logs and roster catch-up carry the occurrence time in
        // nanoseconds. Filing a missed notice at sync time would put it after
        // newer chat messages and make it the sidebar's latest preview.
        let occurred_at = i64::try_from(event_ts)
            .ok()
            .filter(|ts| *ts > 0)
            .map(chrono::DateTime::from_timestamp_nanos)
            .unwrap_or_else(Utc::now);
        // `sender_id` drives the UI's name lookup — the converter formats
        // system messages as `"{sender_name} {description}"`, e.g.
        // "Alice joined the channel". Set it to the user the event is
        // about (joiner / leaver / kicked) so `get_display_name` can
        // resolve a real name instead of falling back to "Anonymous".
        Message {
            id,
            conversation_id: *channel_id,
            // The channel's own bucket, as its messages are filed: anything
            // else is missing from its history and sidebar preview.
            conversation_type: crate::services::message::conversion::conversation_type_for_channel(
                &self.channel_repo,
                channel_id,
            ),
            sender_id: subject,
            device_id: None,
            content: MessageContent::System(content),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at: occurred_at.timestamp_nanos_opt().unwrap_or(0) as u64,
            created_at: occurred_at,
            updated_at: occurred_at,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
            failure_reason: None,
        }
    }
}

/// The member notice's primary key, derived from the server event it records.
///
/// The server publishes one membership `ChannelEvent` to the channel's stream
/// AND to every member's `user:{id}:channel_events`, and the global stream is
/// subscribed to both, so a member receives the same event twice (same bytes,
/// same `timestamp`). With a deterministic id `insert_or_ignore` files it once
/// however many paths deliver it. A zero timestamp carries no identity, so it
/// gets a fresh id rather than colliding with every other such event.
fn member_notice_id(
    channel_id: &ChannelId,
    user_id: &UserId,
    kind: &str,
    event_ts: u64,
) -> MessageId {
    if event_ts == 0 {
        return MessageId::new();
    }
    let key = format!("channel_member_notice:{channel_id}:{user_id}:{kind}:{event_ts}");
    MessageId(uuid::Uuid::new_v5(&uuid::Uuid::nil(), key.as_bytes()))
}

/// Whether a member the sync is adding locally joined while we weren't
/// looking: after the last sync and after our own join. A first sync, or a
/// channel we just joined, backfills its roster without announcing anyone.
// compares the server's joined_at with this device's clock; a skewed
// clock can drop or add an announcement near the boundary.
pub(super) fn join_is_news(
    joined_at: chrono::DateTime<Utc>,
    last_sync: Option<chrono::DateTime<Utc>>,
    own_joined: Option<chrono::DateTime<Utc>>,
) -> bool {
    matches!((last_sync, own_joined), (Some(s), Some(o)) if joined_at > s && joined_at > o)
}

/// A membership's server `joined_at` in nanoseconds: the identity of a join
/// or departure a roster sync found, standing in for the missed event's time.
fn membership_ts(membership: &Membership) -> u64 {
    membership.joined_at.timestamp_nanos_opt().unwrap_or(0) as u64
}
