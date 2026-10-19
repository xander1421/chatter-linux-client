use super::*;

impl ChatterClient {
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
        for section in sections.iter_mut() {
            section.conversations.retain(|c| {
                self.visible_in_mode(&c.conversation_type)
                    && !self.is_blocked_conversation(&c.conversation_type, &c.conversation_id)
            });
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
    ) -> Result<crate::domain::section::CustomSection, crate::services::section_service::SectionError>
    {
        self.sections().create_section(name, emoji)
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
            ConversationType::SavedMessages => "Saved Messages".to_string(),
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
        if self.anon_mode_enabled() {
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
