//! Channel selection and message loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::message::ConversationType;
use chatter_core::domain::ids::ChannelId;
use chatter_core::services::attachment::AttachmentService;
use chatter_core::services::channel::ChannelService;
use chatter_core::services::message::MessageService;
use chatter_core::services::user::UserService;

use crate::bin_ui::converters::{channel_type_to_string, messages_to_display_batch};
use chatter_core::services::conversation::ConversationHandle;
use crate::MainWindow;

use super::stop_audio_playback;

pub(crate) fn wire_channel_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx_clone = ui_ctx.clone();
    let chat_state = ui_ctx.chat_state().clone();
    let channel_service = ui_ctx.channels().clone();
    let message_service = ui_ctx.messages().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();
    let pin_service = ui_ctx.pins().clone();
    let pinned_state = ui_ctx.pinned_state().clone();

    let weak_window = window.as_weak();

    window.on_channel_selected(move |channel_id: SharedString| {
        let channel_id_str = channel_id.to_string();
        tracing::info!(
            channel_id = %channel_id_str,
            "[CHANNEL_SELECTED] on_channel_selected triggered"
        );
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_clone.clone();
        let chat_state = chat_state.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
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
                    // Clear composer reply/edit state to prevent stale context
                    w.set_composer_is_replying(false);
                    w.set_composer_reply_to_sender("".into());
                    w.set_composer_reply_to_sender_name("".into());
                    w.set_composer_reply_to_timestamp("".into());
                    w.set_composer_reply_preview("".into());
                    w.set_composer_reply_has_thumbnail(false);
                    w.set_composer_reply_thumbnail(slint::Image::default());
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                    // Clear pinned bar immediately while loading new channel's pins
                    w.set_has_pinned_messages(false);
                    w.set_pinned_message_preview("".into());
                    w.set_pinned_total_count(0);
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
            chat_state.set_active_conversation(chatter_core::state::ActiveConversation::Channel(channel_id));

            // Get current user ID from user service
            let current_user_id = user_service.current_user_id().ok().map(|id| id.to_string());
            tracing::info!(
                channel_id = %channel_id,
                current_user_id = ?current_user_id,
                "[CHANNEL_SELECTED] current_user_id for is_own check"
            );

            // Load pinned messages for this channel
            // All channels use encrypted group mailboxes, so messages are stored as AnonGroup
            let (msg_ids, preview) = match pin_service.load_pinned_for_channel(&channel_id, &ConversationType::AnonGroup).await {
                Ok(pinned) if !pinned.is_empty() => {
                    let ids = pinned.iter().map(|p| p.message_id).collect();
                    let preview = pinned[0].preview.clone();
                    tracing::info!(channel_id = %channel_id, count = pinned.len(), "[PINNED] Loaded pinned messages");
                    (ids, preview)
                }
                Ok(_) => {
                    tracing::debug!("[PINNED] No pinned messages for channel {}", channel_id);
                    (vec![], String::new())
                }
                Err(e) => {
                    tracing::warn!("[PINNED] Failed to load pinned messages: {}", e);
                    (vec![], String::new())
                }
            };
            super::apply_pinned_ui(&weak, &pinned_state, channel_id, msg_ids, preview);

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
                let handle = ConversationHandle::new(
                    chatter_core::state::ActiveConversation::Channel(channel_id),
                    ui_ctx.clone(),
                );
                tokio::spawn(async move {
                    if let Err(e) = handle.send_read_receipt(&message_timestamps).await {
                        tracing::warn!("Failed to send channel read receipts: {}", e);
                    }
                });
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

    // Use the channel's own conversation_type() which respects require_encryption
    let conversation_type = channel.conversation_type();

    // Fetch recent messages (limit 50)
    let messages = match message_service.get_messages_local_asc(channel_id, &conversation_type, None, 50) {
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
    let display_messages = messages_to_display_batch(
        &messages,
        |uid| user_service.get_display_name(uid),
        |att_id| attachment_service.get(att_id).ok().flatten(),
    );

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

            // For DMs, look up peer's presence from DM sidebar model
            let is_dm_type = channel_type == "dm";
            if is_dm_type {
                let conversations = w.get_dm_conversations();
                let cid = channel_id_for_lookup.to_string();
                for i in 0..conversations.row_count() {
                    if let Some(item) = conversations.row_data(i) {
                        if item.channel_id.as_str() == cid {
                            w.set_header_presence_status(item.presence_status.clone());
                            break;
                        }
                    }
                }
            } else {
                w.set_header_presence_status("offline".into());
            }

            w.set_channel_name(channel_name.into());
            w.set_channel_type(channel_type.into());
            w.set_is_dm(is_dm_type);
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

