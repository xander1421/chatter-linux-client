use super::*;

fn ensure_resync_ingested(outcome: Ingested) -> Result<(), MessageError> {
    match outcome {
        Ingested::RetryableFailure | Ingested::DeferredPendingSenderKey => {
            Err(MessageError::ResyncPending)
        }
        // Tombstones are handled before ingest. A rejected resync payload is
        // a validation error, never evidence that authoritative truth was applied.
        Ingested::Stored | Ingested::OwnMessage | Ingested::DeliveryIssue => Ok(()),
        Ingested::Dropped => Err(MessageError::InvalidContent(
            "resync message rejected".into(),
        )),
    }
}

impl ChannelInbound {
    /// One undeleted message from a resync page (`get_message_changes`).
    ///
    /// Missing sender keys and retryable ingest failures are typed application
    /// gaps. Validation/storage errors remain distinct; only a fully applied
    /// message application may complete recovery. Peer reaction authority is
    /// independent of the server message projection.
    pub async fn ingest_resynced(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Result<(), MessageError> {
        let id = proto_msg
            .id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| MessageError::InvalidContent("invalid resync message id".into()))?;
        if self.messages.message_repo.is_tombstoned(&id)?
            || self.cancelled_own_post(proto_msg, channel_id)?
        {
            return Ok(());
        }
        // Validate the server envelope even when ciphertext was already consumed.
        // Empty authors remain valid for anonymous rows; nonempty identities
        // must parse. Conversion validates parent IDs and content without decrypting.
        let author = if proto_msg.author_id.is_empty() {
            None
        } else {
            Some(
                proto_msg
                    .author_id
                    .parse::<UserId>()
                    .map_err(|_| MessageError::InvalidContent("invalid resync author".into()))?,
            )
        };
        let envelope = proto_to_domain_message_with_sender(
            proto_msg,
            author,
            conversation_type_for_channel(&self.messages.channel_repo, &channel_id),
        )?;
        if envelope.conversation_id != channel_id {
            return Err(MessageError::InvalidContent(
                "resync channel mismatch".into(),
            ));
        }
        let content = proto_msg
            .content
            .as_ref()
            .and_then(|content| content.r#type.as_ref())
            .ok_or_else(|| {
                MessageError::InvalidContent("resync message has no content type".into())
            })?;
        let encrypted = if let chat_v1::message::content::Type::Encrypted(encrypted) = content {
            let group = GroupEncrypted::decode(encrypted.ciphertext.as_slice()).map_err(|_| {
                MessageError::InvalidContent("invalid resync encrypted envelope".into())
            })?;
            if group.channel_id != channel_id.to_string() || group.ciphertext.is_empty() {
                return Err(MessageError::InvalidContent(
                    "invalid resync group envelope".into(),
                ));
            }
            if author.is_none() || proto_msg.author_device_id == 0 {
                return Err(MessageError::InvalidContent(
                    "invalid resync encrypted author".into(),
                ));
            }
            true
        } else {
            false
        };
        let existing = self.messages.get_message_local(&id)?;
        if let Some(local) = &existing {
            // Encrypted envelopes carry a transport clock; the cached sent_at
            // is the authenticated DataMessage clock, so do not equate them.
            if local.conversation_type != crate::domain::message::ConversationType::Channel
                || local.sender_id != author
                || local.thread_id != envelope.thread_id
                || (!encrypted && local.sent_at != proto_msg.timestamp)
            {
                return Err(MessageError::InvalidContent(
                    "resync message identity mismatch".into(),
                ));
            }
        }
        if existing
            .as_ref()
            .is_some_and(|local| local.conversation_id != channel_id)
        {
            return Err(MessageError::InvalidContent(
                "resync local channel mismatch".into(),
            ));
        }
        match &existing {
            // Never seen: store it as the server has it, edit included.
            None => {
                ensure_resync_ingested(self.ingest(proto_msg, channel_id, false).await)?;
            }
            Some(local) if is_newer_edit(proto_msg, local) => {
                ensure_resync_ingested(self.ingest_edit(proto_msg, channel_id).await)?;
            }
            Some(_) => {}
        }
        self.messages
            .reconcile_channel_reactions(proto_msg, channel_id)
    }
}
