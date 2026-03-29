//! Anonymous DM event handlers.

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::{AnonDmId, AnonDmMessageId};
use chatter_core::domain::ids::UserId;
use chatter_core::domain::message::Message;
use crate::MainWindow;

/// Handle anonymous DM request received.
pub(super) fn handle_request_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anon DM list to show the request
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);

            // Update pending message request count
            if let Ok(requests) = ui_ctx.anon_dms().list_message_requests() {
                w.set_pending_message_requests(requests.len() as i32);
            }
        }
    });
}

/// Handle anonymous DM request accepted.
pub(super) fn handle_request_accepted(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    peer_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anon DM list
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
            tracing::info!(
                conversation_id = %conversation_id,
                peer_id = %peer_id,
                "Anon DM request accepted"
            );
        }
    });
}

/// Handle anonymous DM message received.
///
/// Fetches the stored message, resolves attachments and sender info,
/// pushes into the active Slint message model if this conversation is
/// currently selected, and always updates the sidebar preview.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    sender_id: UserId,
    message_id: AnonDmMessageId,
    _sent_at: u64,
) {
    // Fetch the stored message from the DB
    let msg = match ui_ctx.anon_dms().get_message(&message_id) {
        Ok(Some(m)) => m,
        _ => {
            tracing::warn!(
                message_id = %message_id,
                "AnonDmMessageReceived: message not found in store"
            );
            // Still refresh sidebar as fallback
            let weak = window.clone();
            let ui_ctx = ui_ctx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
                }
            });
            return;
        }
    };

    // Resolve sender display name
    let sender_name = ui_ctx.users().get_display_name(&sender_id);

    // Get conversation for peer avatar
    let dm = ui_ctx.anon_dms().get_conversation(&conversation_id).ok().flatten();
    let current_user_id = ui_ctx.sessions().current_user_id();
    let is_own = current_user_id.map(|uid| sender_id == uid).unwrap_or(false);
    let current_user_id_str = current_user_id
        .map(|uid| uid.to_string())
        .unwrap_or_default();

    // Determine sender avatar
    let sender_avatar = if is_own {
        ui_ctx.anon_dms().get_my_avatar()
    } else {
        dm.as_ref().and_then(|d| d.peer_avatar.clone())
    };

    // Resolve attachments from stored IDs
    let attachment_ids = msg.attachments.clone();
    let resolved_attachments: Vec<chatter_core::domain::Attachment> = attachment_ids
        .iter()
        .filter_map(|att_id| ui_ctx.attachments().get(att_id).ok().flatten())
        .collect();

    // Build display data (off UI thread)
    let mut display_data = crate::bin_ui::converters::anon_dm_message_to_display_data(
        &msg,
        &sender_name,
        sender_avatar,
        &resolved_attachments,
    );
    display_data.decode_attachment_thumbnails();

    // Build sidebar preview
    let msg_text = msg.as_text().unwrap_or_default().to_string();
    let att_count = resolved_attachments.len();
    let preview = if msg_text.is_empty() && att_count > 0 {
        format!("\u{1F4CE} {} attachment{}", att_count, if att_count == 1 { "" } else { "s" })
    } else if msg_text.chars().count() > 50 {
        let truncated: String = msg_text.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        msg_text.to_string()
    };
    let timestamp_str = super::super::utils::format_timestamp(msg.sent_at);

    // Trigger attachment auto-download in background
    let downloadable: Vec<_> = resolved_attachments.iter()
        .filter(|a| a.remote_url.is_some() && a.encryption_key.is_some())
        .cloned()
        .collect();

    if !downloadable.is_empty() {
        let ui_ctx_dl = ui_ctx.clone();
        let weak_dl = window.clone();
        let conv_id = conversation_id.to_string();
        tokio::spawn(async move {
            super::super::attachment_download::download_attachments(
                &downloadable,
                ui_ctx_dl.attachments().clone(),
                weak_dl,
                conv_id,
            ).await;
        });
    }

    let conversation_id_str = conversation_id.to_string();
    let weak = window.clone();
    let ui_ctx_ui = ui_ctx.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // Check if this anon DM is currently selected
        // Anon DMs use "anon_dm:{conversation_id}" as the selected_dm_id
        let selected_dm = w.get_selected_dm_id().to_string();
        let is_active = selected_dm
            .strip_prefix("anon_dm:")
            .map(|id_str| id_str == conversation_id_str)
            .unwrap_or(false);

        if is_active {
            let was_at_bottom = w.get_is_at_bottom();

            // Build Slint message item
            let new_msg = display_data.to_slint_with_user(&current_user_id_str);

            // Deduplicate by (sender_id, timestamp_ns)
            let current_model = w.get_messages();
            let ts_str = new_msg.timestamp_ns.to_string();
            let sender_str = new_msg.sender_id.to_string();
            let already_exists = (0..current_model.row_count()).any(|i| {
                current_model.row_data(i).map(|m| {
                    m.sender_id.as_str() == sender_str && m.timestamp_ns.as_str() == ts_str
                }).unwrap_or(false)
            });

            if !already_exists {
                if let Some(vec_model) = current_model.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                    vec_model.push(new_msg);
                } else {
                    let mut messages: Vec<crate::MessageItemData> = (0..current_model.row_count())
                        .filter_map(|i| current_model.row_data(i))
                        .collect();
                    messages.push(new_msg);
                    w.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
                }

                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_ui.attachments().clone(),
                );
            }

            if was_at_bottom {
                w.set_message_scroll_position(-99999.0);
                w.set_is_at_bottom(true);
            }
        }

        // Always update sidebar preview
        let conversation = chatter_core::state::ActiveConversation::AnonDm(conversation_id);
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w, conversation, &preview, &timestamp_str, true,
        );
    });
}

/// Handle anonymous DM message received via unified MessageReceived event.
///
/// Uses the Message struct directly from the event instead of fetching from
/// the anon DM repo. The message carries all needed data via conversation_type.
pub(super) fn handle_message_received_unified(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    let conversation_id = AnonDmId::from_uuid(*message.conversation_id.as_uuid());
    let sender_id = message.sender_id.unwrap_or_else(UserId::new);

    // Resolve sender display name
    let sender_name = ui_ctx.users().get_display_name(&sender_id);

    // Get conversation for peer avatar
    let dm = ui_ctx.anon_dms().get_conversation(&conversation_id).ok().flatten();
    let current_user_id = ui_ctx.sessions().current_user_id();
    let is_own = current_user_id.map(|uid| sender_id == uid).unwrap_or(false);
    let current_user_id_str = current_user_id
        .map(|uid| uid.to_string())
        .unwrap_or_default();

    // Determine sender avatar
    let sender_avatar = if is_own {
        ui_ctx.anon_dms().get_my_avatar()
    } else {
        dm.as_ref().and_then(|d| d.peer_avatar.clone())
    };

    // Resolve attachments from stored IDs
    let resolved_attachments: Vec<chatter_core::domain::Attachment> = message.attachments
        .iter()
        .filter_map(|att_id| ui_ctx.attachments().get(att_id).ok().flatten())
        .collect();

    // Build display data from the unified Message
    let mut display_data = crate::bin_ui::converters::anon_dm_message_to_display_data(
        &message,
        &sender_name,
        sender_avatar,
        &resolved_attachments,
    );
    display_data.decode_attachment_thumbnails();

    // Build sidebar preview
    let msg_text = message.as_text().unwrap_or_default().to_string();
    let preview = if msg_text.is_empty() {
        "[Message]".to_string()
    } else if msg_text.chars().count() > 50 {
        let truncated: String = msg_text.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        msg_text.to_string()
    };
    let timestamp_str = super::super::utils::format_timestamp(message.sent_at);

    let conversation_id_str = conversation_id.to_string();
    let weak = window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // Check if this anon DM is currently selected
        let selected_dm = w.get_selected_dm_id().to_string();
        let is_active = selected_dm
            .strip_prefix("anon_dm:")
            .map(|id_str| id_str == conversation_id_str)
            .unwrap_or(false);

        if is_active {
            let was_at_bottom = w.get_is_at_bottom();

            let new_msg = display_data.to_slint_with_user(&current_user_id_str);

            // Deduplicate by (sender_id, timestamp_ns)
            let current_model = w.get_messages();
            let ts_str = new_msg.timestamp_ns.to_string();
            let sender_str = new_msg.sender_id.to_string();
            let already_exists = (0..current_model.row_count()).any(|i| {
                current_model.row_data(i).map(|m| {
                    m.sender_id.as_str() == sender_str && m.timestamp_ns.as_str() == ts_str
                }).unwrap_or(false)
            });

            if !already_exists {
                if let Some(vec_model) = current_model.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                    vec_model.push(new_msg);
                } else {
                    let mut messages: Vec<crate::MessageItemData> = (0..current_model.row_count())
                        .filter_map(|i| current_model.row_data(i))
                        .collect();
                    messages.push(new_msg);
                    w.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
                }
            }

            if was_at_bottom {
                w.set_message_scroll_position(-99999.0);
                w.set_is_at_bottom(true);
            }
        }

        // Always update sidebar preview
        let conversation = chatter_core::state::ActiveConversation::AnonDm(conversation_id);
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w, conversation, &preview, &timestamp_str, true,
        );
    });
}

/// Handle anonymous DM init received.
pub(super) fn handle_init_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anon DM list for new init
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
        }
    });
}
