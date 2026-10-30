use super::*;

impl MessageService {
    /// Classify a raw `(channel_id, conversation_type, peer_id)` triple into
    /// the typed `ActiveConversation` variant it represents.
    ///
    /// - AnonDm/AnonGroup `ConversationType`s map straight to their variants.
    /// - ServerDm → channel-free `ServerDm { peer_id }` (the peer is the
    ///   passed `peer_id`, or the `channel_id`'s UUID which equals it).
    /// - Otherwise → `Channel`.
    ///
    /// Lives here because UI callers used to inline this across multiple
    /// feature handlers.
    /// Derive the `ConversationType` for a channel/DM id from the local channel
    /// row. Defaults to `Channel` when the row isn't synced yet. Thin accessor
    /// so callers outside `message/` (e.g. call-history storage) don't reach
    /// into `channel_repo` directly.
    pub fn conversation_type(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> crate::domain::message::ConversationType {
        conversion::conversation_type_for_channel(&self.channel_repo, channel_id)
    }

    pub fn classify_active_conversation(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        peer_id: Option<crate::domain::ids::UserId>,
    ) -> crate::state::ActiveConversation {
        use crate::domain::message::ConversationType;
        use crate::state::ActiveConversation;

        match conversation_type {
            ConversationType::AnonDm => ActiveConversation::AnonDm(
                crate::domain::anon_dm::AnonDmId::from_uuid(*channel_id.as_uuid()),
            ),
            ConversationType::AnonGroup => ActiveConversation::AnonGroup(
                crate::domain::anon_group::AnonGroupId::from_uuid(*channel_id.as_uuid()),
            ),
            ConversationType::ServerDm => {
                // Channel-free DM: keyed by peer. conversation_id == peer
                // user_id, so recover the peer from either source.
                let peer = peer_id.unwrap_or_else(|| {
                    crate::domain::ids::UserId::from_uuid(*channel_id.as_uuid())
                });
                ActiveConversation::ServerDm { peer_id: peer }
            }
            _ => ActiveConversation::Channel(*channel_id),
        }
    }

    /// Check if the locally-logged-in user has reacted with an emoji on a message.
    ///
    /// Signal-style message identification via (sender_id, sent_at_ns). Reads
    /// the per-user blob cache on the message row — authoritative for both
    /// anon (rebuilt locally) and regular (delivered from server) paths.
    pub fn user_has_reacted(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> Result<bool, MessageError> {
        self.message_repo
            .user_has_reacted(scope, sender_id, sent_at_ns, emoji)
            .map_err(MessageError::from)
    }

    /// Whether a channel requires end-to-end encryption.
    ///
    /// Looks up channel locally first, fetches from server if not found.
    pub(crate) async fn channel_requires_encryption(
        &self,
        channel_id: &ChannelId,
        channel_client: &mut ChannelClient,
    ) -> Result<bool, MessageError> {
        // Try local lookup first
        if let Ok(Some(channel)) = self.channel_repo.get(channel_id) {
            return Ok(channel.config.require_encryption);
        }

        // Not found locally - fetch from server
        tracing::debug!(
            channel_id = %channel_id,
            "Channel not in local repo, fetching from server"
        );

        let proto_channel = channel_client.get_channel(&channel_id.to_string()).await?;
        Ok(proto_channel.config.map(|c| c.encrypted).unwrap_or(false))
    }
}
