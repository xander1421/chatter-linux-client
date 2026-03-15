//! Context menu callback for messages.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::UserId;
use chatter_linux::state::ActiveConversation;

use crate::MainWindow;

/// Wire more-clicked callback to show context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_more_clicked(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = Arc::clone(ui_ctx);
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns, x, y) - Signal-style identification with click position
    window.on_more_clicked(move |sender_id: SharedString, timestamp_ns: SharedString, click_x: f32, click_y: f32| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = Arc::clone(&ui_ctx);
        let weak = weak_window.clone();
        // Store click position for context menu placement
        let menu_x = click_x;
        let menu_y = click_y;

        // Determine conversation type from state (no string parsing)
        let conversation = ui_ctx.chat_state.active_conversation();

        // Move service lookups to background task
        tokio::spawn(async move {
            // Parse sender_id for Signal-style lookup
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for context menu");
                    return;
                }
            };

            // Parse timestamp for Signal-style lookup
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for context menu");
                    return;
                }
            };

            match conversation {
                ActiveConversation::AnonDm(anon_dm_id) => {
                    // Anon DM message lookup (now in background)
                    let message = match ui_ctx.anon_dm_service.get_message_by_sender_timestamp(
                        &anon_dm_id,
                        &target_sender_id,
                        target_sent_timestamp,
                    ) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                conversation_id = %anon_dm_id,
                                "Anon DM message not found for context menu"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up anon DM message for context menu");
                            return;
                        }
                    };

                    // Check if this is the current user's own message
                    let current_user_id = ui_ctx.auth_state.get().current_user_id;
                    let is_own_message = Some(message.sender_id) == current_user_id;

                    // Show context menu - pin state and attachments check happens on UI thread
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Check pin state and attachments from UI model (must be on UI thread)
                            let (is_pinned, has_attachments) = {
                                let model = w.get_messages();
                                let mut pinned = false;
                                let mut attachments = false;
                                for i in 0..model.row_count() {
                                    if let Some(m) = model.row_data(i) {
                                        if m.sender_id.as_str() == sender_id_str.as_str()
                                            && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                                        {
                                            pinned = m.is_pinned;
                                            attachments = m.has_attachments;
                                            break;
                                        }
                                    }
                                }
                                (pinned, attachments)
                            };

                            w.set_context_menu_sender_id(sender_id_str.into());
                            w.set_context_menu_timestamp_ns(timestamp_ns_str.into());
                            w.set_context_menu_is_own_message(is_own_message);
                            w.set_context_menu_is_pinned(is_pinned);
                            w.set_context_menu_has_attachments(has_attachments);
                            w.set_context_menu_x(menu_x);
                            w.set_context_menu_y(menu_y);
                            w.set_context_menu_visible(true);
                        }
                    });
                }

                ActiveConversation::AnonGroup(anon_group_id) => {
                    // Anon group message lookup (now in background)
                    let message = match ui_ctx.anon_group_service.get_message_by_sender_timestamp(
                        &anon_group_id,
                        &target_sender_id,
                        target_sent_timestamp,
                    ) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                group_id = %anon_group_id,
                                "Anon group message not found for context menu"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up anon group message for context menu");
                            return;
                        }
                    };

                    // Check if this is the current user's own message
                    let current_user_id = ui_ctx.auth_state.get().current_user_id;
                    let is_own_message = Some(message.sender_id) == current_user_id;

                    // Show context menu - pin state and attachments check happens on UI thread
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Check pin state and attachments from UI model (must be on UI thread)
                            let (is_pinned, has_attachments) = {
                                let model = w.get_messages();
                                let mut pinned = false;
                                let mut attachments = false;
                                for i in 0..model.row_count() {
                                    if let Some(m) = model.row_data(i) {
                                        if m.sender_id.as_str() == sender_id_str.as_str()
                                            && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                                        {
                                            pinned = m.is_pinned;
                                            attachments = m.has_attachments;
                                            break;
                                        }
                                    }
                                }
                                (pinned, attachments)
                            };

                            w.set_context_menu_sender_id(sender_id_str.into());
                            w.set_context_menu_timestamp_ns(timestamp_ns_str.into());
                            w.set_context_menu_is_own_message(is_own_message);
                            w.set_context_menu_is_pinned(is_pinned);
                            w.set_context_menu_has_attachments(has_attachments);
                            w.set_context_menu_x(menu_x);
                            w.set_context_menu_y(menu_y);
                            w.set_context_menu_visible(true);
                        }
                    });
                }

                ActiveConversation::Dm { .. } | ActiveConversation::Channel(_) => {
                    // Regular message lookup via message_service (now in background)
                    let message = match ui_ctx.message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                "Message not found for context menu (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for context menu");
                            return;
                        }
                    };

                    // Check if this is the current user's own message
                    let current_user_id = ui_ctx.auth_state.get().current_user_id.clone();
                    let is_own_message = message.sender_id == current_user_id;

                    // Check if message is pinned and has attachments (from service data)
                    let is_pinned = message.is_pinned;
                    let has_attachments = !message.attachments.is_empty();

                    // Show context menu with Signal-style IDs
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_context_menu_sender_id(sender_id_str.into());
                            w.set_context_menu_timestamp_ns(timestamp_ns_str.into());
                            w.set_context_menu_is_own_message(is_own_message);
                            w.set_context_menu_is_pinned(is_pinned);
                            w.set_context_menu_has_attachments(has_attachments);
                            w.set_context_menu_x(menu_x);
                            w.set_context_menu_y(menu_y);
                            w.set_context_menu_visible(true);
                        }
                    });
                }

                ActiveConversation::None => return,
            }
        });
    });
}
