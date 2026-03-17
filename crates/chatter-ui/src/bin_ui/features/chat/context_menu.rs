//! Context menu callback for messages.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::Permissions;
use chatter_core::domain::UserId;
use chatter_core::services::conversation::ConversationHandle;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;

/// Wire more-clicked callback to show context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_more_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = Arc::clone(ui_ctx);
    let weak_window = window.as_weak();

    window.on_more_clicked(move |sender_id: SharedString, timestamp_ns: SharedString, click_x: f32, click_y: f32| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = Arc::clone(&ui_ctx);
        let weak = weak_window.clone();
        let menu_x = click_x;
        let menu_y = click_y;

        let conversation = ui_ctx.chat_state().active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }

        let handle = ConversationHandle::new(conversation, ui_ctx.clone());

        tokio::spawn(async move {
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for context menu");
                    return;
                }
            };
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for context menu");
                    return;
                }
            };

            let is_own_message = match handle.is_own_message(&target_sender_id, target_sent_timestamp) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to look up message for context menu");
                    return;
                }
            };

            // Derive delete-for-everyone from permissions:
            // DMs use Permissions::dm() (both users can delete any message),
            // channels use role-based permissions checked via is_own_message.
            let can_delete_for_everyone = match &conversation {
                ActiveConversation::Dm { .. } | ActiveConversation::AnonDm(_) => {
                    Permissions::dm().can_delete_any_message
                }
                _ => is_own_message,
            };

            if conversation.is_anon() {
                // Anon conversations: get pin state and attachments from UI model
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let (is_pinned, has_attachments) = find_message_ui_state(
                            &w, &sender_id_str, &timestamp_ns_str,
                        );
                        set_context_menu(&w, sender_id_str, timestamp_ns_str, is_own_message, can_delete_for_everyone, is_pinned, has_attachments, menu_x, menu_y);
                    }
                });
            } else {
                // DM/Channel: get pin state and attachments from service data
                let msg = match handle.conversation() {
                    ActiveConversation::Dm { .. } | ActiveConversation::Channel(_) => {
                        match ui_ctx.messages().get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                            Ok(Some(msg)) => msg,
                            Ok(None) => {
                                tracing::error!("Message not found for context menu");
                                return;
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to look up message for context menu");
                                return;
                            }
                        }
                    }
                    _ => return,
                };

                let is_pinned = msg.is_pinned;
                let has_attachments = !msg.attachments.is_empty();

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        set_context_menu(&w, sender_id_str, timestamp_ns_str, is_own_message, can_delete_for_everyone, is_pinned, has_attachments, menu_x, menu_y);
                    }
                });
            }
        });
    });
}

/// Find pin state and attachment info from the UI message model.
fn find_message_ui_state(w: &MainWindow, sender_id: &str, timestamp_ns: &str) -> (bool, bool) {
    let model = w.get_messages();
    for i in 0..model.row_count() {
        if let Some(m) = model.row_data(i) {
            if m.sender_id.as_str() == sender_id && m.timestamp_ns.as_str() == timestamp_ns {
                return (m.is_pinned, m.has_attachments);
            }
        }
    }
    (false, false)
}

/// Set all context menu properties on the window.
fn set_context_menu(
    w: &MainWindow,
    sender_id: String,
    timestamp_ns: String,
    is_own_message: bool,
    can_delete_for_everyone: bool,
    is_pinned: bool,
    has_attachments: bool,
    menu_x: f32,
    menu_y: f32,
) {
    w.set_context_menu_sender_id(sender_id.into());
    w.set_context_menu_timestamp_ns(timestamp_ns.into());
    w.set_context_menu_is_own_message(is_own_message);
    w.set_context_menu_can_delete_for_everyone(can_delete_for_everyone);
    w.set_context_menu_is_pinned(is_pinned);
    w.set_context_menu_has_attachments(has_attachments);
    w.set_context_menu_x(menu_x);
    w.set_context_menu_y(menu_y);
    w.set_context_menu_visible(true);
}
