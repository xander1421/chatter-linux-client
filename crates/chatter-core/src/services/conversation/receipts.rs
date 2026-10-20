use super::*;

impl ConversationHandle {
    // ========================================================================
    // Receipts
    // ========================================================================

    pub async fn send_read_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        self.send_receipt_with_recovery(timestamps, true).await
    }

    pub(super) async fn send_read_receipt_once(&self, timestamps: &[u64]) -> ConversationResult {
        if timestamps.is_empty() {
            return Ok(());
        }
        // Nothing is read while the app is off screen (backgrounded, locked,
        // unfocused) — even with this chat open behind it.
        if !self.client.chat_state().is_app_visible() {
            return Ok(());
        }
        // Privacy gate: honours the local "send read receipts" setting. When
        // disabled (`read_receipts_enabled() == false`) we do NOT tell the peer
        // we've read their message. Fail-safe default is ON (shipped default +
        // getter `unwrap_or(true)`).
        if !self.client.read_receipts_enabled().unwrap_or(true) {
            return Ok(());
        }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => self
                .client
                .anon_dms()
                .send_read_receipt(dm_id, timestamps.to_vec())
                .await
                .map_err(ConversationError::AnonDm),
            // Groups don't track per-user read state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            ActiveConversation::Channel(channel) => self
                .client
                .messages()
                .send_channel_receipt(*channel, timestamps, true)
                .await
                .map_err(ConversationError::Message),
            _ => {
                let content = crate::services::gateway::build_read_receipt_content_with_channel(
                    timestamps.to_vec(),
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    pub async fn send_delivery_receipt(&self, timestamps: &[u64]) -> ConversationResult {
        self.send_receipt_with_recovery(timestamps, false).await
    }

    pub(super) async fn send_delivery_receipt_once(
        &self,
        timestamps: &[u64],
    ) -> ConversationResult {
        if timestamps.is_empty() {
            return Ok(());
        }
        match &self.conversation {
            ActiveConversation::AnonDm(dm_id) => self
                .client
                .anon_dms()
                .send_delivery_receipt(dm_id, timestamps.to_vec())
                .await
                .map_err(ConversationError::AnonDm),
            // Groups don't track per-user delivery state to preserve sender anonymity.
            ActiveConversation::AnonGroup(_) => Ok(()),
            ActiveConversation::None => Err(ConversationError::NoConversation),
            // Bound inside the attempt, so initial sends AND pending recovery
            // return a retryable error instead of monopolizing either queue.
            ActiveConversation::Channel(channel) => tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.client
                    .messages()
                    .send_channel_receipt(*channel, timestamps, false),
            )
            .await
            .unwrap_or_else(|_| {
                Err(crate::services::MessageError::Network(
                    crate::error::NetworkError::Timeout,
                ))
            })
            .map_err(ConversationError::Message),
            _ => {
                let content = crate::services::gateway::build_delivery_receipt_content_with_channel(
                    timestamps.to_vec(),
                    self.active_channel_id().as_deref(),
                );
                self.dispatch_content(content).await
            }
        }
    }

    pub(super) async fn send_receipt_with_recovery(
        &self,
        timestamps: &[u64],
        read: bool,
    ) -> ConversationResult {
        if timestamps.is_empty() || matches!(self.conversation, ActiveConversation::AnonGroup(_)) {
            return Ok(());
        }
        if read
            && (!self.client.chat_state().is_app_visible()
                || !self.client.read_receipts_enabled().unwrap_or(true))
        {
            return Ok(());
        }
        let auth = self.client.auth_state().get();
        let shield = self.client.anon_mode_enabled();
        let delivery_owner = if !read && matches!(self.conversation, ActiveConversation::Channel(_))
        {
            auth.current_user_id
        } else {
            None
        };
        if delivery_owner.is_some() {
            for entry in self.client.pending_receipts.lock().iter_mut() {
                if entry.delivery_owner == delivery_owner {
                    entry.revision = auth.session_revision;
                }
            }
            for entry in self.client.sent_receipts.lock().iter_mut() {
                if entry.delivery_owner == delivery_owner {
                    entry.revision = auth.session_revision;
                }
            }
        }
        // A chat reopen may load the same page while a failed receipt is
        // already queued. Let recovery own that timestamp instead of sending
        // another immediate deposit around its backoff.
        let queued = self
            .client
            .pending_receipts
            .lock()
            .iter()
            .find(|entry| {
                entry.conversation == self.conversation
                    && entry.read == read
                    && entry.revision == auth.session_revision
                    && entry.shield == shield
            })
            .map(|entry| entry.timestamps.clone())
            .unwrap_or_default();
        let unsent: Vec<u64> = {
            let sent = self.client.sent_receipts.lock();
            let previous = sent.iter().find(|entry| {
                entry.conversation == self.conversation
                    && entry.read == read
                    && entry.revision == auth.session_revision
            });
            timestamps
                .iter()
                .copied()
                .filter(|timestamp| {
                    !queued.contains(timestamp)
                        && !previous.is_some_and(|entry| entry.timestamps.contains(timestamp))
                })
                .collect()
        };
        if unsent.is_empty() {
            return Ok(());
        }
        let result = if read {
            self.send_read_receipt_once(&unsent).await
        } else {
            self.send_delivery_receipt_once(&unsent).await
        };
        let current = self.client.auth_state().get();
        let same_delivery_owner = delivery_owner.is_some()
            && delivery_owner == current.current_user_id
            && !current.session_ended;
        if auth.current_user_id.is_none()
            || (auth.session_revision != current.session_revision && !same_delivery_owner)
        {
            return result;
        }
        if result.is_ok() {
            record_sent_receipt(
                &self.client,
                &PendingReceipt {
                    conversation: self.conversation,
                    read,
                    revision: current.session_revision,
                    delivery_owner,
                    shield,
                    timestamps: unsent.iter().copied().collect(),
                },
            );
        }
        let mut pending = self.client.pending_receipts.lock();
        if let Some(entry) = pending.iter_mut().find(|entry| {
            entry.conversation == self.conversation
                && entry.read == read
                && entry.revision == current.session_revision
                && entry.shield == shield
        }) {
            if result.as_ref().err().is_some_and(retryable_receipt_error) {
                entry.timestamps.extend(unsent.iter().copied());
            } else if result.is_ok() {
                for timestamp in &unsent {
                    entry.timestamps.remove(timestamp);
                }
            }
        } else if result.as_ref().err().is_some_and(retryable_receipt_error) {
            pending.push(PendingReceipt {
                conversation: self.conversation,
                read,
                revision: current.session_revision,
                delivery_owner,
                shield,
                timestamps: unsent.iter().copied().collect(),
            });
        }
        pending.retain(|entry| !entry.timestamps.is_empty());
        result
    }

    /// A persisted incoming message is read only while its conversation is on
    /// screen. Reuse the open-chat read path for local state, linked-device
    /// sync, and peer receipts (including the receipt privacy setting).
    pub(crate) async fn mark_received_as_read(
        client: &Arc<ChatterClient>,
        message: &crate::domain::message::Message,
    ) -> Result<(), ConversationError> {
        let active = client.chat_state().active_conversation();
        if client.chat_state().is_app_visible() && active.is_for_channel(message.conversation_id) {
            client.conversation(active).mark_as_read().await?;
        }
        Ok(())
    }

    /// Mark all messages in this conversation as read.
    ///
    /// Two halves, and the sidebar only ever sees the first one: the unread
    /// badge is a `COUNT(is_read = 0)` over the messages table (see
    /// `list_regular_dm_sidebar`, `channel_repo`, `conversation_stats`), so a
    /// receipt on the wire clears nothing locally — the badge came back on the
    /// next rebuild and survived restarts. The local write covers every
    /// conversation type; only the receipt is per-transport, and anon groups
    /// have no receipt at all (per-user read state would defeat the sender
    /// anonymity the group is for).
    /// Returns the [`UnreadAnchor`] that was just cleared, so the caller can
    /// open the conversation where the user left off. Read-receipt failures do
    /// not swallow it — the anchor is the caller's only chance at the boundary.
    pub async fn mark_as_read(&self) -> Result<UnreadAnchor, ConversationError> {
        let anchor = self.mark_read_local()?;
        if let Some((conversation_id, conversation_type)) = self.receipt_target() {
            if let Ok(page) = self.client.messages().get_messages_local(
                &conversation_id,
                &conversation_type,
                None,
                crate::config::MESSAGE_PAGE_SIZE as u32,
            ) {
                self.send_read_receipts_for_page(&page);
            }
        }
        Ok(anchor)
    }

    /// The local half of [`Self::mark_as_read`]: clears `is_read` and returns
    /// the anchor, sending nothing. A conversation open calls this, loads its
    /// newest page, and hands that page to
    /// [`Self::send_read_receipts_for_page`] — the same rows `mark_as_read`
    /// would have queried a second time.
    pub fn mark_read_local(&self) -> Result<UnreadAnchor, ConversationError> {
        // Off screen, nothing has been read. `ChatterClient::set_app_visible`
        // re-runs this for the open conversation when the app comes back.
        if !self.client.chat_state().is_app_visible() {
            return Ok(UnreadAnchor::default());
        }
        let (Some(conversation_id), Some(conversation_type)) = (
            self.conversation.channel_id(),
            self.client.conversation_type_of(&self.conversation),
        ) else {
            return Err(ConversationError::NoConversation);
        };
        Ok(self
            .client
            .messages()
            .mark_conversation_read(&conversation_id, &conversation_type)?)
    }

    /// Where a read receipt for this conversation goes, if one goes at all:
    /// nothing while off screen, and anon groups have no receipt (per-user read
    /// state would defeat the sender anonymity the group is for).
    pub(super) fn receipt_target(
        &self,
    ) -> Option<(
        crate::domain::ids::ChannelId,
        crate::domain::message::ConversationType,
    )> {
        if !self.client.chat_state().is_app_visible()
            || matches!(self.conversation, ActiveConversation::AnonGroup(_))
        {
            return None;
        }
        Some((
            self.conversation.channel_id()?,
            self.client.conversation_type_of(&self.conversation)?,
        ))
    }

    /// Receipt the peer's messages in `page` — the conversation's newest page,
    /// as the open just loaded it. Receipting our own back to ourselves is
    /// noise. `send_read_receipt` holds the privacy gate and picks the
    /// transport.
    pub fn send_read_receipts_for_page(&self, page: &[crate::domain::message::Message]) {
        if self.receipt_target().is_none() {
            return;
        }
        let my_user_id = self.client.auth_state().get().current_user_id;
        let timestamps: Vec<u64> = page
            .iter()
            .filter(|m| m.sender_id != my_user_id)
            .map(|m| m.sent_at)
            .collect();
        // Off the caller's path: a receipt is a network send (session, sealed
        // sender, or the Shield mailbox). Awaited, a slow peer held the
        // previous conversation on screen until it finished.
        let handle = self.clone();
        tokio::spawn(async move {
            let _ = handle.send_read_receipt(&timestamps).await;
        });
    }
}
