//! Message editing callbacks for channels, DMs, and anonymous conversations.
//!
//! Handles entering edit mode, sending edits, and cancelling edits.
//! All operations are properly synchronized using Signal-style identification.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::UserId;
use chatter_linux::services::conversation::ConversationHandle;
use chatter_linux::state::ActiveConversation;
use crate::MainWindow;

/// Wire edit-message callback - enters edit mode by populating composer.
pub fn wire_edit_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_edit_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        let conversation = ui_ctx.chat_state.active_conversation();

        // Move service lookups to background task
        tokio::spawn(async move {
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(_) => return,
            };

            // Get message text via ConversationHandle (polymorphic across all types)
            let handle = chatter_linux::services::conversation::ConversationHandle::new(
                conversation, ui_ctx.clone(),
            );
            let text = match handle.get_message_text(&target_sender_id, target_sent_timestamp) {
                Ok(t) => t,
                Err(_) => return,
            };

            if let Some(text) = text {
                let composite_key = format!("{}:{}", sender_id_str, timestamp_ns_str);

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_composer_text(text.into());
                        w.set_composer_edit_message_id(composite_key.into());
                        w.set_composer_is_editing(true);
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            timestamp = %timestamp_ns_str,
                            "Entered edit mode (Signal-style)"
                        );
                    }
                });
            }
        });
    });
}

/// Wire send-edit callback - sends the edited message content.
///
/// Uses Signal-style (sender_id, timestamp_ns) stored in composer_edit_message_id as "sender_id:timestamp_ns".
pub fn wire_send_edit(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_send_edit(move |composite_key: SharedString, new_text: SharedString| {
        let composite_key_str = composite_key.to_string();
        let new_text_str = new_text.to_string();

        // Parse composite key "sender_id:timestamp_ns"
        let parts: Vec<&str> = composite_key_str.split(':').collect();
        if parts.len() != 2 {
            tracing::error!("Invalid composite key for edit: {}", composite_key_str);
            return;
        }
        let sender_id_str = parts[0].to_string();
        let timestamp_ns_str = parts[1].to_string();

        let handle = ConversationHandle::current(&ui_ctx);
        if matches!(handle.conversation(), ActiveConversation::None) {
            return;
        }

        let is_anon = matches!(
            handle.conversation(),
            ActiveConversation::AnonDm(_) | ActiveConversation::AnonGroup(_)
        );
        let message_service = ui_ctx.message_service.clone();
        let weak = weak_window.clone();

        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(_) => return,
        };

        tokio::spawn(async move {
            if let Err(e) = handle.send_edit(target_sent_timestamp, &new_text_str).await {
                tracing::error!(error = %e, "Failed to send edit");
                return;
            }

            // For DM/Channel, persist the edit locally (anon services handle this internally)
            if !is_anon {
                let target_sender_id: UserId = match sender_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };

                if let Ok(Some(message)) = message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated = message.clone();
                    updated.content = MessageContent::Text(new_text_str.clone());
                    updated.is_edited = true;
                    let _ = message_service.update_message(&updated);
                }
            }

            // Update UI
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // For anon conversations, update message content in the UI model
                    if is_anon {
                        let current_model = w.get_messages();
                        let messages: Vec<_> = (0..current_model.row_count())
                            .filter_map(|i| current_model.row_data(i))
                            .map(|mut m| {
                                if m.sender_id.as_str() == sender_id_str.as_str()
                                    && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                                {
                                    m.content = new_text_str.clone().into();
                                    m.is_edited = true;
                                }
                                m
                            })
                            .collect();
                        w.set_messages(ModelRc::new(VecModel::from(messages)));
                    }

                    // Clear edit mode
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                    w.set_composer_text("".into());
                }
            });
        });
    });
}

/// Wire cancel-edit callback - exits edit mode without sending.
pub fn wire_cancel_edit(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_edit(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_composer_text("".into());
                w.set_composer_edit_message_id("".into());
                w.set_composer_is_editing(false);
                tracing::debug!("Cancelled edit mode");
            }
        });
    });
}
