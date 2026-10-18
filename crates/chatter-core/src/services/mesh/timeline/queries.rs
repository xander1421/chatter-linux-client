use super::*;

impl MeshTimeline {
    /// History for one mesh conversation, oldest first — ready to render.
    ///
    /// `before_sent_at` pages backwards through it, in the sender's nanosecond
    /// timestamps, exactly as the other conversation types do.
    pub fn history(
        &self,
        conversation: &MeshConversation,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let mut messages = self.messages.list_for_conversation_asc(
            &conversation.conversation_id(),
            &conversation.conversation_type(),
            before_sent_at,
            limit,
        )?;

        // Put the mesh delivery state back. Without this every state is
        // live-only: a message that was `waiting`, `carried` or `not delivered`
        // comes back after a reopen looking ordinarily sent, which is exactly
        // the arrival-nobody-confirmed claim the mesh surfaces must never make.
        //
        // A read failure leaves the states empty rather than failing the whole
        // history: showing the messages without their states is a degraded
        // timeline, showing nothing is a lost one.
        let ids: Vec<String> = messages.iter().map(|m| m.id.to_string()).collect();
        match self.delivery.get_many(&ids) {
            Ok(states) => {
                for message in &mut messages {
                    if let Some(state) = states.get(&message.id.to_string()) {
                        message.delivery_status.clone_from(state);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "mesh: could not read delivery states");
            }
        }
        Ok(messages)
    }

    /// Remember where a message this device sent has got to.
    ///
    /// `wire_id` is the ID the mesh knows the message by; it is mapped to the
    /// stored row's ID here so callers never have to know that derivation.
    pub fn record_delivery(&self, wire_id: &str, state: MeshDelivery) {
        let id = super::super::mapping::wire_message_id(wire_id);
        if let Err(error) = self.delivery.set(&id.to_string(), state.as_str()) {
            tracing::warn!(%error, "mesh: could not store a delivery state");
        }
    }

    /// Mark everything in a mesh conversation read locally.
    ///
    /// Local only: it moves no read receipt onto the wire. Telling a peer their
    /// message was read is a separate act with a separate cost (it puts traffic
    /// on the radio) and lives on
    /// [`MeshService::mark_read`](super::super::MeshService::mark_read).
    pub fn mark_read(&self, conversation: &MeshConversation) -> Result<(), StorageError> {
        // Mesh has no scroll restore, so the cleared boundary goes unused here.
        self.messages.mark_conversation_read(
            &conversation.conversation_id(),
            &conversation.conversation_type(),
        )?;
        Ok(())
    }

    /// Every mesh conversation, shaped for the unified sidebar.
    ///
    /// Ordered the way `UnifiedConversationRepository::list_all` orders its own
    /// rows — most recent message first, conversations with no messages last —
    /// so a caller can merge the two lists without re-sorting either.
    pub fn conversations(&self) -> Result<Vec<UnifiedConversation>, StorageError> {
        let entries: Vec<Entry> = self.open.lock().values().cloned().collect();
        let mut conversations = Vec::with_capacity(entries.len());
        for entry in entries {
            conversations.push(self.shape(&entry)?);
        }
        conversations.sort_by(|a, b| {
            b.last_message_at
                .cmp(&a.last_message_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(conversations)
    }

    /// The mesh conversation a sidebar row refers to.
    ///
    /// [`UnifiedConversation::id`] is a string, and the derivation that produced
    /// it is one-way, so the round trip from a rendered row back to something
    /// sendable has to come from here rather than being recomputed. `None` means
    /// the id belongs to some other conversation type, which is the answer a
    /// caller dispatching on a mixed sidebar wants.
    #[must_use]
    pub fn conversation_for(&self, id: &ChannelId) -> Option<MeshConversation> {
        self.open
            .lock()
            .get(id)
            .map(|entry| entry.conversation.clone())
    }

    /// The persisted sidebar label for an open mesh conversation.
    #[must_use]
    pub fn conversation_name(&self, id: &ChannelId) -> Option<String> {
        self.open.lock().get(id).map(|entry| entry.label.clone())
    }
}
