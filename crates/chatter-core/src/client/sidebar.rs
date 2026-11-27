use super::*;
use crate::domain::{ids::SectionId, message::ConversationType, section::CustomSection};
use crate::ui::sidebar_data::ChatRowSendData;

const SAVED_CARD_ID: &str = "saved";
const CHATS_CARD_ID: &str = "chats";
const CHATS_COLLAPSED_KEY: &str = "sidebar.chats_collapsed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarCardKind {
    Saved,
    Chats,
    Section,
}

#[derive(Clone, Debug)]
pub struct SidebarCard {
    pub id: String,
    pub kind: SidebarCardKind,
    pub title: String,
    pub emoji: Option<String>,
    pub collapsed: bool,
    pub unread: u32,
    pub pinned: Vec<SidebarCardRow>,
    pub rows: Vec<SidebarCardRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarCardRowKind {
    Dm,
    PrivateGroup,
    Channel,
    Saved,
}

#[derive(Clone, Debug)]
pub struct SidebarCardRow {
    pub conversation_id: String,
    pub kind: SidebarCardRowKind,
    pub name: String,
    pub short_name: String,
    pub preview: String,
    pub time_label: String,
    pub unread: u32,
    pub muted: bool,
    pub online: bool,
    pub locked: bool,
    /// Existing chat presentation, including avatar bytes and the exact open/menu kind.
    pub chat_row: ChatRowSendData,
}

impl From<ChatRowSendData> for SidebarCardRow {
    fn from(row: ChatRowSendData) -> Self {
        use SidebarCardRowKind as Kind;
        let kind = match row.kind {
            ConversationType::SavedMessages => Kind::Saved,
            ConversationType::AnonGroup => Kind::PrivateGroup,
            ConversationType::Channel | ConversationType::Mesh => Kind::Channel,
            ConversationType::ServerDm | ConversationType::AnonDm | ConversationType::MeshDm => {
                Kind::Dm
            }
        };
        let name = row.shown_name();
        let short_name = if kind == Kind::Dm {
            row.display_name
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        } else {
            name.clone()
        };
        Self {
            conversation_id: row.id.clone(),
            kind,
            name,
            short_name,
            preview: row.last_message.clone(),
            time_label: row.timestamp_text(),
            unread: row.unread_count,
            muted: row.is_muted,
            online: row.presence_status == "online",
            locked: row.private_channel,
            chat_row: row,
        }
    }
}


impl ChatterClient {
    /// Sidebar cards in display order. Saved follows its existing Shield
    /// visibility policy, and only explicitly created sections are projected.
    pub fn sidebar_cards(&self) -> Vec<SidebarCard> {
        let mut cards = Vec::new();
        if self.visible_in_mode(&ConversationType::SavedMessages) {
            let title = chatter_i18n::tr("Saved messages");
            let (preview, time_label) = self.saved_messages_sidebar_preview();
            let mut row = SidebarCardRow::from(ChatRowSendData {
                last_message: preview,
                ..ChatRowSendData::new(
                    ConversationType::SavedMessages,
                    SAVED_CARD_ID,
                    title.as_str(),
                )
            });
            row.time_label = time_label;
            cards.push(SidebarCard {
                id: SAVED_CARD_ID.into(),
                kind: SidebarCardKind::Saved,
                title,
                emoji: None,
                collapsed: false,
                unread: 0,
                pinned: Vec::new(),
                rows: vec![row],
            });
        }

        let chats = self.sidebar_rows();
        let unread = chats
            .iter()
            .filter(|row| !row.is_muted)
            .map(|row| row.unread_count)
            .sum();
        let (pinned, rows) = chats
            .into_iter()
            .map(SidebarCardRow::from)
            .partition(|row| row.chat_row.is_pinned);
        cards.push(SidebarCard {
            id: CHATS_CARD_ID.into(),
            kind: SidebarCardKind::Chats,
            title: chatter_i18n::tr("Chats"),
            emoji: None,
            collapsed: self.chats_card_collapsed().unwrap_or_default(),
            unread,
            pinned,
            rows,
        });

        let sections = self.list_sections_with_conversations().unwrap_or_default();
        let rows = self.section_rows(&sections);
        cards.extend(sections.into_iter().zip(rows).map(|(section, rows)| {
            let unread = self.section_unread_badge(&section);
            SidebarCard {
                id: section.id.to_string(),
                kind: SidebarCardKind::Section,
                title: section.name,
                emoji: Some(section.icon_emoji).filter(|emoji| !emoji.is_empty()),
                collapsed: !section.is_expanded,
                unread,
                pinned: Vec::new(),
                rows: rows.into_iter().map(SidebarCardRow::from).collect(),
            }
        }));
        cards
    }

    fn chats_card_collapsed(&self) -> Result<bool, SectionError> {
        Ok(self
            .channels()
            .settings_repo
            .get_bool(CHATS_COLLAPSED_KEY)?
            .unwrap_or(false))
    }

    pub fn toggle_card_collapsed(&self, card_id: &str) -> Result<(), SectionError> {
        if card_id == CHATS_CARD_ID {
            use crate::storage::repositories::settings_repo::SettingsRepository;
            let mut conn = self.channels().settings_repo.database().connection()?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(crate::error::StorageError::from)?;
            let collapsed: Option<bool> =
                SettingsRepository::get_with_connection(&tx, CHATS_COLLAPSED_KEY)?;
            SettingsRepository::set_with_connection(
                &tx,
                CHATS_COLLAPSED_KEY,
                &!collapsed.unwrap_or_default(),
            )?;
            tx.commit().map_err(crate::error::StorageError::from)?;
            return Ok(());
        }
        let id = card_id
            .parse::<SectionId>()
            .map_err(|_| SectionError::NotFound)?;
        self.sections().toggle_current_expansion(&id)
    }

    pub fn move_conversation_to_card(
        &self,
        conversation_id: &str,
        card_id: &str,
    ) -> Result<(), SectionError> {
        let kind = self.movable_card_row_kind(conversation_id)?;
        if card_id == CHATS_CARD_ID {
            return self.remove_conversation_from_section(conversation_id);
        }
        let section = card_id
            .parse::<SectionId>()
            .map_err(|_| SectionError::NotFound)?;
        self.move_conversation_to_section(conversation_id, kind, &section)
    }

    pub fn create_section_with(
        &self,
        conversation_id: &str,
        name: &str,
        emoji: &str,
    ) -> Result<CustomSection, SectionError> {
        let kind = self.movable_card_row_kind(conversation_id)?;
        self.create_section(name, emoji, Some((conversation_id, kind)))
    }

    fn movable_card_row_kind(&self, id: &str) -> Result<ConversationType, SectionError> {
        if id == SAVED_CARD_ID {
            return Err(SectionError::NotFound);
        }
        if let Some(row) = self.sidebar_rows().into_iter().find(|row| row.id == id) {
            return Ok(row.kind);
        }
        self.list_sections_with_conversations()?
            .into_iter()
            .flat_map(|section| section.conversations)
            .find(|row| row.conversation_id == id)
            .map(|row| row.conversation_type)
            .ok_or(SectionError::NotFound)
    }

    /// Return the adjacent visible conversation in the same slot order used by
    /// the sidebar. A missing selection starts at the corresponding edge.
    pub fn adjacent_sidebar_conversation(
        &self,
        forward: bool,
        archive_open: bool,
    ) -> Option<crate::state::ActiveConversation> {
        use crate::ui::sidebar_data::{sidebar_slots, SidebarSlot};

        let rows = if archive_open { self.archived_sidebar_rows() } else { self.sidebar_rows() };
        let sections = if archive_open {
            Vec::new()
        } else {
            self.list_sections_with_conversations().unwrap_or_default()
        };
        let shape: Vec<_> = sections.iter()
            .map(|section| (section.is_expanded, section.conversations.len()))
            .collect();
        let ordered: Vec<_> = sidebar_slots(rows.len(), &shape).into_iter().filter_map(|slot| {
            match slot {
                SidebarSlot::Chat(index) => rows.get(index)
                    .map(|row| (&row.id, &row.kind)),
                SidebarSlot::SectionChat(section, index) => sections.get(section)
                    .and_then(|section| section.conversations.get(index))
                    .map(|row| (&row.conversation_id, &row.conversation_type)),
                _ => None,
            }
        }).filter_map(|(id, kind)| self.resolve_row_conversation(id, kind)).collect();
        let current = self.active_conversation();
        let index = ordered.iter().position(|&row| row == current);
        let next = match (index, forward) {
            (Some(i), true) => (i + 1) % ordered.len(),
            (Some(0), false) => ordered.len().checked_sub(1)?,
            (Some(i), false) => i - 1,
            (None, true) => 0,
            (None, false) => ordered.len().checked_sub(1)?,
        };
        ordered.get(next).copied()
    }

    const ARCHIVE_PREFIX: &'static str = "conv.archived.";

    /// Archive is a personal, device-local sidebar preference for every chat
    /// kind. ChannelService's archive RPC is owner-only channel management,
    /// so it cannot represent a member's personal chat archive.
    pub fn archived_conversation_ids(&self) -> std::collections::HashSet<String> {
        self.channels().settings_repo.get_all().unwrap_or_default().into_iter()
            .filter_map(|(key, value)| {
                (value.trim() == "true").then(|| key.strip_prefix(Self::ARCHIVE_PREFIX))
                    .flatten().map(str::to_owned)
            }).collect()
    }

    pub fn set_conversation_archived(
        &self,
        conversation_id: &str,
        archived: bool,
    ) -> Result<(), crate::error::StorageError> {
        let key = format!("{}{conversation_id}", Self::ARCHIVE_PREFIX);
        if archived {
            self.channels().settings_repo.set_bool(&key, true)
        } else {
            self.channels().settings_repo.delete(&key).map(|_| ())
        }
    }

    /// A new message returns an unmuted chat to the main list. Muted chats
    /// keep their place in Archive until explicitly unarchived.
    pub fn unarchive_on_incoming_message(
        self: &std::sync::Arc<Self>,
        message: &crate::domain::message::Message,
    ) -> Result<(), crate::error::StorageError> {
        let id = message.conversation_id.to_string();
        let key = format!("{}{id}", Self::ARCHIVE_PREFIX);
        if !self.channels().settings_repo.get_bool(&key)?.unwrap_or(false) {
            return Ok(());
        }
        let Some(conversation) = self.resolve_row_conversation(&id, &message.conversation_type) else {
            return Ok(());
        };
        if self.is_conversation_muted(conversation).unwrap_or(true) {
            return Ok(());
        }
        self.set_conversation_archived(&id, false)
    }
    /// A folder's header badge: unread in its rows, leaving out muted ones,
    /// as Telegram's folder counters do (a muted chat keeps its own grey
    /// count on its row).
    pub fn section_unread_badge(&self, section: &crate::domain::section::CustomSection) -> u32 {
        section
            .conversations
            .iter()
            .filter(|c| c.unread_count > 0)
            .map(|c| self.section_member_unread_badge(c))
            .sum()
    }

    /// One member's exact contribution to the folder header (muted excluded).
    pub fn section_member_unread_badge(
        &self,
        conversation: &crate::domain::section::SectionConversation,
    ) -> u32 {
        if conversation.unread_count == 0 {
            return 0;
        }
        if self
            .resolve_row_conversation(
                &conversation.conversation_id,
                &conversation.conversation_type,
            )
            .is_none_or(|conv| !self.read_conversation_mute(&conv).unwrap_or(false))
        {
            conversation.unread_count
        } else { 0 }
    }

    // ── Sidebar sections (custom grouping) façade ────────────────────────

    /// Custom folders with their rows, already ordered by each folder's sort
    /// mode — minus anything the current mode hides.
    ///
    /// A folder is not a way around Anon Mode. The loose lists hide identified
    /// channels and friend DMs while the shield is on, and hide anon DMs while
    /// it is off; a conversation filed into a folder kept rendering — and
    /// stayed clickable — in the wrong mode.
    pub fn list_sections_with_conversations(
        &self,
    ) -> Result<
        Vec<crate::domain::section::CustomSection>,
        crate::services::section_service::SectionError,
    > {
        let mut sections = self.sections().list_sections_with_conversations()?;
        let archived = self.archived_conversation_ids();
        for section in sections.iter_mut() {
            section.conversations.retain(|c| {
                self.visible_in_mode(&c.conversation_type)
                    && !self.is_blocked_conversation(&c.conversation_type, &c.conversation_id)
                    && !archived.contains(&c.conversation_id)
            });
            for conversation in &mut section.conversations {
                if conversation.conversation_type == crate::domain::message::ConversationType::ServerDm {
                    if let Ok(peer) = conversation.conversation_id.parse::<crate::domain::ids::UserId>() {
                        conversation.display_name = self.display_name(&peer);
                    }
                } else if conversation.conversation_type == crate::domain::message::ConversationType::AnonDm {
                    if let Ok(id) = conversation.conversation_id.parse::<crate::domain::anon_dm::AnonDmId>() {
                        if let Some(dm) = self.get_anon_dm_conversation(&id) {
                            conversation.display_name = self.anon_dm_peer_name(&dm);
                        }
                    }
                }
            }
            crate::services::section_service::sort_conversations(
                &mut section.conversations,
                section.sort_mode,
            );
            // The aggregate badge counts what the folder actually shows.
            section.unread_count = section.conversations.iter().map(|c| c.unread_count).sum();
        }
        Ok(sections)
    }

    /// Whether a conversation of this kind may be shown (listed, searched,
    /// opened) in the current mode. Shield on: never channels or Saved
    /// Messages. Friend DMs show in both — one conversation whose transport
    /// follows the mode (`send_friend_dm_content`). Anon DMs are secret chats
    /// (Telegram-style): device-bound, carried over the onion channel in
    /// either mode, so they show in both, as their own rows beside the friend
    /// DM with the same person. Anon groups and mesh are serverless and show
    /// in both.
    pub fn visible_in_mode(&self, kind: &crate::domain::message::ConversationType) -> bool {
        use crate::domain::message::ConversationType;
        let anon = self.anon_mode_enabled();
        match kind {
            ConversationType::Channel | ConversationType::SavedMessages => !anon,
            ConversationType::AnonDm
            | ConversationType::ServerDm
            | ConversationType::AnonGroup
            | ConversationType::Mesh
            | ConversationType::MeshDm => true,
        }
    }

    /// Whether the open conversation may stay open in the current mode
    /// ([`Self::visible_in_mode`]). Nothing open counts as visible. Entering
    /// Shield with a channel or Saved Messages open must close it.
    pub fn active_conversation_visible_in_mode(&self) -> bool {
        self.active_conversation_type()
            .is_none_or(|kind| self.visible_in_mode(&kind))
    }

    pub fn create_section(
        &self,
        name: &str,
        emoji: &str,
        conversation: Option<(&str, crate::domain::message::ConversationType)>,
    ) -> Result<crate::domain::section::CustomSection, crate::services::section_service::SectionError>
    {
        self.sections().create_section(name, emoji, conversation)
    }

    pub fn delete_section(
        &self,
        id: &crate::domain::ids::SectionId,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().delete_section(id)
    }

    pub fn rename_section(
        &self,
        id: &crate::domain::ids::SectionId,
        new_name: &str,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().rename_section(id, new_name)
    }

    pub fn change_section_icon(
        &self,
        id: &crate::domain::ids::SectionId,
        new_emoji: &str,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().change_icon(id, new_emoji)
    }

    pub fn toggle_section_expanded(
        &self,
        id: &crate::domain::ids::SectionId,
        expanded: bool,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().toggle_expanded(id, expanded)
    }

    pub fn reorder_sections(
        &self,
        ids: &[crate::domain::ids::SectionId],
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().reorder_sections(ids)
    }

    /// Move a section one place up (`delta` -1) or down (`delta` +1).
    pub fn move_section(
        &self,
        id: &crate::domain::ids::SectionId,
        delta: i32,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().move_section(id, delta)
    }

    /// Set how a section orders the conversations inside it.
    pub fn set_section_sort_mode(
        &self,
        id: &crate::domain::ids::SectionId,
        mode: crate::domain::section::SectionSortMode,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().set_sort_mode(id, mode)
    }

    // ── Conversation pins (local-only) ───────────────────────────────────

    /// Pin or unpin a conversation by its sidebar id.
    pub fn set_conversation_pinned(&self, conversation_id: &str, pinned: bool) -> bool {
        match self.conversation_pins().set_pinned(conversation_id, pinned) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(error = %e, conversation_id, pinned, "failed to persist conversation pin");
                false
            }
        }
    }

    /// Whether a conversation is pinned.
    pub fn is_conversation_pinned(&self, conversation_id: &str) -> bool {
        self.conversation_pins().is_pinned(conversation_id)
    }

    /// Every pinned conversation id, in one query.
    pub fn pinned_conversation_ids(&self) -> std::collections::HashSet<String> {
        self.conversation_pins().pinned_ids()
    }

    pub fn move_conversation_to_section(
        &self,
        conv_id: &str,
        conv_type: crate::domain::message::ConversationType,
        section_id: &crate::domain::ids::SectionId,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections()
            .move_conversation_to_section(conv_id, conv_type, section_id)
    }

    pub fn remove_conversation_from_section(
        &self,
        conv_id: &str,
    ) -> Result<(), crate::services::section_service::SectionError> {
        self.sections().remove_conversation_from_section(conv_id)
    }

    /// The name a conversation is shown under — the chat list's name, so a
    /// search hit and its sidebar row agree. A friend DM is channel-free
    /// (keyed by the peer, no `channels` row), so it resolves through the
    /// peer's display name; everything else through its stored metadata.
    pub fn conversation_display_name(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> String {
        use crate::domain::message::ConversationType;
        match conversation_type {
            ConversationType::ServerDm => self.display_name(
                &crate::domain::ids::UserId::from_uuid(*conversation_id.as_uuid()),
            ),
            // The stored name of the note-to-self channel is its internal
            // `saved_messages_<uuid>` id, not something to show a person.
            ConversationType::SavedMessages => chatter_i18n::tr("Saved messages"),
            _ => self
                .sections()
                .get_conversation_metadata(&conversation_id.to_string(), conversation_type)
                .ok()
                .flatten()
                .map(|c| c.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Unknown".to_string()),
        }
    }

    /// Preview text for a pinned message, resolved per conversation flavor
    /// (anon groups read the unified anon store; everything else the pin
    /// service). Falls back to `[Pinned message]`.
    pub fn pinned_message_preview(
        &self,
        conversation: &crate::state::ActiveConversation,
        msg_id: &crate::domain::ids::MessageId,
    ) -> String {
        use crate::state::ActiveConversation;
        match conversation {
            ActiveConversation::AnonGroup(_) => {
                let anon_id =
                    crate::domain::anon_group::AnonGroupMessageId::from_string(msg_id.to_string());
                self.get_anon_group_message(&anon_id)
                    .and_then(|m| {
                        m.as_text()
                            .filter(|t| !t.is_empty())
                            .map(|t| self.truncate_message_preview(&t))
                    })
                    .unwrap_or_else(|| "[Pinned message]".to_string())
            }
            _ => self
                .get_pinned_message(msg_id)
                .map(|d| d.preview)
                .unwrap_or_else(|| "[Pinned message]".to_string()),
        }
    }

    /// Channels to list in the sidebar: pinned first, then MRU; only
    /// Public/Private; anything filed into a custom section is excluded (it
    /// renders inside that folder instead — it used to render in BOTH); and
    /// empty while anon mode hides regular channels. Single source of the
    /// sidebar-inclusion rule.
    pub fn sidebar_channels(&self) -> Vec<crate::domain::channel::Channel> {
        if !self.visible_in_mode(&ConversationType::Channel) {
            return Vec::new();
        }
        use crate::domain::channel::ChannelType;
        // A channel that lives in a folder is not also a loose channel.
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        let mut channels: Vec<_> = self
            .get_channels_mru()
            .into_iter()
            .filter(|c| matches!(c.channel_type, ChannelType::Public | ChannelType::Private))
            .filter(|c| !sectioned.contains(&c.id.to_string()))
            .collect();
        // Pinned to the top, MRU preserved within each group.
        self.conversation_pins()
            .sort_pinned_first(&mut channels, |c| c.id.to_string());
        let channels = channels;

        // The local AI channel is pinned to the top when local AI is on.
        #[cfg(feature = "jarvis")]
        {
            let mut channels = channels;
            if self.jarvis_channel_enabled() {
                channels.insert(0, self.jarvis_virtual_channel());
            }
            return channels;
        }
        #[cfg(not(feature = "jarvis"))]
        channels
    }

    /// The channel to select on load: first non-SavedMessages channel in MRU
    /// order.
    pub fn first_selectable_channel_id(&self) -> Option<crate::domain::ids::ChannelId> {
        use crate::domain::channel::ChannelType;
        self.get_channels_mru()
            .iter()
            .filter(|c| !c.id.0.is_nil()) // never auto-select the local AI channel
            .find(|c| !matches!(c.channel_type, ChannelType::SavedMessages))
            .map(|c| c.id)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        channel::{Channel, ChannelType, MemberRole, Membership},
        ids::{ChannelId, UserId},
        message::Message,
    };
    use crate::state::ActiveConversation;
    use crate::storage::repositories::{
        channel_repo::ChannelRepository,
        settings_repo::{keys, SettingsRepository},
    };

    #[tokio::test]
    async fn sidebar_cards_order_pins_badges_and_shield_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let me = UserId::new();
        client.auth_state().set_logged_in(me, 1, "Me".into());
        SettingsRepository::new(client.services.database.clone())
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();

        let ids = || {
            client
                .sidebar_cards()
                .into_iter()
                .map(|card| card.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(), [SAVED_CARD_ID, CHATS_CARD_ID]);
        assert!(client
            .list_sections_with_conversations()
            .unwrap()
            .is_empty());

        let repo = ChannelRepository::new(client.services.database.clone());
        let add_channel = |name: &str| {
            let channel = Channel::new(ChannelId::new(), name.into(), ChannelType::Private, me);
            repo.insert(&channel).unwrap();
            repo.insert_membership(&Membership {
                channel_id: channel.id,
                user_id: me,
                role: MemberRole::Owner,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: Default::default(),
            })
            .unwrap();
            let message = Message::new_text(
                channel.id,
                ConversationType::Channel,
                Some(UserId::new()),
                "hello".into(),
                1,
            );
            client.messages().store_message_local(&message).unwrap();
            channel.id
        };
        let pinned = add_channel("Pinned channel").to_string();
        let loose = add_channel("Loose channel").to_string();
        let filed = add_channel("Filed channel");
        let filed_id = filed.to_string();
        assert!(client.set_conversation_pinned(&pinned, true));
        assert!(client.set_conversation_pinned(&filed_id, true));

        let first = client.create_section("First", "📁", None).unwrap();
        let second = client
            .create_section_with(&filed_id, "Second", "📌")
            .unwrap();
        let second_id = second.id.to_string();
        client.reorder_sections(&[second.id, first.id]).unwrap();
        assert_eq!(
            ids(),
            [
                SAVED_CARD_ID,
                CHATS_CARD_ID,
                second_id.as_str(),
                first.id.to_string().as_str()
            ]
        );
        assert!(client
            .move_conversation_to_card(&filed_id, &SectionId::new().to_string())
            .is_err());

        let cards = client.sidebar_cards();
        assert_eq!(cards[1].pinned.len(), 1);
        assert_eq!(cards[1].pinned[0].conversation_id, pinned);
        assert_eq!(cards[1].rows.len(), 1);
        assert_eq!(cards[1].rows[0].conversation_id, loose);
        assert_eq!(cards[1].unread, 2);
        assert!(cards[2].pinned.is_empty());
        assert_eq!(cards[2].rows[0].conversation_id, filed_id);
        assert_eq!(cards[2].unread, 1);

        client
            .mute_conversation(ActiveConversation::Channel(filed), true)
            .unwrap();
        client.toggle_card_collapsed(&second_id).unwrap();
        client.toggle_card_collapsed(CHATS_CARD_ID).unwrap();
        let cards = client.sidebar_cards();
        assert!(cards[1].collapsed && cards[2].collapsed);
        assert_eq!(cards[2].unread, 0);
        assert_eq!(cards[2].rows[0].unread, 1);

        #[cfg(feature = "onion")]
        {
            client
                .services
                .grpc_pool
                .set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
            client.set_anon_mode_enabled(true);
        }
        #[cfg(not(feature = "onion"))]
        client
            .anon_mode
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(client.anon_mode_enabled());
        let cards = client.sidebar_cards();
        assert_eq!(cards[0].id, CHATS_CARD_ID);
        assert!(cards
            .iter()
            .all(|card| card.rows.is_empty() && card.pinned.is_empty()));

        #[cfg(feature = "onion")]
        client.set_anon_mode_enabled(false);
        #[cfg(not(feature = "onion"))]
        client
            .anon_mode
            .store(false, std::sync::atomic::Ordering::Release);
        client
            .move_conversation_to_card(&filed_id, CHATS_CARD_ID)
            .unwrap();
        let cards = client.sidebar_cards();
        assert!(cards[2].rows.is_empty());
        assert!(cards[1]
            .pinned
            .iter()
            .any(|row| row.conversation_id == filed_id));
        assert_eq!(cards[1].unread, 2);
    }

    #[test]
    fn sidebar_cards_short_name_uses_the_bare_dm_display_name() {
        let mut dm = ChatRowSendData::new(ConversationType::AnonDm, "dm", "Ada Lovelace");
        dm.label = crate::ui::sidebar_data::ChatRowLabel::IncomingSecretRequest;
        let dm = SidebarCardRow::from(dm);
        assert_eq!(dm.short_name, "Ada");
        assert_ne!(dm.short_name, dm.name);
        let channel = SidebarCardRow::from(ChatRowSendData::new(
            ConversationType::Channel,
            "channel",
            "Flight Operations",
        ));
        assert_eq!(channel.short_name, "Flight Operations");
    }
}
