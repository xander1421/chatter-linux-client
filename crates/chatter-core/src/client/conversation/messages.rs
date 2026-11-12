use super::*;

impl ConversationHandle {
    /// The newest own main-list message for which the context menu currently offers Edit.
    /// Search persisted history so the shortcut also finds messages outside the
    /// loaded UI page, and use the menu rule (including Saved's age exception).
    pub fn last_editable_own_message(&self) -> ConversationResult<Option<(UserId, u64)>> {
        let Some(own) = self.client.auth_state().get().current_user_id else {
            return Ok(None);
        };
        let scope = self.storage_scope()?;
        let now_ns = crate::util::time::TimestampNanos::now().as_u64();
        let messages = self.client.messages();
        let repo = &messages.message_repo;
        let mut before = None;
        loop {
            let page = repo.list_for_conversation(&scope.id, &scope.kind, before, 100)?;
            for message in &page {
                // A thread reply is not in the main list: Up there must not
                // reach into a thread and edit a message nobody can see.
                if message.sender_id == Some(own)
                    && message.thread_id.is_none()
                    && stored_message_menu(message, true, now_ns).actions.edit
                {
                    return Ok(Some((own, message.sent_at)));
                }
            }
            if scope.kind != ConversationType::SavedMessages
                && page.last().is_some_and(|message| {
                    now_ns.saturating_sub(message.sent_at)
                        > crate::domain::message::REMOTE_ACTION_WINDOW_NS
                })
            {
                return Ok(None);
            }
            if page.len() < 100 {
                return Ok(None);
            }
            before = page.last().map(|message| message.sent_at);
        }
    }

    /// The newest own reply in the thread under `root` that the menu would
    /// let us edit: Up in the thread composer.
    pub fn last_editable_own_thread_reply(
        &self,
        root: &crate::domain::ids::MessageId,
    ) -> ConversationResult<Option<(UserId, u64)>> {
        let Some(own) = self.client.auth_state().get().current_user_id else {
            return Ok(None);
        };
        let now_ns = crate::util::time::TimestampNanos::now().as_u64();
        let replies = self.client.messages().message_repo.list_for_thread(root, None, 100)?;
        Ok(replies
            .iter()
            .rev()
            .find(|m| m.sender_id == Some(own) && stored_message_menu(m, true, now_ns).actions.edit)
            .map(|m| (own, m.sent_at)))
    }

    /// Get message text by sender + timestamp across any conversation type.
    pub fn get_message_text(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<Option<String>> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => Ok(msg.as_text().map(|s| s.to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                // Use timestamp-only lookup: the 3-field lookup (group_id + sender_id + sent_at)
                // fails due to sender_id byte encoding mismatch between storage and lookup paths.
                // Nanosecond timestamps are unique per group so timestamp-only is safe.
                match self
                    .client
                    .anon_groups()
                    .get_message_by_timestamp(id, target_sent_timestamp)
                {
                    Ok(Some(msg)) => Ok(msg.as_text().map(|s| s.to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                Ok(msg.as_text().map(|s| s.to_string()))
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Forward a message from this conversation to another one.
    ///
    /// Re-sends the source message's text AND its attachments to `target` as a
    /// fresh message (Signal-style — no forwarding envelope, the recipient sees
    /// a normal message). Attachments ride the pointer they were already
    /// uploaded with, so forwarding a photo costs no second upload.
    ///
    /// The quoted parent of a reply is deliberately not carried: the target
    /// conversation has no such message, and Signal drops it too.
    ///
    /// Secret chats are refused both ways a forward could leak them: nothing
    /// is forwarded out of one, and only text is forwarded into one
    /// ([`ConversationType::can_forward_from`] / [`ConversationType::accepts_forward`]).
    pub async fn forward_to(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
        target: ActiveConversation,
    ) -> ConversationResult {
        if !self.can_forward_from() {
            return Err(ConversationError::Other(
                "messages in a secret chat cannot be forwarded".into(),
            ));
        }
        let text = self
            .get_message_text(target_author, target_sent_timestamp)?
            .unwrap_or_default();
        let attachment_ids =
            self.get_message_attachment_ids(target_author, target_sent_timestamp)?;
        let (target_kind, _) = target
            .routing_key()
            .ok_or(ConversationError::NoConversation)?;
        if !target_kind.accepts_forward(!attachment_ids.is_empty()) {
            return Err(ConversationError::Other(
                "attachments cannot be forwarded into a secret chat".into(),
            ));
        }
        let attachments: Vec<crate::domain::attachment::Attachment> = attachment_ids
            .iter()
            .filter_map(|id| self.client.attachments().get(id).ok().flatten())
            .collect();
        let pending = forwardable_attachments(&attachments);
        // An attachment-only message whose blobs can no longer be re-pointed
        // would send as an empty message; say so instead.
        if text.is_empty() && pending.is_empty() {
            return Err(ConversationError::Other(
                "message to forward has no content".into(),
            ));
        }
        self.client
            .send_message(target, &text, &pending, None, None)
            .await
            .map_err(|e| ConversationError::Other(e.to_string()))?;
        Ok(())
    }

    /// Whether messages in this conversation may be forwarded at all — false
    /// in a secret chat. The selection bar asks this; the message menu's own
    /// `forward` flag follows the same rule.
    pub fn can_forward_from(&self) -> bool {
        self.conversation
            .routing_key()
            .is_some_and(|(kind, _)| kind.can_forward_from())
    }

    /// The forward picker's destinations for `sources` (messages in this
    /// conversation, by author and sent time), narrowed by `query`.
    ///
    /// Empty from a secret chat. Secret chats are offered only when no source
    /// carries an attachment; a source that cannot be read counts as carrying
    /// one, so an unreadable row never opens the text-only path.
    pub fn forward_destinations(
        &self,
        sources: &[(UserId, u64)],
        query: &str,
    ) -> Vec<crate::client::ForwardDestination> {
        if !self.can_forward_from() {
            return Vec::new();
        }
        let has_attachments = sources.iter().any(|(author, sent_at)| {
            self.get_message_attachment_ids(author, *sent_at)
                .map_or(true, |ids| !ids.is_empty())
        });
        let mut out = self.client.forward_destinations(query);
        out.retain(|d| d.kind.accepts_forward(has_attachments));
        out
    }

    /// Get attachment IDs for a message by sender + timestamp across any conversation type.
    pub fn get_message_attachment_ids(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<Vec<crate::domain::AttachmentId>> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => Ok(msg.attachments),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                match self
                    .client
                    .anon_groups()
                    .get_message_by_timestamp(id, target_sent_timestamp)
                {
                    Ok(Some(msg)) => Ok(msg.attachments),
                    Ok(None) => Ok(vec![]),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                Ok(msg.attachments)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Check if a message is owned by the current user.
    pub fn is_own_message(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<bool> {
        let current_user_id = self.client.auth_state().get().current_user_id;
        match &self.conversation {
            ActiveConversation::AnonDm(id) => {
                match self.client.anon_dms().get_message_by_sender_timestamp(
                    id,
                    target_author,
                    target_sent_timestamp,
                ) {
                    Ok(Some(msg)) => Ok(msg.sender_id == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::AnonGroup(id) => {
                // Use timestamp-only lookup to avoid sender_id byte mismatch.
                // Ownership is determined from the stored message's sender_id field.
                match self
                    .client
                    .anon_groups()
                    .get_message_by_timestamp(id, target_sent_timestamp)
                {
                    Ok(Some(msg)) => Ok(msg.sender_id == current_user_id),
                    Ok(None) => Err(ConversationError::Other("Message not found".into())),
                    Err(e) => Err(ConversationError::Other(e.to_string())),
                }
            }
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                let msg = self.lookup_message(target_author, target_sent_timestamp)?;
                Ok(msg.sender_id == current_user_id)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }

    /// Whether the current user may delete a message for everyone.
    ///
    /// Own message, already on the server, inside the remote-action window.
    /// Delegates to [`crate::domain::message::MessageActions`] so the menu
    /// and this helper cannot drift.
    pub fn can_delete_for_everyone(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<bool> {
        Ok(self
            .message_menu(target_author, target_sent_timestamp)?
            .actions
            .delete_for_everyone)
    }

    /// Action flags for the message menu. One lookup for every conversation
    /// type; the UI renders `actions` and does not re-decide them.
    pub fn message_menu(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<MessageMenu> {
        let message = self.stored_message(target_author, target_sent_timestamp)?;
        let is_own = message.sender_id == self.client.auth_state().get().current_user_id;
        let now_ns = crate::util::time::TimestampNanos::now().as_u64();
        let mut menu = stored_message_menu(&message, is_own, now_ns);
        // A channel owner/admin/moderator may delete anyone's message for
        // everyone, not just their own within the window — the server authorizes
        // this via message `delete = author + channel->moderate` (schema.zed:128).
        // `decide` keeps channel delete-for-everyone author-only, so extend it
        // here for moderators. The server stays authoritative; offering the
        // action to a non-moderator only yields a rejected request.
        if let ActiveConversation::Channel(channel_id) = &self.conversation {
            if !menu.actions.delete_for_everyone && !message.is_deleted && message.sent_at > 0 {
                // on_server, mirroring `decide`: someone else's message has
                // reached the server; our own must not be failed/sending.
                let reached = !is_own
                    || !matches!(message.delivery_status.as_str(), "failed" | "sending");
                if reached {
                    if let Some(me) = self.client.session_current_user_id() {
                        use crate::domain::channel::MemberRole;
                        let can_moderate = self
                            .client
                            .channels()
                            .channel_repo
                            .get_membership(channel_id, &me)
                            .ok()
                            .flatten()
                            .is_some_and(|m| {
                                matches!(
                                    m.role,
                                    MemberRole::Moderator | MemberRole::Admin | MemberRole::Owner
                                )
                            });
                        if can_moderate {
                            menu.actions.delete_for_everyone = true;
                        }
                    }
                }
            }
        }
        Ok(menu)
    }

    pub(super) fn stored_message(
        &self,
        target_author: &UserId,
        target_sent_timestamp: u64,
    ) -> ConversationResult<crate::domain::Message> {
        match &self.conversation {
            ActiveConversation::AnonDm(id) => self
                .client
                .anon_dms()
                .get_message_by_sender_timestamp(id, target_author, target_sent_timestamp)?
                .ok_or_else(|| ConversationError::Other("Message not found".into())),
            // Timestamp-only: anon-group sender ids are not byte-identical to
            // the id the menu was opened with (see `is_own_message`).
            ActiveConversation::AnonGroup(id) => self
                .client
                .anon_groups()
                .get_message_by_timestamp(id, target_sent_timestamp)?
                .ok_or_else(|| ConversationError::Other("Message not found".into())),
            ActiveConversation::ServerDm { .. } | ActiveConversation::Channel(_) => {
                self.lookup_message(target_author, target_sent_timestamp)
            }
            ActiveConversation::None => Err(ConversationError::NoConversation),
        }
    }
}
