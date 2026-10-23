use super::*;

impl ChatterClient {
    // ── Drafts (façade) ─────────────────────────────────────────────

    /// Conversation switch: park `draft` under `leaving`, return the draft
    /// parked under `entering` (empty if none). An empty draft clears the slot.
    pub fn swap_composer_draft(
        &self,
        leaving: &str,
        draft: ComposerDraft,
        entering: &str,
    ) -> ComposerDraft {
        // A queued navigation callback can arrive after sign-out cleared the
        // store but before the old UI text was cleared: a changed session
        // discards `draft` rather than parking it under the new account.
        let (mut drafts, changed_session) = self.composer_drafts_for_session();
        if !changed_session && !leaving.is_empty() {
            let unchanged = drafts
                .get(leaving)
                .map_or(draft.is_empty(), |held| *held == draft);
            if draft.is_empty() {
                drafts.remove(leaving);
            } else {
                drafts.insert(leaving.to_owned(), draft);
            }
            if !unchanged {
                self.persist_composer_drafts(&drafts);
            }
        }
        *self.composer_owner.lock() = entering.to_owned();
        // The entry stays in the map while shown, so a restart before the
        // next autosave still finds it.
        drafts.get(entering).cloned().unwrap_or_default()
    }

    // ── Conversation listing ────────────────────────────────────────

    /// List all channels the user is a member of.
    pub fn list_channels(&self) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().get_channels_for_user()
    }

    /// Number of conversations with at least one unread message — the Chats
    /// tab and tray badge. Counted from the messages table directly
    /// (`conversation_stats`), because the channel-rooted `list_all()` left out
    /// every channel-free server DM: two unread DMs and one unread channel
    /// read "1".
    ///
    /// Only conversations the sidebar shows as a row count
    /// ([`sidebar_row_keys`](Self::sidebar_row_keys)): with the shield on an
    /// identified channel has no row, and in either mode neither does a DM
    /// with an ex-friend, a channel we left or a group we were removed from —
    /// an unread message there lit the tab with no row to clear it.
    ///
    /// Returns 0 on storage error: a missing badge beats a UI hard-fail.
    pub fn unread_conversation_count(&self) -> u32 {
        self.unread_badges(&crate::state::ActiveConversation::None)
            .total
    }

    /// Absolute unread values for one sidebar row and the Chats total, from
    /// one read of the messages table. The UI sets these; it never adds to
    /// its own copy (the live `+1` and the rebuild counted differently).
    ///
    /// `row` is `conversation`'s own count ([`ActiveConversation::is_for_channel`]).
    /// Blocking storage read: call off the UI thread.
    pub fn unread_badges(&self, conversation: &crate::state::ActiveConversation) -> UnreadBadges {
        let Ok(stats) = self.services.conversation_repo.conversation_stats() else {
            return UnreadBadges::default();
        };
        let rows = self.sidebar_row_keys();
        let mut badges = UnreadBadges::default();
        for (key, st) in &stats {
            if st.unread_count == 0 {
                continue;
            }
            let (id, _) = key;
            // Mesh conversations are not Chats rows: they have their own tab and badge.
            if rows.contains(key) {
                badges.total += 1;
            }
            if id.parse().is_ok_and(|cid| conversation.is_for_channel(cid)) {
                badges.row += st.unread_count;
            }
        }
        badges
    }

    /// Unread messages in a private group, for its sidebar row — the same
    /// count `unread_badges` gives the row live, so a rebuild keeps the badge.
    pub fn anon_group_unread_count(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> u32 {
        self.messages()
            .message_repo
            .get_unread_count(&(*group_id).into(), &ConversationType::AnonGroup)
            .unwrap_or(0) as u32
    }

    /// Every conversation the Chats list shows as a row in the current mode,
    /// keyed as `conversation_stats` keys them — the loose lists
    /// (`list_dm_sidebar`, `sidebar_channels`, `sidebar_private_groups`) plus
    /// the folders (`list_sections_with_conversations`). Ids only: the same
    /// inclusion rules as those builders, without their previews.
    pub(super) fn sidebar_row_keys(&self) -> std::collections::HashSet<(String, ConversationType)> {
        let mut rows = std::collections::HashSet::new();
        for peer in self.dm_peers() {
            rows.insert((peer.to_string(), ConversationType::ServerDm));
        }
        // Secret chats (anon DMs) and their requests are rows in both modes.
        let anon = self
            .list_anon_dm_conversations()
            .into_iter()
            .chain(self.list_anon_dm_message_requests())
            .chain(self.list_anon_dm_outgoing_requests());
        for dm in anon {
            rows.insert((dm.id.to_string(), ConversationType::AnonDm));
        }
        for channel in self.sidebar_channels() {
            rows.insert((channel.id.to_string(), ConversationType::Channel));
        }
        for group in self.list_anon_groups().unwrap_or_default() {
            rows.insert((group.id.to_string(), ConversationType::AnonGroup));
        }
        for section in self.list_sections_with_conversations().unwrap_or_default() {
            for c in section.conversations {
                rows.insert((c.conversation_id, c.conversation_type));
            }
        }
        rows
    }

    /// Resolve an opaque notification tap only after sign-in, against the
    /// conversations visible to this account in the current mode.
    pub fn conversation_for_notification_key(
        &self,
        key: &str,
    ) -> Option<crate::state::ActiveConversation> {
        if !self.is_logged_in() {
            return None;
        }
        self.services
            .notification_service
            .resolve_key(key, self.sidebar_row_keys())
    }

    /// `Some(false)` means an opaque tap belongs to a different account and
    /// must not be retained for a later sidebar refresh.
    pub fn notification_tap_belongs_to_current_account(&self, key: &str) -> Option<bool> {
        self.services
            .notification_service
            .tap_belongs_to_current_account(key)
    }

    /// A pending secret request opens Requests, not its chat row. Resolve the
    /// opaque key against this account before exposing that distinction to UI.
    pub fn notification_target_for_key(&self, key: &str) -> Option<NotificationTarget> {
        let conversation = self.conversation_for_notification_key(key)?;
        if let crate::state::ActiveConversation::AnonDm(id) = conversation {
            if self
                .list_anon_dm_message_requests()
                .iter()
                .any(|dm| dm.id == id)
            {
                return Some(NotificationTarget::SecretRequest);
            }
        }
        Some(NotificationTarget::Conversation(conversation))
    }

    /// Whether `message` belongs in the conversation on screen — the one
    /// predicate for "insert it into the open view", "mark it read", "don't
    /// badge it" and "don't notify for it".
    pub fn is_showing(&self, message: &crate::domain::message::Message) -> bool {
        self.is_showing_conversation(message.conversation_id)
    }

    /// [`is_showing`](Self::is_showing) for events that carry only the
    /// conversation id (edits, deletes).
    pub fn is_showing_conversation(&self, conversation_id: crate::domain::ids::ChannelId) -> bool {
        self.chat_state()
            .active_conversation()
            .is_for_channel(conversation_id)
    }

    /// A page of channel members (fetched from the server, which also
    /// refreshes local membership) shaped for display: resolved name, known
    /// handle and avatar. The avatar comes from the member's stored profile —
    /// the same source DM rows use, refreshed when stale — so a member shows
    /// their photo wherever this device can decrypt it (friends: the profile
    /// key only travels with friendship), and initials only when it cannot.
    pub async fn list_members_with_profiles(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<
        Vec<crate::domain::channel::MemberWithProfile>,
        crate::services::channel::ChannelError,
    > {
        let members = self
            .channels()
            .get_channel_members(channel_id, limit, offset)
            .await?;
        let me = self.session_current_user_id();
        // Refetch stale member profiles first, as a DM open does for its peer
        // (Signal-Desktop fetches every member's profile on conversation
        // open). Only members whose profile key we hold — a friend's, or a
        // co-member's from their SKDM: those are the profiles this device can
        // decrypt. Throttled per user; a no-op in Shield mode.
        let users = &self.messages().user_repo;
        let here = crate::state::ActiveConversation::Channel(*channel_id);
        futures::future::join_all(
            members
                .iter()
                .filter(|m| {
                    me.as_ref().is_some_and(|me| {
                        me != &m.user_id
                            && matches!(users.get_friend_profile_key(me, &m.user_id), Ok(Some(_)))
                    })
                })
                .map(|m| self.refresh_profile_if_stale(&m.user_id, &here)),
        )
        .await;
        // Our own role, for what we may do to each row. Saved Messages has
        // no moderation (the server refuses it).
        let actor = self.get_channel_local(channel_id).and_then(|c| {
            if matches!(c.channel_type, crate::domain::ChannelType::SavedMessages) {
                return None;
            }
            let me = me.as_ref()?;
            if c.is_owner(me) {
                return Some(crate::domain::channel::MemberRole::Owner);
            }
            // This page's row is freshest; when our row is on another page,
            // use our stored membership (kept current by roster sync), so
            // moderation flags do not depend on which page is displayed.
            // Display only — the server still authorizes every action.
            members
                .iter()
                .find(|m| &m.user_id == me)
                .map(|m| m.role)
                .or_else(|| {
                    self.channels()
                        .channel_repo
                        .get_membership(channel_id, me)
                        .ok()
                        .flatten()
                        .map(|m| m.role)
                })
        });
        Ok(members
            .into_iter()
            .map(|membership| {
                let is_me = me.as_ref() == Some(&membership.user_id);
                let moderation = crate::domain::channel::MemberModeration::for_target(
                    actor,
                    is_me,
                    membership.role,
                );
                let unverified = self.is_unverified_channel_member(channel_id, &membership.user_id);
                let mut row = self.member_with_profile(membership);
                row.moderation = moderation;
                row.unverified = unverified;
                // Our own row reads "You", as in Signal's member list.
                if is_me {
                    row.display_name = "You".into();
                }
                row
            })
            .collect())
    }

    /// Resolve one membership's display data from local state.
    pub fn member_with_profile(
        &self,
        membership: crate::domain::channel::Membership,
    ) -> crate::domain::channel::MemberWithProfile {
        let uid = membership.user_id;
        crate::domain::channel::MemberWithProfile {
            display_name: self.display_name(&uid),
            // Our full handle lives in settings, while the user directory may
            // only hold the nickname used at login. Peers show a handle only
            // when the directory has its complete lookup form.
            username: if self.session_current_user_id() == Some(uid) {
                self.full_username().unwrap_or_default()
            } else {
                self.known_username(&uid).unwrap_or_default()
            },
            avatar_bytes: self.user_avatar(&uid),
            moderation: crate::domain::channel::MemberModeration::default(),
            unverified: false,
            membership,
        }
    }

    /// List all anonymous DM conversations.
    pub fn list_anon_dms(
        &self,
    ) -> Result<Vec<crate::domain::anon_dm::AnonDm>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().list_conversations()
    }

    /// Look up an anon DM conversation by id.
    pub fn get_anon_dm_conversation(
        &self,
        id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().get_conversation(id).ok().flatten()
    }

}
