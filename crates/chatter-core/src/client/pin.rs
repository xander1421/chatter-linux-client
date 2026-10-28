//! pin façade — thin delegates to services.

use super::ChatterClient;

/// One row in the pinned-messages sheet, resolved from stored pins.
pub struct PinnedDisplayItem {
    pub message_id: crate::domain::ids::MessageId,
    pub preview: String,
    pub sender_name: String,
    pub sent_time: String,
    pub thumbnail: Vec<u8>,
}

impl ChatterClient {
    pub fn clear_pinned(&self) {
        self.services.chat_state.clear_pinned();
    }

    pub fn pinned_should_show(&self) -> bool {
        self.services.chat_state.pinned_should_show()
    }

    pub fn pinned_navigate_prev(&self) {
        self.services.chat_state.pinned_navigate_prev();
    }

    pub fn pinned_navigate_next(&self) {
        self.services.chat_state.pinned_navigate_next();
    }

    pub fn pinned_navigate_to(&self, index: usize) {
        self.services.chat_state.pinned_navigate_to(index);
    }

    pub fn pinned_dismiss(&self) {
        self.services.chat_state.pinned_dismiss();
    }

    pub fn set_pinned_messages(&self, ids: Vec<crate::domain::ids::MessageId>) {
        self.services.chat_state.set_pinned_messages(ids);
    }

    pub fn pinned_current_message_id(&self) -> Option<crate::domain::ids::MessageId> {
        self.services.chat_state.pinned_current_message_id()
    }

    pub fn pinned_total_count(&self) -> usize {
        self.services.chat_state.pinned_total_count()
    }

    pub fn add_pinned_message(&self, message_id: crate::domain::ids::MessageId) {
        self.services.chat_state.add_pinned_message(message_id);
    }

    pub fn remove_pinned_message(&self, message_id: &crate::domain::ids::MessageId) {
        self.services.chat_state.remove_pinned_message(message_id);
    }

    /// Drop pins whose message is no longer stored (deleted for everyone,
    /// deleted for me, expired). Returns whether any were dropped. The pinned
    /// bar kept showing a deleted message until the conversation was
    /// reopened. Private-group messages live in their own store and are left
    /// to the group's pin events.
    pub fn prune_deleted_pins(&self) -> bool {
        if matches!(self.active_conversation(), crate::state::ActiveConversation::AnonGroup(_)) {
            return false;
        }
        let gone: Vec<_> = self
            .chat_snapshot()
            .pinned
            .pinned_message_ids
            .into_iter()
            .filter(|id| self.get_message_local(id).is_none_or(|m| m.is_deleted))
            .collect();
        for id in &gone {
            self.services.chat_state.remove_pinned_message(id);
        }
        !gone.is_empty()
    }

    /// Every stored pin, newest pin-added first, regardless of loaded history.
    pub async fn pinned_display_items(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> Vec<PinnedDisplayItem> {
        let Some((mut kind, id)) = conversation.routing_key() else {
            return Vec::new();
        };
        if let crate::state::ActiveConversation::Channel(channel_id) = conversation {
            kind = self.channel_conversation_type(&channel_id);
        }
        let Ok(id) = id.parse() else {
            return Vec::new();
        };
        let pins = self.load_pinned_for_channel(&id, &kind)
            .await
            .unwrap_or_default();
        let messages: Vec<_> = pins
            .iter()
            .filter_map(|pin| self.get_message_local(&pin.message_id))
            .collect();
        let names = self.search_sender_display_names(&messages);
        let mut details: std::collections::HashMap<_, _> = messages
            .into_iter()
            .zip(names)
            .map(|(message, sender_name)| {
                (
                    message.id,
                    (
                        sender_name,
                        crate::util::time::format_detail_time(message.created_at),
                    ),
                )
            })
            .collect();
        pins.into_iter()
            .filter_map(|pin| {
                let (sender_name, sent_time) = details.remove(&pin.message_id)?;
                Some(PinnedDisplayItem {
                    message_id: pin.message_id,
                    preview: pin.preview,
                    sender_name,
                    sent_time,
                    thumbnail: pin.thumbnail,
                })
            })
            .collect()
    }

    // ── Pin / pinned messages façade ────────────────────────────────────

    pub async fn load_pinned_for_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<
        Vec<crate::services::pin_service::PinnedMessageData>,
        crate::services::pin_service::PinError,
    > {
        self.pins()
            .load_pinned_for_channel(channel_id, conversation_type)
            .await
    }

    pub fn get_pinned_message(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Option<crate::services::pin_service::PinnedMessageData> {
        self.pins().get_pinned_message(message_id).ok().flatten()
    }

    /// Look up a stored anon group message by id.
    pub fn get_anon_group_message(
        &self,
        id: &crate::domain::anon_group::AnonGroupMessageId,
    ) -> Option<crate::domain::message::Message> {
        self.anon_groups().get_message(id).ok().flatten()
    }
}
