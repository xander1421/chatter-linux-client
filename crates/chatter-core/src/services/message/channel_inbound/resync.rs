use super::*;

pub(super) fn ensure_resync_ingested(outcome: Ingested) -> Result<(), MessageError> {
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
        let session = self.messages.receive_session()?;
        self.ingest_resynced_with_session(proto_msg, channel_id, session).await
    }

    pub(crate) async fn ingest_resynced_with_session(&self, proto_msg: &chat_v1::Message, channel_id: ChannelId, session: ReceiveSession) -> Result<(), MessageError> {
        session.check()?;
        let inbound = self.clone();
        let proto = proto_msg.clone();
        let preflight_session = session.clone();
        let action = self.messages.database.run_blocking(move || {
            preflight_session.check()?;
            Ok(inbound.resync_preflight(&proto, channel_id, &preflight_session))
        }).await??;
        match action {
            ResyncAction::Skip => return Ok(()),
            ResyncAction::Insert => ensure_resync_ingested(self.ingest_with_session(proto_msg, channel_id, false, PresenceHint::KnownPresent, session.clone()).await)?,
            ResyncAction::Edit => ensure_resync_ingested(self.ingest_edit_with_session(proto_msg, channel_id, session.clone()).await)?,
            ResyncAction::Existing => {}
        }
        let messages = self.messages.clone();
        let proto = proto_msg.clone();
        self.messages.database.run_blocking(move || {
            session.check()?;
            Ok(messages.reconcile_channel_reactions(&proto, channel_id))
        }).await?
    }

    /// Per-row metadata is used only to choose decrypt/edit work; persistence
    /// rereads current state transactionally and never writes this snapshot.
    fn resync_preflight(&self, proto_msg: &chat_v1::Message, channel_id: ChannelId, session: &ReceiveSession) -> Result<ResyncAction, MessageError> {
        let id = proto_msg
            .id
            .parse::<crate::domain::ids::MessageId>()
            .map_err(|_| MessageError::InvalidContent("invalid resync message id".into()))?;
        if self.messages.message_repo.is_tombstoned(&id)?
            || self.cancelled_own_post(proto_msg, channel_id, session)?
        {
            return Ok(ResyncAction::Skip);
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
        if Self::drop_blocked_sender(&self.messages, proto_msg, channel_id)? {
            return Ok(ResyncAction::Skip);
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
        Ok(match existing {
            None => ResyncAction::Insert,
            Some(local) if is_newer_edit(proto_msg, &local) => ResyncAction::Edit,
            Some(_) => ResyncAction::Existing,
        })
    }
}

#[derive(Clone, Copy)]
enum ResyncAction { Skip, Insert, Edit, Existing }
