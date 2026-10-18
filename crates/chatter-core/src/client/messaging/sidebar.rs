use super::*;

impl ChatterClient {
    pub fn get_channel_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Option<crate::domain::channel::Channel> {
        self.channels().get_channel_local(channel_id).ok().flatten()
    }

    pub async fn list_channels_paged(
        &self,
        limit: u32,
        offset: u32,
        archived: Option<bool>,
    ) -> Result<Vec<crate::domain::channel::Channel>, crate::services::channel::ChannelError> {
        self.channels().list_channels(limit, offset, archived).await
    }

    pub fn get_channels_mru(&self) -> Vec<crate::domain::channel::Channel> {
        self.channels().get_channels_mru().unwrap_or_default()
    }

    pub fn list_channel_members_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Vec<crate::domain::channel::Membership> {
        self.channels()
            .list_members_local(channel_id)
            .unwrap_or_default()
    }

    pub async fn get_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<crate::domain::channel::Channel, crate::services::channel::ChannelError> {
        self.channels().get_channel(channel_id).await
    }

    /// The newest locally-stored message in a conversation, if it has one.
    pub fn latest_message(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Option<crate::domain::message::Message> {
        // Bound the work even if a chat contains only timer-update notices.
        const PAGE: u32 = 32;
        const MAX_PAGES: usize = 8;
        let mut before: Option<crate::domain::ids::MessageId> = None;
        for _ in 0..MAX_PAGES {
            let messages = self
                .get_messages_local(conversation_id, conversation_type, before.as_ref(), PAGE)
                .ok()?;
            if let Some(message) = messages
                .iter()
                .find(|message| !crate::services::disappearing::is_timer_notice(message))
            {
                return Some(message.clone());
            }
            if messages.len() < PAGE as usize {
                return None;
            }
            let next = messages.last()?.id;
            if before == Some(next) {
                return None;
            }
            before = Some(next);
        }
        None
    }

    /// Typed label for a message's attachments — "🎤 Voice message",
    /// "🎬 Video +2", "📄 report.pdf" (see `FileMetadata::preview_label`).
    /// `None` when the message carries no attachment or its metadata has not
    /// landed locally yet; the caller then falls back to a plain count.
    fn attachment_preview_label(
        &self,
        message: &crate::domain::message::Message,
    ) -> Option<String> {
        let label = self
            .get_attachment_metadata(message.attachments.first()?)?
            .metadata
            .preview_label();
        let extra = message.attachments.len() - 1;
        Some(if extra > 0 {
            format!("{} +{}", label, extra)
        } else {
            label
        })
    }

    /// Flatten search snippets with the same rules as chat-row previews.
    pub fn plain_message_preview(&self, input: &str) -> String {
        plain_sidebar_preview(input)
    }

    /// The one-line sidebar preview for a message. `Message::sidebar_preview`
    /// owns the wording; this adds the attachment lookup the domain type has no
    /// storage handle for.
    pub fn message_sidebar_preview(&self, message: &crate::domain::message::Message) -> String {
        let label = self.attachment_preview_label(message);
        let preview = message.sidebar_preview(
            self.auth_state().get().current_user_id.as_ref(),
            label.as_deref(),
        );
        // A notice is worded to follow its actor ("left the channel"), as the
        // message list renders it; alone in a preview it named nobody.
        if matches!(
            message.content,
            crate::domain::message::MessageContent::System(_)
        ) {
            return plain_sidebar_preview(&format!(
                "{} {}",
                self.message_sender_display_name(message),
                preview
            ));
        }
        plain_sidebar_preview(&preview)
    }

    /// Preview + raw activity time of a conversation's latest message: the
    /// second line and the time source of a sidebar row. Empty pair when
    /// nothing has happened yet, so a fresh row renders as name only.
    ///
    /// Every sidebar row kind — DM, channel, private group — goes through here,
    /// which is why the anon-group store can hand its own `Message` in rather
    /// than each caller re-deriving the wording.
    pub fn sidebar_activity(
        &self,
        latest: Option<&crate::domain::message::Message>,
    ) -> (String, Option<chrono::DateTime<chrono::Utc>>) {
        match latest {
            Some(msg) => (self.message_sidebar_preview(msg), Some(msg.created_at)),
            None => (String::new(), None),
        }
    }

    /// The storage bucket a channel's messages and pins are filed under.
    ///
    /// Never hardcode `ConversationType::Channel` for a channel: its messages
    /// are filed under `Channel::conversation_type()` — `Channel`, or
    /// `SavedMessages` for the vault, or `ServerDm`.
    pub fn channel_conversation_type(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> crate::domain::message::ConversationType {
        crate::services::message::conversion::conversation_type_for_channel(
            &self.services.channel_repo,
            channel_id,
        )
    }

    /// One folder row, shaped like the loose row for the same conversation so
    /// a chat looks the same filed or not: the name (a channel-free DM with no
    /// unified-conversation row yet falls back to the peer profile), initials,
    /// badge, pin, Leave/Delete availability, and — for the kinds a folder
    /// renders with the main list's rows (friend DMs and channels) — preview,
    /// time, avatar, presence and the private-channel lock.
    pub fn section_row(
        &self,
        conv: &crate::domain::section::SectionConversation,
    ) -> crate::ui::sidebar_data::ChatRowSendData {
        use crate::domain::message::ConversationType as CT;
        use crate::ui::sidebar_data::ChatRowSendData;

        let mut display_name = conv.display_name.clone();
        if display_name.is_empty() {
            if let Ok(peer_id) = conv.conversation_id.parse() {
                display_name = self.display_name(&peer_id);
            }
        }
        let channel_id = conv
            .conversation_id
            .parse::<crate::domain::ids::ChannelId>()
            .ok();
        let is_channel = conv.conversation_type == CT::Channel;
        let mut row = ChatRowSendData {
            unread_count: conv.unread_count,
            is_pinned: conv.is_pinned,
            can_leave: match conv.conversation_type {
                // As `private_group_row`.
                CT::AnonGroup => conv
                    .conversation_id
                    .parse()
                    .is_ok_and(|id| self.anon_groups().can_leave(&id)),
                CT::Channel => channel_id.is_some_and(|id| self.can_leave_channel(&id)),
                _ => false,
            },
            can_delete: (is_channel && channel_id.is_some_and(|id| self.is_former_channel(&id)))
                || (conv.conversation_type == CT::AnonGroup
                    && conv
                        .conversation_id
                        .parse()
                        .ok()
                        .and_then(|id| self.get_anon_group(&id))
                        .is_some_and(|g| {
                            g.status == crate::domain::anon_group::AnonGroupStatus::Removed
                        })),
            ..ChatRowSendData::new(
                conv.conversation_type.clone(),
                conv.conversation_id.clone(),
                display_name,
            )
        };

        let Some(id) = channel_id else { return row };
        if !matches!(conv.conversation_type, CT::ServerDm | CT::Channel) {
            return row;
        }
        (row.last_message, row.timestamp_at) =
            self.sidebar_activity(self.latest_message(&id, &conv.conversation_type).as_ref());
        match conv.conversation_type {
            CT::ServerDm => {
                let peer = crate::domain::ids::UserId::from_uuid(*id.as_uuid());
                let profile = self.get_profile_local(&peer);
                row.presence_status = self.peer_presence(&peer).as_str().to_string();
                row.avatar_bytes = profile.and_then(|p| p.avatar_bytes);
            }
            _ => {
                let channel = self.channels().channel_repo.get(&id).ok().flatten();
                row.private_channel = channel.as_ref().is_some_and(|c| {
                    c.channel_type == crate::domain::channel::ChannelType::Private
                });
                // A channel's icon is raw bytes on its record, not a URL.
                row.avatar_bytes = channel.and_then(|c| c.icon).filter(|b| !b.is_empty());
            }
        }
        row
    }

    /// Build the regular (non-anon) DM sidebar rows — one per accepted
    /// friend, keyed by the peer's `user_id` (channel-free, Signal-style).
    /// A friend with no messages yet still shows (empty preview), so a
    /// freshly-accepted friend appears immediately. Shown in both modes: in
    /// Shield the same conversation travels over the mailbox.
    ///
    /// This is the single source of truth for regular DM rows — both
    /// `list_dm_sidebar` and the initial-load sidebar builder use it, so
    /// the UI never iterates channels to find DMs.
    pub fn list_regular_dm_sidebar(&self) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        use crate::domain::message::ConversationType;
        use crate::ui::sidebar_data::ChatRowSendData;

        // Each row carries its last activity, so the list is ordered newest
        // first. The friend list's order used to leak through, so every
        // rebuild (opening a profile, a pin) undid the move-to-top a live
        // message had made.
        let mut out: Vec<ChatRowSendData> = Vec::new();
        for peer_id in self.dm_peers() {
            // Local storage key for a channel-free DM = the peer's user_id
            // wrapped as a ChannelId (same UUID).
            let conversation_id: crate::domain::ids::ChannelId = peer_id.into();

            let display_name = self.display_name(&peer_id);
            // Presence from the stored profile (hidden for a blocked peer).
            // It used to be hardcoded "offline", so every friend's dot in the
            // sidebar was grey until a live presence event happened to
            // arrive, and grey again after the next rebuild.
            let profile = self.get_profile_local(&peer_id);
            let presence_status = self.peer_presence(&peer_id).as_str().to_string();
            let avatar_bytes = profile.and_then(|p| p.avatar_bytes);

            // Preview + timestamp from local DM history (empty if none). This
            // used to hand-roll the text-only case, so a DM whose last message
            // was a file rendered blank until the next live message arrived.
            let latest = self.latest_message(&conversation_id, &ConversationType::ServerDm);
            let (last_message, timestamp_at) = self.sidebar_activity(latest.as_ref());

            let unread_count = self
                .messages()
                .message_repo
                .get_unread_count(&conversation_id, &ConversationType::ServerDm)
                .unwrap_or(0) as u32;

            out.push(ChatRowSendData {
                user_id: peer_id.to_string(),
                avatar_bytes,
                presence_status,
                last_message,
                timestamp_at,
                unread_count,
                last_activity: latest.map(|m| m.created_at),
                ..ChatRowSendData::new(
                    ConversationType::ServerDm,
                    peer_id.to_string(),
                    display_name,
                )
            });
        }
        self.finish_dm_sidebar(out)
    }

    /// Final pass every DM sidebar list goes through: drop rows that live in a
    /// custom section (they render inside that folder — showing them here too
    /// rendered the same conversation twice), mark pins, and order the rows
    /// as the chat list does. Idempotent, so the anon and non-anon paths can
    /// both call it.
    fn finish_dm_sidebar(
        &self,
        mut rows: Vec<crate::ui::sidebar_data::ChatRowSendData>,
    ) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        rows.retain(|r| !sectioned.contains(&r.id));

        let pinned = self.conversation_pins().pinned_ids();
        for row in rows.iter_mut() {
            row.is_pinned = pinned.contains(&row.id);
        }
        crate::ui::sidebar_data::order_chat_rows(&mut rows);
        rows
    }

    /// Build the complete DM sidebar list — friend DMs plus secret chats
    /// (anon DMs: active conversations, incoming requests, outgoing requests)
    /// — as one ordered, fully-shaped vector the UI renders directly. This is
    /// the single source of truth for the sidebar; the UI must not merge two
    /// separate lists (that races and drops fresh DMs).
    pub fn list_dm_sidebar(&self) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        use crate::domain::anon_dm::AnonDmStatus;
        use crate::domain::message::ConversationType;
        use crate::ui::sidebar_data::ChatRowSendData;

        // Friend DMs show in both modes — one conversation per friend, whose
        // transport follows the mode. Secret chats (anon DMs) also show in
        // both modes (Telegram-style), as their own `AnonDm` rows beside the
        // friend DM with the same person: the row kind is what the UI marks
        // as secret. Their traffic rides the onion channel in either mode.
        let mut out: Vec<ChatRowSendData> = self.list_regular_dm_sidebar();
        // Old requests can coexist with an accepted chat in local storage.
        // A superseding incoming request takes the peer's one visible row.
        let mut shown_secret_peers = std::collections::HashSet::new();
        let anon_conversations = self.list_anon_dm_conversations();
        let superseded_peers: std::collections::HashSet<_> = anon_conversations
            .iter()
            .filter(|dm| dm.status == AnonDmStatus::Superseded)
            .map(|dm| dm.peer_id)
            .collect();

        // Anon DMs — incoming message requests awaiting acceptance.
        for dm in self.list_anon_dm_message_requests() {
            if !shown_secret_peers.insert(dm.peer_id) {
                continue;
            }
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            let latest = self.get_latest_anon_dm_message(&dm.id);
            let last_message = if superseded_peers.contains(&dm.peer_id) {
                format!("{} started a new secret chat — Accept / Decline", peer_name)
            } else {
                match &latest {
                    Some(msg) => self.message_sidebar_preview(msg),
                    None => "Message request".to_string(),
                }
            };
            out.push(ChatRowSendData {
                user_id: dm.peer_id.to_string(),
                avatar_bytes: self.anon_dm_avatar(&dm),
                presence_status: "pending".to_string(),
                last_message,
                timestamp_at: Some(dm.created_at),
                unread_count: 1,
                last_activity: Some(latest.map_or(dm.created_at, |m| m.created_at)),
                label: crate::ui::sidebar_data::ChatRowLabel::IncomingSecretRequest,
                ..ChatRowSendData::new(ConversationType::AnonDm, dm.id.to_string(), peer_name)
            });
        }

        // Anon DMs — active conversations.
        for dm in anon_conversations {
            if dm.status != AnonDmStatus::Active || !shown_secret_peers.insert(dm.peer_id) {
                continue;
            }
            let display_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            let latest = self.get_latest_anon_dm_message(&dm.id);
            // The preview every other row uses: "You: " on our own messages
            // (RC108 O5), typed attachment labels.
            let (last_message, timestamp_at) = match &latest {
                Some(msg) => (self.message_sidebar_preview(msg), Some(msg.created_at)),
                None => (String::new(), None),
            };
            out.push(ChatRowSendData {
                user_id: dm.peer_id.to_string(),
                avatar_bytes: self.anon_dm_avatar(&dm),
                presence_status: "online".to_string(),
                last_message,
                timestamp_at,
                unread_count: dm.unread_count,
                last_activity: latest.map(|m| m.created_at),
                ..ChatRowSendData::new(ConversationType::AnonDm, dm.id.to_string(), display_name)
            });
        }

        // Anon DMs — outgoing requests we sent, awaiting acceptance.
        for dm in self.list_anon_dm_outgoing_requests() {
            if !shown_secret_peers.insert(dm.peer_id) {
                continue;
            }
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            let latest = self.get_latest_anon_dm_message(&dm.id);
            let (last_message, timestamp_at) = match &latest {
                Some(msg) => {
                    let preview = msg
                        .as_text()
                        .filter(|t| !t.is_empty())
                        .map(|t| plain_sidebar_preview(&format!("You: {}", t)))
                        .unwrap_or_else(|| "Message request sent".to_string());
                    (preview, Some(msg.created_at))
                }
                None => ("Waiting for response...".to_string(), Some(dm.created_at)),
            };
            out.push(ChatRowSendData {
                user_id: dm.peer_id.to_string(),
                avatar_bytes: self.anon_dm_avatar(&dm),
                presence_status: "offline".to_string(),
                last_message,
                timestamp_at,
                last_activity: Some(latest.map_or(dm.created_at, |m| m.created_at)),
                label: crate::ui::sidebar_data::ChatRowLabel::OutgoingSecretRequest,
                ..ChatRowSendData::new(ConversationType::AnonDm, dm.id.to_string(), peer_name)
            });
        }

        self.finish_dm_sidebar(out)
    }

    /// The chat list: every loose conversation the current mode shows — DMs
    /// (friend DMs and secret chats with their requests), channels (current and former),
    /// private groups — as ONE list, pinned first, then newest activity first
    /// (Signal/Telegram). It used to be a DM block above a channel block, so a
    /// channel with newer activity never rose above a DM. Folders render their
    /// own rows (`list_sections_with_conversations`); every builder here drops
    /// what lives in one.
    pub fn sidebar_rows(&self) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        let mut rows = self.list_dm_sidebar();
        rows.extend(self.sidebar_channels().iter().map(|c| self.channel_row(c)));
        rows.extend(
            self.sidebar_private_groups()
                .iter()
                .map(|g| self.private_group_row(g)),
        );
        crate::ui::sidebar_data::order_chat_rows(&mut rows);
        // The local AI channel leads the list while it is on; it cannot be
        // pinned, so no activity order may bury it.
        #[cfg(feature = "jarvis")]
        {
            let jarvis = super::jarvis_channel_id().to_string();
            if let Some(i) = rows.iter().position(|r| r.id == jarvis) {
                let row = rows.remove(i);
                rows.insert(0, row);
            }
        }
        rows
    }

    /// A channel's chat-list row.
    fn channel_row(
        &self,
        channel: &crate::domain::channel::Channel,
    ) -> crate::ui::sidebar_data::ChatRowSendData {
        use crate::ui::sidebar_data::ChatRowSendData;
        let id = channel.id.to_string();
        let latest = self.latest_message(&channel.id, &channel.conversation_type());
        let (last_message, _) = self.sidebar_activity(latest.as_ref());
        // A channel with no messages yet orders (and is dated) by when it was
        // created, so a just-created channel leads the list instead of sinking
        // below every dated row.
        let last_activity = latest
            .map(|m| m.created_at)
            .or(channel.last_message_at)
            .unwrap_or(channel.created_at);
        ChatRowSendData {
            avatar_bytes: channel.icon.clone().filter(|b| !b.is_empty()),
            last_message,
            timestamp_at: Some(last_activity),
            unread_count: channel.unread_count,
            is_pinned: self.is_conversation_pinned(&id),
            private_channel: channel.channel_type == crate::domain::channel::ChannelType::Private,
            can_leave: self.can_leave_channel(&channel.id),
            can_delete: self.is_former_channel(&channel.id),
            last_activity: Some(last_activity),
            ..ChatRowSendData::new(
                crate::domain::message::ConversationType::Channel,
                id,
                channel.name.clone(),
            )
        }
    }

    /// A private (anon) group's chat-list row.
    fn private_group_row(
        &self,
        group: &crate::domain::anon_group::AnonGroup,
    ) -> crate::ui::sidebar_data::ChatRowSendData {
        use crate::ui::sidebar_data::ChatRowSendData;
        let latest = self.get_latest_anon_group_message(&group.id);
        let (last_message, _) = self.sidebar_activity(latest.as_ref());
        // As for channels: no messages yet = ordered by creation.
        let last_activity = latest.map_or(group.created_at, |m| m.created_at);
        ChatRowSendData {
            last_message,
            timestamp_at: Some(last_activity),
            unread_count: self.anon_group_unread_count(&group.id),
            member_count: self.get_anon_group_members(&group.id).len() as u32,
            is_pinned: self.is_conversation_pinned(&group.id.to_string()),
            // Any member may leave; the owner only once someone else owns it
            // or nobody else is left (`AnonGroupService::can_leave`).
            can_leave: self.anon_groups().can_leave(&group.id),
            can_delete: group.status == crate::domain::anon_group::AnonGroupStatus::Removed,
            last_activity: Some(last_activity),
            ..ChatRowSendData::new(
                crate::domain::message::ConversationType::AnonGroup,
                group.id.to_string(),
                group.name.clone(),
            )
        }
    }

    /// Resolve a DM sidebar id to the conversation it names.
    ///
    /// The id is either an `AnonDmId` (an anon conversation) or a peer `UserId`
    /// (a channel-free server DM — the row's id IS the peer). Both are UUIDs,
    /// so the anon table is the discriminator, not the string shape.
    ///
    /// The UI used to answer this by scanning the DM sidebar model for a row
    /// with a matching id. Rows filed into a custom folder are removed from
    /// that model on purpose — they render inside the folder — so the lookup
    /// came back empty and a foldered DM opened into nothing: no active
    /// conversation, no messages, and every send silently dropped.
    /// The conversation a sidebar row's "Delete chat" or "Leave" acts on: a
    /// channel row is its channel, a group row its group, a DM row resolves
    /// as [`Self::resolve_dm_conversation`].
    pub fn resolve_row_conversation(
        &self,
        row_id: &str,
        kind: &crate::domain::message::ConversationType,
    ) -> Option<crate::state::ActiveConversation> {
        match kind {
            crate::domain::message::ConversationType::Channel => row_id
                .parse()
                .ok()
                .map(crate::state::ActiveConversation::Channel),
            crate::domain::message::ConversationType::AnonGroup => row_id
                .parse()
                .ok()
                .map(crate::state::ActiveConversation::AnonGroup),
            _ => self.resolve_dm_conversation(row_id),
        }
    }

    pub fn resolve_dm_conversation(&self, dm_id: &str) -> Option<crate::state::ActiveConversation> {
        use crate::state::ActiveConversation;

        if let Ok(anon_id) = dm_id.parse::<crate::domain::anon_dm::AnonDmId>() {
            if self.get_anon_dm_conversation(&anon_id).is_some() {
                return Some(ActiveConversation::AnonDm(anon_id));
            }
        }
        let peer_id = dm_id.parse::<crate::domain::ids::UserId>().ok()?;
        // A friend's DM is the same conversation in both modes; Shield only
        // changes its transport (`send_friend_dm_content`). Anyone else gets
        // no friend DM in Shield — bootstrapping a mailbox with a stranger
        // takes identified calls — and neither does a peer we know only from
        // an anon group (it would tie their anonymous membership, and ours,
        // to the real accounts). A peer we also know anonymously opens that
        // conversation instead; otherwise there is nothing to open.
        if self.is_friend(&peer_id) {
            return Some(ActiveConversation::ServerDm { peer_id });
        }
        if self.anon_mode_enabled() || self.known_only_from_anon_group(&peer_id) {
            return self
                .find_anon_dm_by_peer(&peer_id)
                .map(ActiveConversation::AnonDm);
        }
        Some(ActiveConversation::ServerDm { peer_id })
    }
}
