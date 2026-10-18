use super::*;

impl MessageService {
    /// Mark all unread messages in a conversation as read (sets is_read=1).
    /// Callers use this when the user is actively viewing a conversation so new
    /// incoming messages don't bump the unread badge.
    /// Mark a conversation read and report the boundary that was cleared.
    /// See [`crate::domain::message::UnreadAnchor`] for why the two are one call.
    pub fn mark_conversation_read(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<crate::domain::message::UnreadAnchor, MessageError> {
        let (anchor, read) = self
            .message_repo
            .mark_conversation_read_returning(conversation_id, conversation_type)?;
        self.sync_read(conversation_id, conversation_type, read);
        // Unconditional: an anonymous message has no sender, so `read` can be
        // empty while its notification is still up.
        self.event_bus
            .emit_domain(crate::events::DomainEvent::ConversationsRead {
                conversation_ids: vec![*conversation_id],
                from_other_device: false,
            });
        Ok(anchor)
    }

    /// Tell our other devices what was just read here (Signal's
    /// `SyncMessage.Read`), whatever the read-receipt setting — that setting
    /// is about what the sender learns, not our own devices. Friend DMs and
    /// channels only: anonymous conversations stay off our own devices' wire.
    /// Fire-and-forget; a failed sync leaves the other devices unread.
    fn sync_read(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
        read: Vec<(UserId, u64)>,
    ) {
        use crate::proto::content_v1::{Content, ReadSync, ReadSyncEntry, content};

        if !matches!(
            conversation_type,
            ConversationType::ServerDm | ConversationType::Channel
        ) {
            return;
        }
        let Ok(me) = self.current_user_id() else {
            return;
        };
        let messages: Vec<ReadSyncEntry> = read
            .into_iter()
            .filter(|(sender, _)| *sender != me)
            .map(|(sender, sent_at)| ReadSyncEntry {
                conversation_id: conversation_id.to_string(),
                sender_aci: sender.to_string(),
                sent_timestamp: sent_at,
            })
            .collect();
        if messages.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("ReadSync: no runtime to send on; other devices stay unread");
            return;
        };
        let sync = Content {
            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
            content: Some(content::Content::ReadSync(ReadSync { messages })),
            ..Default::default()
        };
        let service = self.clone();
        runtime.spawn(async move {
            let devices = service
                .sync_to_own_devices(&mut service.auth_client(), sync)
                .await;
            tracing::debug!(devices, "ReadSync sent to our other devices");
        });
    }

    /// Apply a `ReadSync` from another of our devices. Returns the
    /// conversations whose read horizons were applied. Sends nothing back:
    /// only a local read (`mark_conversation_read`) syncs.
    pub fn apply_read_sync(
        &self,
        sync: &crate::proto::content_v1::ReadSync,
    ) -> Result<Vec<ChannelId>, MessageError> {
        let me = self.current_user_id()?;
        let reads: Vec<(ChannelId, UserId, u64)> = sync
            .messages
            .iter()
            .filter_map(|m| {
                Some((
                    m.conversation_id.parse().ok()?,
                    m.sender_aci.parse().ok()?,
                    m.sent_timestamp,
                ))
            })
            .collect();
        Ok(self.message_repo.mark_read_synced(&reads, &me)?)
    }

    /// Set the pinned status of a message in local storage.
    pub fn set_message_pinned(
        &self,
        message_id: &crate::domain::ids::MessageId,
        is_pinned: bool,
    ) -> Result<(), MessageError> {
        if let Some(mut message) = self.message_repo.get(message_id)? {
            message.is_pinned = is_pinned;
            message.updated_at = chrono::Utc::now();
            self.message_repo.update(&message)?;
        }
        Ok(())
    }

}
