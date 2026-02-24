//! Context menu callback for messages.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;

use crate::MainWindow;

/// Wire more-clicked callback to show context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_more_clicked(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let auth_state = ctx.auth_state.clone();

    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_more_clicked(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_repo = message_repo.clone();
        let auth_state = auth_state.clone();
        let weak = weak_window.clone();

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

        // Signal-style lookup: (sender_id, sent_at)
        let message = match message_repo.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
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
        let current_user_id = auth_state.get().current_user_id.clone();
        let is_own_message = message.sender_id == current_user_id;

        // Check if message is pinned
        let is_pinned = message.is_pinned;

        // Show context menu with Signal-style IDs
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_context_menu_sender_id(sender_id_str.into());
                w.set_context_menu_timestamp_ns(timestamp_ns_str.into());
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
