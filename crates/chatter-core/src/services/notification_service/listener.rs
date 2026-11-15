/// Notify for incoming mesh DMs after the mesh timeline has persisted the row.
/// Message IDs suppress duplicate radio/courier arrivals during this session.
#[cfg(feature = "mesh")]
pub fn spawn_mesh_notification_listener(
    mut events: tokio::sync::broadcast::Receiver<crate::services::mesh::MeshEvent>,
    notifications: std::sync::Arc<super::NotificationService>,
) -> tokio::task::JoinHandle<()> {
    use crate::services::mesh::{MeshConversation, MeshEvent};

    tokio::spawn(async move {
        let mut recent = std::collections::VecDeque::new();
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let MeshEvent::Message(message) = event else {
                continue;
            };
            if message.is_local || !matches!(&message.conversation, MeshConversation::Dm(_)) {
                continue;
            }
            let id = message.message.id.to_string();
            if recent.contains(&id) {
                continue;
            }
            recent.push_back(id);
            if recent.len() > 256 {
                recent.pop_front();
            }
            if let Err(error) = notifications.show_message(
                "Chatter",
                "New mesh message",
                crate::domain::notification::NotificationPriority::Normal,
            ) {
                tracing::warn!(%error, "Could not show mesh DM notification");
            }
        }
    })
}

/// Whether a `MessageReceived` should raise an OS notification.
///
/// * Never for our own message — every path that files one of ours (live
///   channel echo, SyncSent from our other device, local call-history rows)
///   stamps `sender_id` with our id.
/// * Never when we cannot say who we are (`me == None`): the self-check would
///   be a no-op, and a toast for your own message is the worse failure.
/// * Never for the conversation on screen (open AND the app visible — an open
///   chat behind the lock screen still notifies).
/// * Never for a muted conversation.
/// * Never for a conversation notice (a timer change and the like): it
///   records something about the chat, it is not a message to read.
/// * Never for a Call record: the call itself rang, and a missed one is on
///   the Calls screen.
pub(crate) fn should_notify_message(
    message: &crate::domain::message::Message,
    me: Option<&crate::domain::ids::UserId>,
    on_screen: bool,
    muted: bool,
) -> bool {
    let Some(me) = me else { return false };
    message.sender_id.as_ref() != Some(me)
        && !on_screen
        && !muted
        && !message.content.is_system()
        && !matches!(
            message.content,
            crate::domain::message::MessageContent::Call(_)
        )
}

/// Whether a notification about `message` (or a reaction to it) must be the
/// generic one at every detail level. A secret chat stores decoded Text/Mixed
/// with a sender, so sealed content alone would miss it: the conversation
/// type decides.
pub(crate) fn is_private_notification(message: &crate::domain::message::Message) -> bool {
    message.is_sealed_sender() || message.conversation_type.is_secret_chat()
}
