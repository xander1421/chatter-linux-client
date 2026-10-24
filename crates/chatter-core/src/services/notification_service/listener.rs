use super::NotificationService;

/// Notify for incoming mesh DMs after the mesh timeline has persisted the row.
/// Message IDs suppress duplicate radio/courier arrivals during this session.
#[cfg(feature = "mesh")]
pub fn spawn_mesh_notification_listener(
    mut events: tokio::sync::broadcast::Receiver<crate::services::mesh::MeshEvent>,
    notifications: std::sync::Arc<NotificationService>,
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

/// Spawn a task that listens to the domain event bus and dispatches user-
/// visible notifications via `NotificationService`.
///
/// Filter policy lives here (not in the UI / client facade) — see
/// [`should_notify_message`].
/// * Show reactions from other users as low-priority notifications.
///
/// "Who am I" comes from the settings-backed `UserService::current_user_id`,
/// NOT `AuthState`: the device-link bring-up never calls
/// `AuthState::set_logged_in`, so on a freshly linked device `AuthState` says
/// nobody is logged in and every self-synced copy of our own send (SyncSent)
/// used to raise a notification (F11).
///
/// Task exits when the event bus closes; returns the `JoinHandle` for the
/// caller to track alongside other lifecycle tasks.
pub fn spawn_notification_listener(
    event_bus: crate::events::EventBus,
    // Weak: the client owns this task, so a strong handle would keep the
    // client alive forever. Used for the per-conversation rules — display
    // name in context, mute, what is on screen.
    client: std::sync::Weak<crate::client::ChatterClient>,
    user_service: std::sync::Arc<crate::services::user::UserService>,
    notification_service: std::sync::Arc<NotificationService>,
) -> tokio::task::JoinHandle<()> {
    use crate::domain::notification::NotificationPriority;
    use crate::inbox::handlers::DomainEvent;

    let mut domain_rx = event_bus.subscribe_domain();
    tokio::spawn(async move {
        loop {
            let event = match domain_rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Notification listener lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            let me = user_service.current_user_id().ok();

            match &event {
                DomainEvent::MessageReceived { message } => {
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    if let Err(error) =
                        crate::services::conversation::ConversationHandle::mark_received_as_read(
                            &client, message,
                        )
                        .await
                    {
                        tracing::warn!(%error, "Failed to mark visible incoming message read");
                    }
                    let (on_screen, muted) = conversation_attention(&client, message);
                    if !should_notify_message(message, me.as_ref(), on_screen, muted) {
                        continue;
                    }
                    use crate::domain::channel::NotificationPreference;
                    match conversation_preference(&client, message) {
                        NotificationPreference::None => continue,
                        NotificationPreference::Mentions
                            if !me.as_ref().is_some_and(|id| client.message_mentions_user(message, id)) => continue,
                        _ => {}
                    }

                    // In an anon group the per-group name, never the global
                    // profile name of a contact who happens to be in it.
                    let sender_name = message.sender_id.map(|id| {
                        client.display_name_in(
                            &message.conversation_type,
                            &message.conversation_id,
                            &id,
                        )
                    });
                    let preview = message.as_text();

                    let _ = notification_service.show_tracked_message_notification(
                        message.id,
                        &message.conversation_id,
                        sender_name.as_deref(),
                        preview,
                        is_private_notification(message),
                        NotificationPriority::Normal,
                    );
                }
                DomainEvent::AnonDmRequestReceived {
                    conversation_id,
                    sender_id,
                    ..
                } => {
                    if me.is_none() || Some(*sender_id) == me {
                        continue;
                    }
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    let id = (*conversation_id).into();
                    let on_screen =
                        client.chat_state().is_app_visible() && client.is_showing_conversation(id);
                    let muted = client
                        .is_conversation_muted(crate::state::ActiveConversation::AnonDm(
                            *conversation_id,
                        ))
                        .unwrap_or(false);
                    if on_screen || muted {
                        continue;
                    }
                    // Requests reveal no more than an incoming secret message,
                    // including when the user allows normal message previews.
                    if let Err(error) = notification_service.show_message_notification(
                        &id,
                        None,
                        None,
                        true,
                        NotificationPriority::Normal,
                    ) {
                        tracing::warn!(%error, "Secret request notification failed");
                    }
                }
                DomainEvent::ReactionAdded {
                    scope,
                    sender_id,
                    sent_at_ns,
                    emoji,
                    reactor_id,
                    ..
                } => {
                    if me.is_none() || Some(*reactor_id) == me {
                        continue;
                    }
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    // Same mute / on-screen rules as the message it reacts
                    // to. A target we do not hold names no conversation to
                    // check, so it stays silent.
                    let Some(target) = client
                        .messages()
                        .message_repo
                        .get_by_reaction_scope(scope, sender_id, *sent_at_ns)
                        .ok()
                        .flatten()
                    else {
                        continue;
                    };
                    let (on_screen, muted) = conversation_attention(&client, &target);
                    if on_screen || muted || conversation_preference(&client, &target) != crate::domain::channel::NotificationPreference::All {
                        continue;
                    }
                    let _ = notification_service.show_message_notification(
                        &target.conversation_id,
                        None,
                        Some(&format!("Reacted {}", emoji)),
                        // The reaction lives in the target's conversation.
                        is_private_notification(&target),
                        NotificationPriority::Low,
                    );
                }

                // Read here or on another of our devices: the notification is
                // stale (Signal cancels a thread's notification either way).
                DomainEvent::ConversationsRead {
                    conversation_ids, ..
                } => {
                    for id in conversation_ids {
                        let _ = notification_service.cancel_conversation(id);
                    }
                }

                DomainEvent::MessageDeleted { message_id, .. } => {
                    notification_service
                        .remove_message_notifications(std::slice::from_ref(message_id));
                }

                // ================================================================
                // Typed lifecycle events (from gRPC notification stream).
                // Side-effects run in spawned tasks so the event loop stays hot.
                // ================================================================
                DomainEvent::DeviceLinked {
                    device_id,
                    device_name,
                    device_info,
                    approved_by_self,
                } => {
                    // Skip the toast on the device that just approved the
                    // pairing — the UI there already showed a confirm
                    // screen. Only fan-out to siblings raises a toast.
                    if *approved_by_self {
                        continue;
                    }
                    let body = match (device_name.as_deref(), device_info.as_deref()) {
                        (Some(name), _) if !name.is_empty() => {
                            format!("\"{}\" is now linked to your account.", name)
                        }
                        (_, Some(info)) if !info.is_empty() => {
                            format!("{} was just linked to your account.", info)
                        }
                        _ => "A new device was just linked to your account.".to_string(),
                    };
                    tracing::info!(device_id, "DeviceLinked notification → surfacing toast");
                    let _ = notification_service.show_message(
                        "New device linked",
                        &body,
                        NotificationPriority::High,
                    );
                }

                DomainEvent::PeerDevicesChanged {
                    peer_user_id,
                    device_id,
                    change_type,
                } => {
                    // Per Signal: an added device of a staying member needs
                    // only our SKDM, a removed one no key reset — see
                    // `MessageService::handle_peer_device_change`.
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    let messages = client.messages().clone();
                    let (peer, device, change) = (*peer_user_id, *device_id, change_type.clone());
                    tokio::spawn(async move {
                        messages
                            .handle_peer_device_change(peer, device, &change)
                            .await;
                    });
                }

                _ => {}
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
pub(super) fn should_notify_message(
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
pub(super) fn is_private_notification(message: &crate::domain::message::Message) -> bool {
    message.is_sealed_sender() || message.conversation_type.is_secret_chat()
}

/// `(on_screen, muted)` for the conversation `message` belongs to. On screen
/// is the same "is the user looking at this" rule that marks messages read:
/// the app is visible AND `is_showing` (which also matches a mailbox DM in
/// its peer's DM view — the old `is_for_channel` check notified for those).
fn conversation_attention(
    client: &std::sync::Arc<crate::client::ChatterClient>,
    message: &crate::domain::message::Message,
) -> (bool, bool) {
    let on_screen = client.chat_state().is_app_visible() && client.is_showing(message);
    let conversation = client.classify_active_conversation(
        &message.conversation_id,
        &message.conversation_type,
        message.sender_id,
    );
    let muted = client.is_conversation_muted(conversation).unwrap_or(false);
    (on_screen, muted)
}

fn conversation_preference(
    client: &std::sync::Arc<crate::client::ChatterClient>,
    message: &crate::domain::message::Message,
) -> crate::domain::channel::NotificationPreference {
    let conversation = client.classify_active_conversation(
        &message.conversation_id,
        &message.conversation_type,
        message.sender_id,
    );
    client.conversation_notification_preference(conversation).unwrap_or_default()
}
