use super::*;

impl ChatterClient {
    pub fn clear_typing(&self, channel: &crate::domain::ids::ChannelId) {
        self.services.chat_state.clear_typing(channel);
    }

    /// Remove rows already displayed before inserting an older history page.
    pub fn unseen_history_rows<T>(
        &self,
        rows: impl IntoIterator<Item = T>,
        visible_ids: impl IntoIterator<Item = String>,
        row_id: impl Fn(&T) -> &str,
    ) -> Vec<T> {
        crate::services::message::MessageService::unseen_history_rows(rows, visible_ids, row_id)
    }

    // ── Messages façade ──────────────────────────────────────────────────

    /// Execute a selected-message delete using the captured account/session.
    pub async fn delete_selected_messages(
        self: &Arc<Self>,
        conversation: ActiveConversation,
        ids: &[String],
        scope: crate::services::conversation::SelectedDeleteScope,
        account: crate::domain::ids::UserId,
        session_revision: u64,
        completed: tokio::sync::mpsc::UnboundedSender<Vec<(crate::domain::ids::UserId, u64)>>,
    ) -> crate::services::conversation::SelectedDeleteResult {
        self.conversation(conversation)
            .delete_selected(ids, scope, account, session_revision, completed)
            .await
    }

    pub fn get_messages_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages()
            .get_messages_local(channel_id, conversation_type, before, limit)
    }

    pub fn get_messages_local_asc(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages()
            .get_messages_local_asc(channel_id, conversation_type, before, limit)
    }

    /// See [`crate::services::message::MessageService::get_older_messages_local_asc`].
    pub fn get_older_messages_local_asc(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        before: &crate::domain::ids::MessageId,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages().get_older_messages_local_asc(
            channel_id,
            conversation_type,
            before,
            crate::config::MESSAGE_PAGE_SIZE as u32,
        )
    }

    pub fn get_newer_messages_local_asc(
        &self, channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        after: &crate::domain::ids::MessageId,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages().get_newer_messages_local_asc(channel_id,conversation_type,after,crate::config::MESSAGE_PAGE_SIZE as u32)
    }

    /// See [`crate::services::message::MessageService::get_messages_local_reaching`].
    pub fn get_messages_local_reaching(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        target: &crate::domain::ids::MessageId,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages().get_messages_local_reaching(
            channel_id,
            conversation_type,
            target,
            crate::config::MESSAGE_PAGE_SIZE as u32,
        )
    }

    pub fn get_message_by_sender_timestamp(
        &self,
        sender_id: &crate::domain::ids::UserId,
        sent_at: u64,
    ) -> Option<crate::domain::message::Message> {
        self.messages()
            .get_by_sender_timestamp(sender_id, sent_at)
            .ok()
            .flatten()
    }

    pub fn mark_conversation_read(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<crate::domain::message::UnreadAnchor, crate::services::message::MessageError> {
        // Off screen nothing is read; `set_app_visible(true)` catches up.
        if !self.chat_state().is_app_visible() {
            return Ok(Default::default());
        }
        self.messages()
            .mark_conversation_read(conversation_id, conversation_type)
    }

    pub fn classify_active_conversation(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        peer_id: Option<crate::domain::ids::UserId>,
    ) -> crate::state::ActiveConversation {
        self.messages()
            .classify_active_conversation(channel_id, conversation_type, peer_id)
    }

    pub fn user_has_reacted(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        sender_id: &crate::domain::ids::UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> bool {
        self.messages()
            .user_has_reacted(scope, sender_id, sent_at_ns, emoji)
            .unwrap_or(false)
    }

    /// Resolve selected IDs against current storage and conversation, in the
    /// caller's display order. A missing or foreign row invalidates the set.
    pub fn selected_message_targets(
        &self,
        conversation: ActiveConversation,
        ids: &[String],
    ) -> Option<Vec<(crate::domain::ids::UserId, u64)>> {
        if ids.is_empty() || conversation.channel_id().is_none() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        ids.iter()
            .map(|id| {
                if !seen.insert(id) {
                    return None;
                }
                let id = id.parse().ok()?;
                let message = self.get_message_local(&id)?;
                if Some(message.conversation_id) != conversation.channel_id() {
                    return None;
                }
                Some((message.sender_id?, message.sent_at))
            })
            .collect()
    }

    pub async fn selection_snapshot(
        self: &Arc<Self>,
        conversation: ActiveConversation,
        ids: Vec<String>,
    ) -> Option<SelectionSnapshot> {
        let client = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let targets = client.selected_message_targets(conversation, &ids)?;
            let handle = client.conversation(conversation);
            let can_delete_everyone = targets.iter().all(|(author, ts)| {
                handle.can_delete_for_everyone(author, *ts).unwrap_or(false)
            });
            let copy_text = targets
                .iter()
                .filter_map(|(author, ts)| handle.get_message_text(author, *ts).ok().flatten())
                .collect::<Vec<_>>()
                .join("\n\n");
            Some(SelectionSnapshot {
                actions: SelectionActions {
                    can_delete_everyone,
                    can_forward: handle.can_forward_from(),
                },
                targets,
                copy_text,
            })
        })
        .await
        .ok()
        .flatten()
    }

    pub fn get_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Option<crate::domain::message::Message> {
        self.messages().get_message_local(message_id).ok().flatten()
    }

    /// Timestamp of an outgoing failed, parked, or retained secret post eligible for retry.
    pub fn retryable_message_timestamp(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Option<u64> {
        let message = self.get_message_local(message_id)?;
        let me = self.session_current_user_id()?;
        (message.sender_id == Some(me)
            && match message.conversation_type {
                ConversationType::Channel | ConversationType::ServerDm => {
                    matches!(message.delivery_status.as_str(), "failed" | "parked")
                }
                ConversationType::AnonDm | ConversationType::AnonGroup
                    if crate::services::gateway::submission::retained_row(
                        &self.services.database,
                        &me.to_string(),
                        &message.id.to_string(),
                    ) =>
                {
                    true
                }
                ConversationType::AnonDm => {
                    message.delivery_status == "failed"
                        && message.failure_reason.as_deref() == Some("Message not submitted")
                }
                ConversationType::AnonGroup
                | ConversationType::SavedMessages
                | ConversationType::Mesh
                | ConversationType::MeshDm => false,
            }
            && !message.is_deleted)
            .then_some(message.sent_at)
    }

    /// Whether the parent message a reply quotes is missing or deleted.
    ///
    /// Signal-Desktop parity: drives the "Original message not found" warning
    /// under a reply's quoted snapshot. See `MessageService::reply_parent_missing`.
    pub fn reply_parent_missing(&self, parent: &crate::domain::ids::MessageId) -> bool {
        self.messages().reply_parent_missing(parent)
    }
}
