//! Helper functions for domain event UI updates.

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_core::client::ChatterClient;
use crate::MainWindow;

/// Update unread count badge in sidebar.
pub(super) fn update_unread_count(window: &MainWindow, channel_id: &str, count: u32) {
    // This would update the sidebar model
    // Implementation depends on how sidebar models are structured
    tracing::trace!(channel_id, count, "Updating unread count badge");
    let _ = (window, channel_id, count); // Suppress unused warnings for now
}

/// Handle pin/unpin domain events for UI update.
///
/// Updates the message's `is_pinned` flag, the pinned bar preview/count,
/// and the `PinnedState` so that navigation arrows work correctly.
pub(super) fn handle_pin_ui_update(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    let weak = window.clone();
    let pinned_state = ui_ctx.pinned_state().clone();
    let channel_id_str = channel_id.to_string();
    let target_ts_str = target_sent_timestamp.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // Check if the pin is for the currently active conversation
        let selected_dm = w.get_selected_dm_id().to_string();
        let selected_channel = w.get_selected_channel_id().to_string();
        let selected_group = w.get_selected_private_group_id().to_string();

        let is_active = selected_dm.contains(&channel_id_str)
            || selected_channel == channel_id_str
            || selected_group == channel_id_str;
        if !is_active {
            return;
        }

        let messages = w.get_messages();
        let new_pinned = !unpin;
        let mut preview = String::new();

        // Find message by timestamp_ns, update is_pinned, generate preview
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ns.as_str() == target_ts_str.as_str() {
                    preview = super::super::utils::generate_preview_from_ui_message(&msg);

                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = new_pinned;
                    if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                        model.set_row_data(i, updated_msg);
                    }

                    // Update pinned_state with the real message_id from the UI model
                    if let Ok(msg_id) = msg.message_id.to_string().parse::<chatter_core::domain::ids::MessageId>() {
                        if new_pinned {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                    }
                    break;
                }
            }
        }

        let pinned_count = pinned_state.get().pinned_message_ids.len() as i32;
        if unpin {
            w.set_pinned_total_count(pinned_count);
            w.set_has_pinned_messages(pinned_count > 0);
            if pinned_count == 0 {
                w.set_pinned_message_preview("".into());
            }
        } else {
            // Use fallback if message wasn't in the loaded UI messages
            if preview.is_empty() {
                preview = "[Pinned message]".to_string();
            }
            w.set_pinned_total_count(pinned_count);
            w.set_has_pinned_messages(true);
            w.set_pinned_message_preview(preview.into());
            w.set_pinned_current_index(pinned_count);
        }

        tracing::debug!(
            channel_id = %channel_id_str,
            target_sent_timestamp,
            pinned = new_pinned,
            pinned_count,
            "Updated pin state from domain event"
        );
    });
}
