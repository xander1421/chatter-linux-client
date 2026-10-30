use super::*;

impl ConversationHandle {
    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Look up a message by sender + timestamp (for standard DM/Channel paths).
    /// Persist an edit to local DB (for sender's own edits).
    pub(super) fn persist_edit(&self, sent_timestamp: u64, new_text: &str) {
        if let Some(user_id) = self.client.auth_state().get().current_user_id {
            if let Ok(msg) = self.lookup_message(&user_id, sent_timestamp) {
                let new_content =
                    crate::domain::message::MessageContent::Text(new_text.to_string());
                let _ = self
                    .client
                    .messages()
                    .update_message_body(&msg.id, &new_content);
            }
        }
    }

    /// The stored row at this handle's storage scope and Signal-style key
    /// (kind, conversation, author, sent clock), undeleted, as it is now. A
    /// UI closure publishing a Saved change reads this — not the event's
    /// flag or the tap's intent — so a stale completion cannot repaint a
    /// state the row has since left. Foreign, deleted or missing: `Err`.
    pub fn stored_message_scoped(
        &self,
        author: &UserId,
        sent_at: u64,
    ) -> ConversationResult<crate::domain::Message> {
        self.lookup_message(author, sent_at)
    }

    pub(super) fn lookup_message(
        &self,
        sender_id: &UserId,
        sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        scoped_stored_message(
            &self.client.messages().message_repo,
            &self.storage_scope()?,
            sender_id,
            sent_timestamp,
        )
    }

    /// Send content to a friend DM. The transport (Shield: mailbox; normal:
    /// sealed-sender streaming) is `send_friend_dm_content`'s decision.
    pub(super) async fn send_friend_dm_content(
        &self,
        content: crate::proto::content_v1::Content,
        peer_id: &UserId,
    ) -> ConversationResult {
        self.client
            .messages()
            .send_friend_dm_content(peer_id, content)
            .await
            .map_err(ConversationError::Message)
    }

    /// Send control content via channel (sender keys).
    pub(super) async fn send_channel_content(
        &self,
        content: crate::proto::content_v1::Content,
        channel_id: &ChannelId,
    ) -> ConversationResult {
        self.client
            .messages()
            .send_control(channel_id, content, None)
            .await
            .map(|_| ())
            .map_err(ConversationError::Message)
    }

    /// The active channel's id as a string, or `None` for DM conversations.
    /// Channel builders embed it for routing; DM builders leave it empty.
    pub(super) fn active_channel_id(&self) -> Option<String> {
        match &self.conversation {
            ActiveConversation::Channel(cid) => Some(cid.to_string()),
            _ => None,
        }
    }

    /// The conversation-id payload field for delete-for-me sync: the peer's
    /// id for DMs, the channel id for channels.
    pub(super) fn dm_or_channel_id(&self) -> String {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => peer_id.to_string(),
            ActiveConversation::Channel(cid) => cid.to_string(),
            _ => String::new(),
        }
    }

    /// Route already-built `Content` to the transport for the active DM or
    /// channel conversation. Valid only for ServerDm / Channel;
    /// anon and None return `NoConversation`.
    pub(super) async fn dispatch_content(
        &self,
        content: crate::proto::content_v1::Content,
    ) -> ConversationResult {
        match &self.conversation {
            ActiveConversation::ServerDm { peer_id } => {
                self.send_friend_dm_content(content, peer_id).await
            }
            ActiveConversation::Channel(channel_id) => {
                self.send_channel_content(content, channel_id).await
            }
            _ => Err(ConversationError::NoConversation),
        }
    }
}
