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
        let message = match self.prepare(proto_msg, channel_id, true).await {
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
        let target_sent_at = match self.messages.get_message_local(&message.id) {
            Ok(Some(existing)) if existing.sender_id != Some(author) => {
                tracing::warn!(%channel_id, message_id = %message.id, %author,
                    "Server edit names another author's message; dropped");
                return Ingested::Dropped;
            }
            Ok(Some(existing)) => existing.sent_at,
            Ok(None) => message.sent_at,
            Err(e) => {
                tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                    "Failed to read the edited message; dropped");
                return Ingested::RetryableFailure;
            }
        };
        let body = message.as_text().unwrap_or_default();
        let edit_ts = proto_msg.edited_at;
        match self.messages.apply_channel_edit(
            &channel_id,
            &author,
            target_sent_at,
            body,
            edit_ts,
            edit_ts,
        ) {
            Ok(Some(edited)) => {
                let _ = self
                    .event_bus()
                    .domain_sender()
                    .send(DomainEvent::MessageEdited {
                        message_id: edited.id,
                        new_content: edited.content,
                        channel_id,
                        conversation_type: edited.conversation_type,
                        author_id: author,
                        target_sent_timestamp: target_sent_at,
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
