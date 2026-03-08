//! Context menu callback for messages.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;

use crate::MainWindow;

/// Wire more-clicked callback to show context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_more_clicked(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let message_service = ui_ctx.message_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let auth_state = ui_ctx.auth_state.clone();

    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_more_clicked(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_service = message_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let anon_group_service = anon_group_service.clone();
        let auth_state = auth_state.clone();
        let weak = weak_window.clone();

        // Get selected DM ID and private group ID to check conversation type
        let selected_dm_id = weak.upgrade()
            .map(|w| w.get_selected_dm_id().to_string())
            .unwrap_or_default();
        let selected_private_group_id = weak.upgrade()
            .map(|w| w.get_selected_private_group_id().to_string())
            .unwrap_or_default();

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

        // Check for anonymous DM - use anon_dm_service for lookup
        if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            // Anon DM message lookup
            let message = match anon_dm_service.get_message_by_sender_timestamp(
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
            let current_user_id = auth_state.get().current_user_id;
            let is_own_message = Some(message.sender_id) == current_user_id;

            // Anon DM messages don't have pin state in the message struct
            // Check UI model for pin state instead
            let is_pinned = weak.upgrade()
                .map(|w| {
                    let model = w.get_messages();
                    for i in 0..model.row_count() {
                        if let Some(m) = model.row_data(i) {
                            if m.sender_id.as_str() == sender_id_str.as_str()
                                && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                            {
                                return m.is_pinned;
                            }
                        }
                    }
                    false
                })
                .unwrap_or(false);

            // Show context menu
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_context_menu_sender_id(sender_id_str.into());
                    w.set_context_menu_timestamp_ns(timestamp_ns_str.into());
                    w.set_context_menu_is_own_message(is_own_message);
                    w.set_context_menu_is_pinned(is_pinned);
                    w.set_context_menu_x(400.0);
                    w.set_context_menu_y(300.0);
                    w.set_context_menu_visible(true);
                }
            });
            return;
        }

        // Check for anonymous group
        if !selected_private_group_id.is_empty() {
            let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!(group_id = %selected_private_group_id, "Invalid anon group ID");
                    return;
                }
            };

            // Anon group message lookup
            let message = match anon_group_service.get_message_by_sender_timestamp(
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
            let current_user_id = auth_state.get().current_user_id;
            let is_own_message = Some(message.sender_id) == current_user_id;

            // Anon group messages don't have pin state - check UI model
            let is_pinned = weak.upgrade()
                .map(|w| {
                    let model = w.get_messages();
                    for i in 0..model.row_count() {
                        if let Some(m) = model.row_data(i) {
                            if m.sender_id.as_str() == sender_id_str.as_str()
                                && m.timestamp_ns.as_str() == timestamp_ns_str.as_str()
                            {
                                return m.is_pinned;
                            }
                        }
                    }
                    false
                })
                .unwrap_or(false);

            // Show context menu
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_context_menu_sender_id(sender_id_str.into());
                    w.set_context_menu_timestamp_ns(timestamp_ns_str.into());
                    w.set_context_menu_is_own_message(is_own_message);
                    w.set_context_menu_is_pinned(is_pinned);
                    w.set_context_menu_x(400.0);
                    w.set_context_menu_y(300.0);
                    w.set_context_menu_visible(true);
                }
            });
            return;
        }

        // Regular message lookup via message_service
        let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
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
