use super::*;

/// Whether this receive path won the durable insert. Only that winner announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Persisted {
    New,
    Existing,
}

impl ChannelInbound {
    pub(crate) fn persist_message(
        messages: &MessageService,
        message: &mut Message,
        channel_id: ChannelId,
    ) -> Option<Persisted> {
        // Channel controls are peer sealed-sender messages. Server projections
        // cannot replace local reaction truth during backfill or live re-ingest.
        message.reaction_counts.clear();
        message.current_user_reactions.clear();
        let persisted = match messages.get_message_local(&message.id) {
            Ok(Some(existing)) => {
                if existing.conversation_id != channel_id
                    || existing.conversation_type != message.conversation_type
                    || existing.sender_id != message.sender_id
                    || existing.thread_id != message.thread_id
                {
                    tracing::warn!("Channel envelope identity differs from retained row");
                    return None;
                }
                if message.sent_at != existing.sent_at {
                    tracing::warn!(
                        message_id = %message.id, incoming = message.sent_at, retained = existing.sent_at,
                        "Retained channel clock differs from the authenticated clock"
                    );
                    return None;
                }
                // SQL update deliberately retains the original row's identity.
                message.created_at = existing.created_at;
                if message.attachments.is_empty() {
                    message.attachments = existing.attachments.clone();
                }
                message.reaction_counts = existing.reaction_counts.clone();
                message.current_user_reactions = existing.current_user_reactions.clone();
                // Read state is local: the server copy always converts as
                // unread, so every catch-up re-fetch of a message the user had
                // already read flipped it back to unread.
                message.is_read |= existing.is_read;
                // Likewise a local soft-delete: the server copy never says deleted.
                message.is_deleted |= existing.is_deleted;
                // Content changes only through the edit path (author check,
                // edit-time ordering, 48h window: `apply_channel_edit`). A
                // re-ingested original — backfill, resync — must not undo an
                // applied edit or bypass those rules. (No stored channel row
                // holds an undecrypted placeholder: ingest decrypts or refuses.)
                message.content = existing.content.clone();
                message.is_edited = existing.is_edited;
                message.edited_at = existing.edited_at;
                if let Err(e) = messages.update_message(message) {
                    tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                        "Failed to update channel message");
                    return None;
                }
                Persisted::Existing
            }
            Ok(None) => {
                // Atomic insert, not the earlier read, decides who announces
                // when a stream, catch-up or sender-key retry races another.
                match messages.message_repo.insert_or_ignore_with_row(message) {
                    Ok((inserted, stored)) => {
                        if let Some(stored) = stored { *message = stored; }
                        return Some(if inserted { Persisted::New } else { Persisted::Existing });
                    },
                    Err(e) => {
                        tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                            "Failed to insert channel message");
                        return None;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%channel_id, message_id = %message.id, error = %e,
                    "Failed to read existing message before upsert");
                return None;
            }
        };
        if let Ok(Some(stored)) = messages.get_message_local(&message.id) {
            *message = stored;
        }
        Some(persisted)
    }
}
