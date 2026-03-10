//! Channel selection and message loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::services::attachment::AttachmentService;
use chatter_linux::services::channel::ChannelService;
use chatter_linux::services::message::MessageService;
use chatter_linux::services::user::UserService;

use crate::bin_ui::converters::{channel_type_to_string, message_to_display_data, MessageDisplayData};
use crate::bin_ui::features::chat::send_channel_read_receipts;
use crate::MainWindow;

use super::stop_audio_playback;

pub(crate) fn wire_channel_selected(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let chat_state = ui_ctx.chat_state.clone();
    let channel_service = ui_ctx.channel_service.clone();
    let message_service = ui_ctx.message_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let group_coordinator = ui_ctx.group_coordinator.clone();
    let runtime_handle = ui_ctx.runtime_handle.clone();
    let pin_service = ui_ctx.pin_service.clone();
    let pinned_state = ui_ctx.pinned_state.clone();

    let weak_window = window.as_weak();

    window.on_channel_selected(move |channel_id: SharedString| {
        let channel_id_str = channel_id.to_string();
        tracing::info!(
            channel_id = %channel_id_str,
            "[CHANNEL_SELECTED] on_channel_selected triggered"
        );
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        let group_coordinator = group_coordinator.clone();
        let runtime_handle = runtime_handle.clone();
        let pin_service = pin_service.clone();
        let pinned_state = pinned_state.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_channel_id(channel_id.clone());
                    w.set_selected_dm_id("".into());
                }
            });

            // Parse channel ID
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid channel ID: {}", e);
                    return;
                }
            };

            // Update chat state
            chat_state.set_active_channel(channel_id);

            // Get current user ID from user service
            let current_user_id = user_service.current_user_id().ok().map(|id| id.to_string());
            tracing::info!(
                channel_id = %channel_id,
                current_user_id = ?current_user_id,
                "[CHANNEL_SELECTED] current_user_id for is_own check"
            );

            // Load pinned messages for this channel
            if let Ok(pinned_messages) = pin_service.load_pinned_for_channel(&channel_id).await {
                if !pinned_messages.is_empty() {
                    let message_ids: Vec<_> = pinned_messages.iter().map(|p| p.message_id).collect();
                    pinned_state.set_pinned_messages(channel_id, message_ids);
                    
                    if let Some(first) = pinned_messages.first() {
                        let preview = first.preview.clone();
                        let total_count = pinned_messages.len() as i32;
                        let should_show = pinned_state.should_show();
                        
                        let weak_pinned = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_pinned.upgrade() {
                                w.set_has_pinned_messages(should_show);
                                w.set_pinned_message_preview(preview.into());
                                w.set_pinned_current_index(1); // 1-based for display
                                w.set_pinned_total_count(total_count);
                            }
                        });
                    }
                } else {
                    pinned_state.clear();
                    let weak_pinned = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_pinned.upgrade() {
                            w.set_has_pinned_messages(false);
                        }
                    });
                }
            }

            // Load channel and messages
            let message_timestamps = load_channel_into_ui(
                weak,
                &channel_id,
                channel_service,
                message_service.clone(),
                user_service,
                attachment_service,
                current_user_id,
            ).await;

            // Send read receipts for all loaded messages
            if !message_timestamps.is_empty() {
                send_channel_read_receipts(
                    channel_id,
                    message_timestamps,
                    message_service,
                    group_coordinator,
                    runtime_handle,
                );
            }
        });
    });
}

/// Load channel data and messages into UI properties.
/// Load channel into UI and return message timestamps for read receipts.
pub async fn load_channel_into_ui(
    window_weak: slint::Weak<MainWindow>,
    channel_id: &ChannelId,
    channel_service: Arc<ChannelService>,
    message_service: Arc<MessageService>,
    user_service: Arc<UserService>,
    attachment_service: Arc<AttachmentService>,
    current_user_id: Option<String>,
) -> Vec<u64> {
    tracing::info!(
        channel_id = %channel_id,
        "[LOAD_CHANNEL] Starting load_channel_into_ui"
    );

    // Get channel info from local service
    let channel = match channel_service.get_channel_local(channel_id) {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            tracing::warn!("Channel {} not found in local repo", channel_id);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return Vec::new();
        }
        Err(e) => {
            tracing::error!("Failed to get channel {}: {}", channel_id, e);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return Vec::new();
        }
    };

    // Fetch recent messages (limit 50)
    let messages = match message_service.get_messages_local(channel_id, None, 50) {
        Ok(msgs) => {
            tracing::info!(
                channel_id = %channel_id,
                message_count = msgs.len(),
                "[LOAD_CHANNEL] Retrieved {} messages from local DB",
                msgs.len()
            );
            msgs
        }
        Err(e) => {
            tracing::error!("Failed to get messages for channel {}: {}", channel_id, e);
            Vec::new()
        }
    };

    // Collect timestamps for read receipts (only for messages not sent by us)
    let current_user = current_user_id.as_ref();
    let message_timestamps: Vec<u64> = messages
        .iter()
        .filter(|msg| {
            // Only send read receipts for messages from others
            msg.sender_id.as_ref().map(|id| id.to_string()).as_ref() != current_user
        })
        .map(|msg| msg.sent_at)
        .collect();

    // Convert messages to display data (thread-safe, can be sent to event loop)
    // Messages come from DB in DESC order (newest first), but chat UI shows oldest first
    let mut display_messages: Vec<MessageDisplayData> = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = if let Some(sender_id) = &msg.sender_id {
            user_service.get_display_name(sender_id)
        } else {
            "Anonymous".to_string()
        };

        // Resolve attachments for this message
        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| {
                match attachment_service.get(att_id) {
                    Ok(Some(att)) => {
                        tracing::debug!(
                            "Resolved attachment {} for message {}: local_path={:?}",
                            att_id, msg.id, att.local_path
                        );
                        Some(att)
                    }
                    Ok(None) => {
                        tracing::warn!("Attachment {} not found in repo for message {}", att_id, msg.id);
                        None
                    }
                    Err(e) => {
                        tracing::error!("Error fetching attachment {}: {}", att_id, e);
                        None
                    }
                }
            })
            .collect();

        if !msg.attachments.is_empty() {
            tracing::debug!(
                "Message {} has {} attachment IDs, resolved {} attachments",
                msg.id, msg.attachments.len(), resolved_attachments.len()
            );
        }

        display_messages.push(message_to_display_data(msg, &sender_name, &resolved_attachments));
    }

    // Reverse to show oldest first (newest at bottom, like chat apps)
    display_messages.reverse();

    // Pre-decode attachment thumbnails before UI thread
    for msg in &mut display_messages {
        msg.decode_attachment_thumbnails();
    }

    // Update UI - special handling for SavedMessages and DMs
    let is_saved_messages = channel.channel_type == ChannelType::SavedMessages;
    let is_dm = channel.channel_type == ChannelType::DirectMessage;
    let channel_id_for_lookup = channel.id;

    // For DMs, we'll look up the peer's display name in the event loop
    // where we have access to the DM conversations model
    let channel_name_from_repo = if is_saved_messages {
        Some("Saved Messages".to_string())
    } else if is_dm {
        None // Will look up from DM conversations in event loop
    } else {
        Some(channel.name.clone())
    };
    let channel_type = channel_type_to_string(channel.channel_type);
    let member_count = 0i32;
    // Don't show topic/description for SavedMessages or DMs
    let topic = if is_saved_messages || is_dm {
        String::new()
    } else {
        channel.description.clone().unwrap_or_default()
    };
    let is_archived = channel.archived;
    let attachment_service_for_gifs = attachment_service.clone();
    let current_user_id = current_user_id.unwrap_or_default();

    let display_count = display_messages.len();
    tracing::info!(
        channel_id = %channel_id_for_lookup,
        display_count = display_count,
        "[LOAD_CHANNEL] Setting {} messages in UI",
        display_count
    );

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak.upgrade() {
            // Convert display data to Slint types (must be done on UI thread)
            // Pass current_user_id so is_own is set correctly for own messages
            let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
            tracing::debug!(
                "[LOAD_CHANNEL] invoke_from_event_loop: setting {} slint messages",
                slint_messages.len()
            );

            // For DMs, look up the peer's display name from DM conversations
            let channel_name = channel_name_from_repo.unwrap_or_else(|| {
                let conversations = w.get_dm_conversations();
                let channel_id_str = channel_id_for_lookup.to_string();
                for i in 0..conversations.row_count() {
                    if let Some(item) = conversations.row_data(i) {
                        if item.channel_id.as_str() == channel_id_str {
                            let name = item.display_name.to_string();
                            if !name.is_empty() {
                                return name;
                            }
                        }
                    }
                }
                // Fall back to channel_id if no name found
                channel_id_str
            });

            w.set_channel_name(channel_name.into());
            w.set_channel_type(channel_type.into());
            w.set_is_dm(channel_type == "dm");
            w.set_member_count(member_count);
            w.set_topic(topic.into());
            w.set_is_archived(is_archived);
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);

            // Auto-register GIF animations for inline playback
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), attachment_service_for_gifs);
        }
    });

    message_timestamps
}

