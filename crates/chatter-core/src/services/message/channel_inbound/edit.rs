use super::*;

impl ChannelInbound {
    /// Same path, for a message the server says was EDITED
    /// (`message_updated`). Decrypts like any channel message (sender keys
    /// for an encrypted channel), then applies the new body through the one
    /// edit choke point,
    /// [`MessageService::apply_channel_edit`]: only the author's own message
    /// in this channel, ordered and dated by the server's `edited_at`, within
    /// Signal's 48h, held if the message is not here yet — and a re-delivered
    /// edit (a catch-up re-fetch) changes nothing and announces nothing. The
    /// author is the server message's `author_id`, which for an encrypted
    /// channel is also the sender-key sender that decrypted it.
    ///
    /// Announces `MessageEdited` so an open conversation patches the bubble
    /// in place instead of appending a duplicate.
    pub async fn ingest_edit(
        &self,
        proto_msg: &chat_v1::Message,
        channel_id: ChannelId,
    ) -> Ingested {
        let Ok(session) = self.messages.receive_session() else { return Ingested::RetryableFailure };
        self.ingest_edit_with_session(proto_msg, channel_id, session).await
    }

    pub(crate) async fn ingest_edit_with_session(&self, proto_msg: &chat_v1::Message, channel_id: ChannelId, session: ReceiveSession) -> Ingested {
        if session.check().is_err() { return Ingested::RetryableFailure; }
        let message = match self.prepare(proto_msg, channel_id, true, &session).await {
            Ok(m) => m,
            Err(outcome) => return outcome,
        };
        // An edit with no known author cannot be authorized.
        let Some(author) = message.sender_id else {
            return Ingested::Dropped;
        };
        // The server names the edited row by id; it must be the author's. Its
        // `sent_at` is the target (an encrypted edit's DataMessage clock need
        // not be the original's).
        let messages = self.messages.clone();
        let edit_ts = proto_msg.edited_at;
        self.messages.database.run_blocking(move || {
            session.check()?;
            Ok(Self::apply_edit_unit(&messages, &session, message, channel_id, author, edit_ts))
        }).await.unwrap_or(Ingested::RetryableFailure)
    }

    fn apply_edit_unit(
        messages: &MessageService,
        session: &ReceiveSession,
        message: Message,
        channel_id: ChannelId,
        author: UserId,
        edit_ts: u64,
    ) -> Ingested {
        let body = message.as_text().unwrap_or_default();
        let modifiers = crate::storage::repositories::message_modifier_repo::MessageModifierRepository::new(messages.database.clone());
        match modifiers.apply_received_edit_admitted(
            &messages.message_repo, &author, Some(channel_id), message.sent_at,
            body, edit_ts, edit_ts, Some(&message.id), Some(session),
        ) {
            Ok(Some(edited)) => {
                if session.check().is_err() { return Ingested::RetryableFailure; }
                let _ = messages
                    .event_bus
                    .domain_sender()
                    .send(DomainEvent::MessageEdited {
                        message_id: edited.id,
                        new_content: edited.content,
                        channel_id,
                        conversation_type: edited.conversation_type,
                        author_id: author,
                        target_sent_timestamp: edited.sent_at,
                    });
                Ingested::Stored
            }
            // Held for its message, refused, or already shown.
            Ok(None) => Ingested::Dropped,
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "Failed to apply a server channel edit");
                Ingested::RetryableFailure
            }
        }
    }
}

/// A new edit has fresh ciphertext; only duplicate/older versions reuse the
/// consumed sender key. This comparison also avoids replaying an old edit.
pub(crate) fn is_newer_edit(proto: &chat_v1::Message, existing: &Message) -> bool {
    proto.edited
        && proto.edited_at > 0
        && proto.edited_at
            > existing
                .edited_at
                .and_then(|time| time.timestamp_nanos_opt())
                .and_then(|ns| u64::try_from(ns).ok())
                .unwrap_or(0)
}
