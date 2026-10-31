use super::*;

/// Only the durable insert winner announces. Suppression is terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Persisted {
    New,
    Existing,
    Suppressed,
}

/// Scheduling hint only: authority always comes from the writer transaction.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PresenceHint {
    KnownPresent,
    UnknownAbsent,
}

impl ChannelInbound {
    pub(crate) fn persist_message_admitted(
        messages: &MessageService,
        message: &mut Message,
        channel_id: ChannelId,
        hint: PresenceHint,
        session: &ReceiveSession,
    ) -> Option<Persisted> {
        // Peer controls own reaction truth; server snapshots cannot replace it.
        message.reaction_counts.clear();
        message.current_user_reactions.clear();
        match messages.message_repo.store_channel_received(message, matches!(hint, PresenceHint::KnownPresent), session) {
            Ok((inserted, Some(stored))) => {
                *message = stored;
                Some(if inserted { Persisted::New } else { Persisted::Existing })
            }
            Ok((false, None)) => Some(Persisted::Suppressed),
            Ok((true, None)) => None,
            Err(error) => {
                tracing::warn!(%channel_id, message_id = %message.id, %error, "Channel transactional persistence failed");
                None
            }
        }
    }
}
