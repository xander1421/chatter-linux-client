//! DM message receiving handlers.
//!
//! Handles incoming DM messages and sidebar updates.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::message::Message;
use chatter_linux::proto::content_v1::AttachmentPointer;

use crate::bin_ui::converters::{
    attachment_pointer_to_display_data, message_to_display_data_with_attachments,
};
use crate::bin_ui::features::chat::get_dm_channel_from_peer;
use crate::{DMListItemData, MainWindow, MessageItemData};

use super::super::typing::clear_typing_for_peer;
use super::super::utils::format_timestamp;
use super::download::auto_download_attachments;

/// Handle received DM message.
pub fn handle_dm_received(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    peer_id: chatter_linux::domain::UserId,
    text: String,
    timestamp: u64,
    message_id: String,
    attachments: Vec<AttachmentPointer>,
) {
    tracing::info!("DM received from {}: {} chars, {} attachments", peer_id, text.len(), attachments.len());

    // Collect attachments with encryption keys for auto-download
    let download_pointers: Vec<AttachmentPointer> = attachments
        .iter()
        .filter(|p| !p.cdn_key.is_empty() && !p.encryption_key.is_empty())
        .cloned()
        .collect();

    // Auto-download ALL attachments in background
    if !download_pointers.is_empty() {
        let ui_ctx = ui_ctx.clone();
        let weak_for_download = weak_window.clone();
        let peer_id_for_download = peer_id.to_string();

        tokio::spawn(async move {
            auto_download_attachments(
                download_pointers,
                ui_ctx,
                weak_for_download,
                peer_id_for_download,
            ).await;
        });
    }

    // Try to get display name from profile, fall back to user
    let sender_name_from_repo = ui_ctx.user_service
        .get_profile_local(&peer_id)
        .ok()
        .flatten()
        .map(|p| p.display_name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| {
        ui_ctx.user_service
        .get_user_local(&peer_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .filter(|name| !name.is_empty())
        });

        // Build attachment display data from pointers for immediate display

    let attachment_display: Vec<_> = attachments
        .iter()
        .filter(|p| !p.cdn_key.is_empty())
        .map(|p| attachment_pointer_to_display_data(p))
        .collect();

    // Build preview and timestamp for sidebar
    let timestamp_str = format_timestamp(timestamp);
    let attachment_count = attachments.len();
    let preview = if text.is_empty() && attachment_count > 0 {
        format!("📎 {} attachment{}", attachment_count, if attachment_count == 1 { "" } else { "s" })
    } else if text.chars().count() > 50 {
        let truncated: String = text.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        text.clone()
    };

    let message_id_for_ui = message_id.clone();
    let text_for_ui = text.clone();

    let weak = weak_window.clone();
    let ui_ctx_cloned = ui_ctx.clone();
    let peer_id_cloned = peer_id.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Get sender name from DM conversations if not in repo
        let sender_name = sender_name_from_repo.unwrap_or_else(|| {
            let conversations = window.get_dm_conversations();
            for i in 0..conversations.row_count() {
                if let Some(item) = conversations.row_data(i) {
                    if item.user_id.as_str() == peer_id_cloned.to_string() {
                        let name = item.display_name.to_string();
                        if !name.is_empty() {
                            return name;
                        }
                    }
                }
            }
            peer_id_cloned.to_string()
        });

        let selected_dm = window.get_selected_dm_id().to_string();

        // Check if this is the active conversation
        let peer_id_str = peer_id_cloned.to_string();
        let peer_channel_id = get_dm_channel_from_peer(&window, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;

        // Clear typing timer (Signal-style)
        clear_typing_for_peer(&window, &peer_id_str);

        if is_active {
            // Check if user is at bottom before updating
            let was_at_bottom = window.get_is_at_bottom();

            // Get current messages model
            let current_model = window.get_messages();
            let mut messages: Vec<MessageItemData> = (0..current_model.row_count())
                .filter_map(|i| current_model.row_data(i))
                .collect();

            // Check if this message is already in the UI
            let timestamp_ns_str = timestamp.to_string();
            let already_exists = messages.iter().any(|m| {
                m.sender_id.as_str() == peer_id_str && m.timestamp_ns.as_str() == timestamp_ns_str
            });

            if !already_exists {
                // Use shared converter to create MessageItemData
                // Create a temporary domain Message object for conversion
                let mut domain_msg = Message::new_text(
                    chatter_linux::domain::ids::ChannelId::new(), // DMs use empty/virtual channel ID
                    Some(peer_id_cloned),
                    text_for_ui,
                    timestamp,
                );
                
                // Use the provided message_id if valid, otherwise keep the generated one
                if let Ok(msg_id) = message_id_for_ui.parse() {
                    domain_msg.id = msg_id;
                }

                let current_user_id = ui_ctx_cloned
                    .auth_state
                    .get()
                    .current_user_id
                    .map(|id| id.to_string())
                    .unwrap_or_default();

                let display_data = message_to_display_data_with_attachments(
                    &domain_msg,
                    &sender_name,
                    attachment_display,
                );

                let mut new_msg = display_data.to_slint_with_user(&current_user_id);
                new_msg.delivery_status = "delivered".into();

                // Append and sort by timestamp + sender_id (Signal-style deterministic ordering)
                messages.push(new_msg);
                messages.sort_by(|a, b| {
                    let ts_a = a.timestamp_ns.to_string().parse::<u64>().unwrap_or(0);
                    let ts_b = b.timestamp_ns.to_string().parse::<u64>().unwrap_or(0);
                    ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
                });

                window.set_messages(ModelRc::new(VecModel::from(messages)));

                // Auto-register GIFs for animation
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_cloned.attachment_service.clone(),
                );
            }

            // Auto-scroll to bottom if user was at bottom before new message
            if was_at_bottom {
                window.set_message_scroll_position(-99999.0);
                window.set_is_at_bottom(true);
            }

            // Send read receipt for this message since conversation is active
            if let Some(id_str) = peer_channel_id.as_ref() {
                if let Some(anon_dm_id_str) = id_str.strip_prefix("anon_dm:") {
                    if let Ok(dm_id) = anon_dm_id_str.parse::<chatter_linux::domain::anon_dm::AnonDmId>() {
                        let service = ui_ctx_cloned.anon_dm_service.clone();
                        tokio::spawn(async move {
                            let _ = service.send_read_receipt(&dm_id, vec![timestamp]).await;
                        });
                    }
                }
            }
        }

        // Always update sidebar
        update_dm_sidebar(&window, &ui_ctx_cloned, &peer_id_cloned, &sender_name, &preview, &timestamp_str);
    });
}

/// Update DM sidebar with latest message preview.
pub fn update_dm_sidebar(
    window: &MainWindow,
    ui_ctx: &Arc<UIContext>,
    peer_id: &chatter_linux::domain::UserId,
    sender_name: &str,
    preview: &str,
    timestamp_str: &str,
) {
    let peer_id_str = peer_id.to_string();
    let dms = window.get_dm_conversations();
    let mut dm_vec: Vec<DMListItemData> = Vec::new();
    let mut updated_item = None;

    // Extract all items, identifying the one to update
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            if dm.user_id.as_str() == peer_id_str {
                let mut updated = dm.clone();
                updated.last_message = preview.into();
                updated.timestamp = timestamp_str.into();
                // Increment unread count if it's not the active conversation
                let selected_dm = window.get_selected_dm_id().to_string();
                if selected_dm != updated.channel_id.as_str() && selected_dm != updated.user_id.as_str() {
                    updated.unread_count += 1;
                }
                updated_item = Some(updated);
            } else {
                dm_vec.push(dm);
            }
        }
    }

    if let Some(item) = updated_item {
        // Move to top
        dm_vec.insert(0, item);
    } else {
        // Peer not found in sidebar - use dm_to_slint to create it if we can find the channel
        match ui_ctx.channel_service.get_dm_channel_for_peer(peer_id) {
            Ok(Some(channel)) => {
                let mut new_dm = crate::bin_ui::converters::dm_to_slint(&channel, sender_name, &peer_id_str);
                new_dm.last_message = preview.into();
                new_dm.timestamp = timestamp_str.into();
                new_dm.unread_count = 1; // First message in this session
                dm_vec.insert(0, new_dm);
                
                tracing::info!(
                    peer_id = %peer_id_str,
                    channel_id = %channel.id,
                    "Added missing DM conversation to sidebar using dm_to_slint"
                );
            }
            _ => {
                tracing::warn!(
                    peer_id = %peer_id_str,
                    sender_name = %sender_name,
                    "DM conversation not in sidebar and channel not found - cannot add"
                );
            }
        }
    }

    window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(dm_vec)));
}

/// Handle DM unread count update.
pub fn handle_dm_unread_count(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    peer_id: chatter_linux::domain::UserId,
    unread_count: u32,
) {
    tracing::debug!(
        "DM unread count updated: {} (peer {}) = {}",
        channel_id,
        peer_id,
        unread_count
    );

    if let Some(window) = weak_window.upgrade() {
        let peer_id_str = peer_id.to_string();
        let dms = window.get_dm_conversations();

        for i in 0..dms.row_count() {
            if let Some(dm) = dms.row_data(i) {
                if dm.user_id.as_str() == peer_id_str {
                    let mut updated_dm = dm.clone();
                    updated_dm.unread_count = unread_count as i32;

                    if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                        model.set_row_data(i, updated_dm);
                    }
                    break;
                }
            }
        }
    }
}
