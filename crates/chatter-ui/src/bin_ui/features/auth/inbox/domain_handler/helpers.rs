//! Helper functions for domain event UI updates.

use slint::ComponentHandle;
use std::sync::Arc;

use slint::{Model, Weak};

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ConversationType;

/// Whether the open conversation, stored as `active` (see
/// `ConversationHandle::storage_kind`), takes an event about a row stored as
/// `event`. Both directions: a Saved view takes only `SavedMessages` events
/// and no other view takes one, so a same-id channel, DM or group row cannot
/// patch a Saved bubble sharing its author and clock, nor the reverse. A
/// `Channel` view also takes `AnonGroup` — an encrypted channel stores its
/// rows under that kind with its real channel id.
pub(super) fn accepts_event_kind(active: ConversationType, event: ConversationType) -> bool {
    match active {
        ConversationType::SavedMessages => event == ConversationType::SavedMessages,
        ConversationType::Channel => matches!(
            event,
            ConversationType::Channel | ConversationType::AnonGroup
        ),
        other => event == other,
    }
}

/// Update a chat-list row's unread badge.
pub(crate) fn update_unread_count(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: &str,
    count: u32,
) {
    let rows = window.global::<crate::SidebarPane>().get_chat_rows();
    for i in 0..rows.row_count() {
        if let Some(mut row) = rows.row_data(i) {
            if row.conversation_id.as_str() == channel_id {
                row.unread_count = count as i32;
                rows.set_row_data(i, row);
                crate::bin_ui::features::sidebar::refresh_flat_rows(window);
                recompute_total_unread(window, ui_ctx);
                return;
            }
        }
    }

    tracing::trace!(channel_id, count, "Unread count: no matching sidebar entry");
}

/// Update the mobile bottom-nav badge from the authoritative total computed
/// in `chatter-core`. The count is a GROUP BY over every message, so it runs
/// on the runtime and only the setter runs on the UI thread.
pub(crate) fn recompute_total_unread(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = slint::ComponentHandle::as_weak(window);
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let total = ui_ctx.unread_conversation_count();
        crate::run_ui!(weak, |w| w
            .global::<crate::SidebarPane>()
            .set_total_unread_count(total as i32));
    });
}

/// Handle pin/unpin domain events for UI update.
///
/// Updates the message's `is_pinned` flag, the pinned bar preview/count,
/// and the `ChatState` pinned-bar substate so that navigation arrows work correctly.
/// `message_id` and `actor` are the event's: for a Saved event the row is
/// re-read by them inside the UI closure, and the event's own flag is not
/// what gets painted.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_pin_ui_update(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
    conversation_type: ConversationType,
    message_id: chatter_core::domain::ids::MessageId,
    actor: chatter_core::domain::ids::UserId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    if !ui_ctx.is_showing_conversation(channel_id) {
        return;
    }
    // The bubble is matched by clock alone below, so the event's storage
    // kind has to be the open view's (`accepts_event_kind`), and the view
    // this was queued for has to be the one still open when it runs — same
    // session, conversation and history epoch (`HistoryOwner`), checked
    // again inside the UI closure before any state is moved.
    let conversation = ui_ctx.active_conversation();
    let owner = crate::bin_ui::features::history_owner::HistoryOwner::capture(ui_ctx, conversation);
    let handle = ui_ctx.conversation(conversation);
    if !handle
        .storage_kind()
        .is_ok_and(|active| accepts_event_kind(active, conversation_type.clone()))
    {
        return;
    }
    let refresh_ctx = ui_ctx.clone();
    let chat_state = ui_ctx.clone();
    let channel_id_str = channel_id.to_string();
    let target_ts_str = target_sent_timestamp.to_string();

    crate::run_history_ui!(window, owner, |w| {
        // A Saved pin is durable before its event, so the stored row as of
        // publication decides the flag and the preview, not the event: a
        // queued older pin cannot repaint over a later unpin, edit or
        // delete. The row must be the event's (real id) at this account's
        // Saved key — foreign, deleted or missing: nothing is painted — and
        // its bubble is matched by that id, not by clock.
        let saved = conversation_type == ConversationType::SavedMessages;
        let (new_pinned, mut preview) = if saved {
            let row = match refresh_ctx
                .conversation(conversation)
                .stored_message_scoped(&actor, target_sent_timestamp)
            {
                Ok(row) if row.id == message_id => row,
                _ => return,
            };
            let preview = if row.is_pinned {
                refresh_ctx.pinned_message_preview(&conversation, &row.id)
            } else {
                String::new()
            };
            (row.is_pinned, preview)
        } else {
            (!unpin, String::new())
        };
        let message_id_str = message_id.to_string();
        let messages = w.global::<crate::ChatPane>().get_messages();

        // Find the bubble, update is_pinned, generate preview
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                let hit = if saved {
                    msg.message_id.as_str() == message_id_str.as_str()
                } else {
                    msg.timestamp_ns.as_str() == target_ts_str.as_str()
                };
                if hit {
                    if !saved {
                        preview = super::super::utils::generate_preview_from_ui_message(&msg);
                    }

                    let mut updated_msg = msg.clone();
                    updated_msg.is_pinned = new_pinned;
                    if let Some(model) = messages
                        .as_any()
                        .downcast_ref::<slint::VecModel<crate::MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }

                    // Update pinned-bar substate with the real message_id from the UI model
                    if let Ok(msg_id) = msg
                        .message_id
                        .to_string()
                        .parse::<chatter_core::domain::ids::MessageId>()
                    {
                        if new_pinned {
                            chat_state.add_pinned_message(msg_id);
                        } else {
                            chat_state.remove_pinned_message(&msg_id);
                        }
                    }
                    break;
                }
            }
        }
        // The Saved row's id is known whether or not its bubble is loaded.
        if saved {
            if new_pinned {
                chat_state.add_pinned_message(message_id);
            } else {
                chat_state.remove_pinned_message(&message_id);
            }
        }

        let pinned_count = chat_state.pinned_total_count() as i32;
        if !new_pinned {
            w.global::<crate::ChatPane>()
                .set_pinned_total_count(pinned_count);
            w.global::<crate::ChatPane>()
                .set_has_pinned_messages(pinned_count > 0);
            if pinned_count == 0 {
                w.global::<crate::ChatPane>()
                    .set_pinned_message_preview("".into());
            }
        } else {
            // Use fallback if message wasn't in the loaded UI messages
            if preview.is_empty() {
                preview = "[Pinned message]".to_string();
            }
            w.global::<crate::ChatPane>()
                .set_pinned_total_count(pinned_count);
            w.global::<crate::ChatPane>().set_has_pinned_messages(true);
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview(preview.into());
            w.global::<crate::ChatPane>()
                .set_pinned_current_index(pinned_count);
        }

        crate::bin_ui::features::header::refresh_pinned_ui(&w, &refresh_ctx);

        tracing::debug!(
            channel_id = %channel_id_str,
            target_sent_timestamp,
            pinned = new_pinned,
            pinned_count,
            "Updated pin state from domain event"
        );
    });
}
