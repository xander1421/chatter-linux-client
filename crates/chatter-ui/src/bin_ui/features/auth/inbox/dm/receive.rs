//! DM message receiving handlers.
//!
//! Handles incoming DM messages and sidebar updates.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::message::Message;
use chatter_core::proto::content_v1::AttachmentPointer;

use crate::bin_ui::converters::{
    attachment_pointer_to_display_data, message_to_display_data_with_attachments,
};
use crate::{DMListItemData, MainWindow, MessageItemData};

use super::super::typing::clear_typing_for_peer;
use super::super::utils::format_timestamp;
use super::download::auto_download_attachments;

/// Handle received DM message.
pub fn handle_dm_received(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    peer_id: chatter_core::domain::UserId,
    dm_channel_id: chatter_core::domain::ids::ChannelId,
    text: String,
    timestamp: u64,
    message_id: String,
    attachments: Vec<AttachmentPointer>,
) {
    tracing::info!("DM received from {}: {} chars, {} attachments, channel={}", peer_id, text.len(), attachments.len(), dm_channel_id);

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
    let sender_name_from_repo = ui_ctx.users()
        .get_profile_local(&peer_id)
        .ok()
        .flatten()
        .map(|p| p.display_name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| {
        ui_ctx.users()
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

    // Determine is_active from ChatState (source of truth), not the Slint sidebar model.
    // ChatState.active_channel_id is set in dm.rs:106 when user navigates to a DM.
    let is_active = ui_ctx
        .chat_state()
        .get()
        .active_channel_id
        .map(|id| id == dm_channel_id)
        .unwrap_or(false);

    tracing::debug!(
        dm_channel_id = %dm_channel_id,
        peer_id = %peer_id,
        is_active,
        "DM is_active check (from ChatState)"
    );

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

        let peer_id_str = peer_id_cloned.to_string();

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
                    chatter_core::domain::ids::ChannelId::new(), // DMs use empty/virtual channel ID
                    Some(peer_id_cloned),
                    text_for_ui,
                    timestamp,
                );
                
                // Use the provided message_id if valid, otherwise keep the generated one
                if let Ok(msg_id) = message_id_for_ui.parse() {
                    domain_msg.id = msg_id;
                }

                let current_user_id = ui_ctx_cloned
                    .auth_state()
                    .get()
                    .current_user_id
                    .map(|id| id.to_string())
                    .unwrap_or_default();

                let mut display_data = message_to_display_data_with_attachments(
                    &domain_msg,
                    &sender_name,
                    attachment_display,
                );
                // Pre-decode attachment thumbnails before Slint conversion
                display_data.decode_attachment_thumbnails();

                let mut new_msg = display_data.to_slint_with_user(&current_user_id);
                new_msg.delivery_status = "delivered".into();

                // Find insert position (sorted by timestamp + sender_id)
                let new_ts = new_msg.timestamp_ns.to_string().parse::<u64>().unwrap_or(0);
                let new_sender = new_msg.sender_id.to_string();
                let insert_pos = messages.iter().position(|m| {
                    let ts = m.timestamp_ns.to_string().parse::<u64>().unwrap_or(0);
                    match ts.cmp(&new_ts) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Equal => m.sender_id.as_str() > new_sender.as_str(),
                        std::cmp::Ordering::Less => false,
                    }
                }).unwrap_or(messages.len());

                // Insert directly into existing model to avoid full re-render
                if let Some(vec_model) = current_model.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                    vec_model.insert(insert_pos, new_msg);
                } else {
                    // Fallback: replace entire model
                    messages.insert(insert_pos, new_msg);
                    window.set_messages(ModelRc::new(VecModel::from(messages)));
                }

                // Auto-register GIFs for animation
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_cloned.attachments().clone(),
                );
            }

            // Auto-scroll to bottom if user was at bottom before new message
            if was_at_bottom {
                window.set_message_scroll_position(-99999.0);
                window.set_is_at_bottom(true);
            }

            // Send read receipt for this message since conversation is active
            let channel_id_str = dm_channel_id.to_string();
            if let Some(anon_dm_id_str) = channel_id_str.strip_prefix("anon_dm:") {
                if let Ok(dm_id) = anon_dm_id_str.parse::<chatter_core::domain::anon_dm::AnonDmId>() {
                    let service = ui_ctx_cloned.anon_dms().clone();
                    tokio::spawn(async move {
                        let _ = service.send_read_receipt(&dm_id, vec![timestamp]).await;
                    });
                }
            }
        }

        // Always update sidebar (with unread increment for received messages)
        let conv = chatter_core::state::ActiveConversation::Dm {
            channel_id: dm_channel_id,
            peer_id: peer_id_cloned,
        };
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &window, conv, &preview, &timestamp_str, true,
        );

        // If DM wasn't in sidebar (new peer), create it
        if !crate::bin_ui::features::sidebar::sidebar_has_dm(&window, &peer_id_cloned) {
            crate::bin_ui::features::sidebar::create_dm_sidebar_entry(
                &window, &ui_ctx_cloned, &peer_id_cloned, &sender_name, &preview, &timestamp_str,
            );
        }
    });
}

/// Handle DM unread count update.
pub fn handle_dm_unread_count(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_core::domain::ids::ChannelId,
    peer_id: chatter_core::domain::UserId,
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
