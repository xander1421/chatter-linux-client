//! Channel selection and message loading.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::ChannelId;

use crate::{MainWindow, run_ui};
use crate::bin_ui::converters::messages_to_display_batch;

use super::stop_audio_playback;

/// Wire channel selected callback.
pub fn wire_channel_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx_clone = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_channel_selected(move |channel_id: SharedString| {
        let channel_id_str = channel_id.to_string();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_clone.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        let channel_id_str_for_ui = channel_id_str.clone();
        run_ui!(weak, |w| {
            w.set_loading_messages(true);

            // Clear unread count in sidebar
            crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &channel_id_str_for_ui, 0);

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
        });

        tokio::spawn(async move {
            // Parse channel ID
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid channel ID: {}", e);
                    return;
                }
            };

            // Update chat state
            ui_ctx.chat_state().set_active_channel(channel_id);
            ui_ctx.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::Channel(channel_id));
            let ui_ctx_for_sel = ui_ctx.clone();
            run_ui!(weak, |w| super::refresh_selection(&w, &ui_ctx_for_sel));

            // Mark as read
            let handle = ui_ctx.conversation(chatter_core::state::ActiveConversation::Channel(channel_id));
            let _ = handle.mark_as_read().await;

            // Load pinned messages for this channel from DB
            let (msg_ids, preview) = match ui_ctx.load_pinned_for_channel(&channel_id, &chatter_core::domain::message::ConversationType::Channel).await {
                Ok(pinned) if !pinned.is_empty() => {
                    let ids = pinned.iter().map(|p| p.message_id).collect();
                    let preview = pinned[0].preview.clone();
                    (ids, preview)
                }
                _ => (vec![], String::new()),
            };
            super::apply_pinned_ui(&weak, &ui_ctx.pinned_state(), channel_id, msg_ids, preview);

            load_channel_into_ui(
                weak,
                channel_id,
                ui_ctx.clone(),
            ).await;
        });
    });
}

/// Load a channel into the UI.
pub async fn load_channel_into_ui(
    weak: slint::Weak<MainWindow>,
    channel_id: ChannelId,
    ui_ctx: Arc<ChatterClient>,
) -> Vec<String> {
    // 1. Get channel metadata
    let channel = match ui_ctx.get_channel_local(&channel_id) {
        Some(c) => c,
        None => {
            tracing::error!("Channel {} not found locally", channel_id);
            run_ui!(weak, |w| w.set_loading_messages(false));
            return Vec::new();
        }
    };

    // 2. Load messages — use the channel's actual conversation type so
    // SavedMessages (stored as ConversationType::SavedMessages) round-trip correctly.
    let conv_type = channel.conversation_type();
    let messages = match ui_ctx.get_messages_local_asc(&channel_id, &conv_type, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to list channel messages: {}", e);
            run_ui!(weak, |w| w.set_loading_messages(false));
            return Vec::new();
        }
    };

    let message_timestamps: Vec<String> = messages.iter().map(|m| m.sent_at.to_string()).collect();

    // 3. Convert to display format
    let display_messages = messages_to_display_batch(
        &messages,
        |uid| ui_ctx.display_name(uid),
        |att_id| ui_ctx.get_attachment(att_id),
    );

    // 4. Update UI
    let is_saved = matches!(channel.channel_type, ChannelType::SavedMessages);
    let group_name = if is_saved { "Saved Messages".to_string() } else { channel.name.clone() };
    let topic = if is_saved { String::new() } else { channel.description.clone().unwrap_or_default() };
    let channel_type_str = match channel.channel_type {
        ChannelType::Public => "public",
        ChannelType::Private => "private",
        ChannelType::SavedMessages => "saved",
        _ => "public",
    };
    let member_count = channel.member_count as i32;
    let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
    let ui_ctx_gifs = ui_ctx.clone();

    // Channel icon (raw bytes stored on the Channel record). Decode on the
    // worker thread — `slint::Image` is `!Send`, so we carry the RGBA
    // across the thread-hop and build the `Image` inside `run_ui!`.
    let channel_icon_pixels = channel
        .icon
        .as_deref()
        .filter(|b| !b.is_empty())
        .and_then(|bytes| image::load_from_memory(bytes).ok())
        .map(|img| {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            (rgba.into_raw(), w, h)
        });

    run_ui!(weak, |w| {
        w.set_channel_name(group_name.into());
        w.set_channel_type(channel_type_str.into());
        w.set_is_dm(false);
        w.set_member_count(member_count);
        w.set_topic(topic.into());
        w.set_header_presence_status("offline".into());
        match channel_icon_pixels {
            Some((rgba, w_px, h_px)) => {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &rgba, w_px, h_px,
                );
                w.set_channel_avatar_source(slint::Image::from_rgba8(buffer));
                w.set_channel_has_avatar(true);
            }
            None => {
                w.set_channel_avatar_source(slint::Image::default());
                w.set_channel_has_avatar(false);
            }
        }

        let slint_messages: Vec<_> = display_messages.iter().map(|m| m.to_slint_with_user(&current_user_id)).collect();
        w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
        w.set_loading_messages(false);

        // Auto-register GIF animations for inline playback
        crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx_gifs);
    });

    message_timestamps
}
