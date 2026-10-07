//! pin façade — thin delegates to services.

use super::ChatterClient;

impl ChatterClient {
    /// Every stored pin, newest pin-added first, regardless of loaded history.
    pub async fn pinned_display_items(
        &self,
        conversation: crate::state::ActiveConversation,
    ) -> Vec<crate::services::pin_service::PinnedMessageData> {
        let Some((mut kind, id)) = conversation.routing_key() else {
            return Vec::new();
        };
        if let crate::state::ActiveConversation::Channel(channel_id) = conversation {
            kind = self.channel_conversation_type(&channel_id);
        }
        let Ok(id) = id.parse() else {
            return Vec::new();
        };
        self.load_pinned_for_channel(&id, &kind)
            .await
            .unwrap_or_default()
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
