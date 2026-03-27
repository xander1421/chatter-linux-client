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

    let mut domain_rx = event_bus.subscribe_domain();

    tokio::spawn(async move {
        while let Ok(event) = domain_rx.recv().await {
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
                    if chat_data.active_channel_id == Some(message.channel_id) {
                        continue;
                    }

                    let sender_name = message.sender_id
                        .map(|id| id.to_string());

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
