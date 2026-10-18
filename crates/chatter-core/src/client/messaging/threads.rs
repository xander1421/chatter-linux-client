use super::*;

impl ChatterClient {
    /// Reply count per thread root in a conversation, for the "N replies"
    /// indicator on root message bubbles.
    pub fn thread_reply_counts(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> std::collections::HashMap<crate::domain::ids::MessageId, u32> {
        self.messages()
            .thread_reply_counts(channel_id, conversation_type)
            .unwrap_or_default()
    }

    /// The newest `limit` replies before `before` (or overall), oldest →
    /// newest. Page backwards by passing the first returned reply's id.
    pub fn list_thread_replies(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages()
            .list_for_thread(thread_root_id, before, limit)
    }

    /// Total replies in a thread, so the panel knows whether earlier ones
    /// remain to be loaded.
    pub fn thread_reply_count(&self, thread_root_id: &crate::domain::ids::MessageId) -> u32 {
        self.messages()
            .count_for_thread(thread_root_id)
            .unwrap_or(0)
    }

    /// Send a channel/DM message (convenience façade over `MessageService::send`).
    #[allow(clippy::too_many_arguments)]
    pub async fn send_channel_message(
        &self,
        group_fanout: Option<&crate::mailbox::group::GroupFanout>,
        channel_id: &crate::domain::ids::ChannelId,
        text: &str,
        attachment_ids: Vec<crate::domain::ids::AttachmentId>,
        attachment_pointers: Vec<crate::proto::content_v1::AttachmentPointer>,
        thread_id: Option<&crate::domain::ids::MessageId>,
        mentioned_integrations: Vec<crate::domain::ids::IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<crate::domain::message::Message, crate::services::message::MessageError> {
        self.messages()
            .send(
                group_fanout,
                channel_id,
                text,
                attachment_ids,
                attachment_pointers,
                thread_id,
                mentioned_integrations,
                targets_ai_bot,
                None, // thread replies use parent_id, not a reply quote
            )
            .await
    }

    /// Signal-style profile-key handshake, fired once on both sides right
    /// after friend-accept so each peer's real display name + avatar appear
    /// immediately, with zero visible messages. See
    /// `MessageService::send_profile_key_handshake`.
    pub async fn send_profile_key_handshake(
        &self,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::message::MessageError> {
        self.messages().send_profile_key_handshake(peer_id).await
    }
}
