//! Notification wiring.
//!
//! Connects domain events to privacy-aware desktop notifications.
//!
//! # Privacy Levels
//!
//! | Level              | What OS sees              | Leaked metadata     |
//! |--------------------|---------------------------|---------------------|
//! | `none` (default)   | "Chatter: New message"    | Nothing             |
//! | `name_only`        | "Alice: New message"      | Sender identity     |
//! | `name_and_message` | "Alice: hey are you free?"| Sender + content    |
//!
//! Sealed sender messages NEVER show the sender regardless of setting.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::UserId;
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::domain::notification::NotificationPriority;
use chatter_core::inbox::handlers::DomainEvent;
use crate::MainWindow;

/// Wire notification handlers to domain events.
///
/// Subscribes to the domain event bus and shows desktop notifications
/// for incoming messages and reactions, respecting privacy settings.
///
/// Privacy detail level is loaded from settings at startup in CoreServices.
/// User can change it at runtime via `notification_service.set_detail_level()`.
pub fn wire(_window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let notification_service = ui_ctx.notifications().clone();
    let event_bus = ui_ctx.event_bus().clone();
    let auth_state = ui_ctx.auth_state().clone();
    let chat_state = ui_ctx.chat_state().clone();
    let ui_ctx = ui_ctx.clone();

    let mut domain_rx = event_bus.subscribe_domain();

    tokio::spawn(async move {
        loop {
            let event = match domain_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Domain event receiver lagged, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let auth_data = auth_state.get();
            let chat_data = chat_state.get();

            match &event {
                DomainEvent::MessageReceived { message } => {
                    // Skip own messages
                    if let (Some(sender), Some(current)) = (&message.sender_id, &auth_data.current_user_id) {
                        if sender == current {
                            continue;
                        }
                    }
                    // Skip if user is viewing this channel
                    if chat_data.active_channel_id == Some(message.conversation_id) {
                        continue;
                    }

                    let sender_name = message.sender_id
                        .map(|id| resolve_sender_name(&ui_ctx, &id, message));

                    let preview = message.as_text();

                    // Privacy-aware: respects detail level + sealed sender status
                    let _ = notification_service.show_message_notification(
                        sender_name.as_deref(),
                        preview,
                        message.is_sealed_sender(),
                        NotificationPriority::Normal,
                    );
                }

                DomainEvent::ReactionAdded { emoji, reactor_id, .. } => {
                    if Some(*reactor_id) == auth_data.current_user_id {
                        continue;
                    }
                    // Reactions: privacy-safe (no sender in default mode, just emoji)
                    let _ = notification_service.show_message_notification(
                        None,
                        Some(&format!("Reacted {}", emoji)),
                        false,
                        NotificationPriority::Low,
                    );
                }

                _ => {}
            }
        }
    });
}

/// Resolve a sender's display name, checking conversation-specific sources.
///
/// Falls back through: user profile → anon DM peer name → anon group member name → UUID.
fn resolve_sender_name(ui_ctx: &ChatterClient, sender_id: &UserId, message: &Message) -> String {
    let name = ui_ctx.users().get_display_name(sender_id);

    // If get_display_name returned the UUID string, try conversation-specific lookups
    if name == sender_id.to_string() {
        match message.conversation_type {
            ConversationType::AnonDm => {
                let dm_id = AnonDmId::from_uuid(*message.conversation_id.as_uuid());
                if let Ok(Some(dm)) = ui_ctx.anon_dms().get_conversation(&dm_id) {
                    if let Some(peer_name) = dm.peer_display_name.as_ref().filter(|n| !n.is_empty()) {
                        return peer_name.clone();
                    }
                }
                name
            }
            ConversationType::AnonGroup => {
                let group_id = AnonGroupId::from_uuid(*message.conversation_id.as_uuid());
                if let Ok(Some(member_name)) = ui_ctx.anon_groups().get_member_display_name(&group_id, sender_id) {
                    if !member_name.is_empty() {
                        return member_name;
                    }
                }
                name
            }
            _ => name,
        }
    } else {
        name
    }
}
