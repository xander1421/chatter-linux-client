//! Context menu callback for messages.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;

use crate::MainWindow;

/// Wire more-clicked callback to show context menu.
pub fn wire_more_clicked(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let auth_state = ctx.auth_state.clone();

    let weak_window = window.as_weak();

    window.on_more_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();
        let auth_state = auth_state.clone();
        let weak = weak_window.clone();

        // Parse message ID and look up message info
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for context menu: {}", e);
                return;
            }
        };

        // Look up the message to determine ownership and pin status
        let message = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::error!("Message not found for context menu: {}", msg_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to look up message for context menu: {}", e);
                return;
            }
        };

        // Check if this is the current user's own message
        let current_user_id = auth_state.get().current_user_id.clone();
        let is_own_message = message.sender_id == current_user_id;

        // Check if message is pinned
        let is_pinned = message.is_pinned;

        // Show context menu
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_context_menu_message_id(message_id_str.into());
                w.set_context_menu_is_own_message(is_own_message);
                w.set_context_menu_is_pinned(is_pinned);
                // Position in center (fixed values work since menu uses full screen backdrop)
                w.set_context_menu_x(400.0);
                w.set_context_menu_y(300.0);
                w.set_context_menu_visible(true);
            }
        });
    });
}
