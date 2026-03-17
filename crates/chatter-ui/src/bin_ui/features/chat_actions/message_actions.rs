//! Message action callbacks.
//!
//! Handles message click, reply, react, and thread actions.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use crate::bin_ui::features::chat::emoji_loader;
use crate::MainWindow;

/// Wire message click callback.
pub fn wire_message_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_message_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("Message clicked: {}", message_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::trace!("Message selected: {}", message_id_str);
            }
        });
    });
}

/// Wire reply click callback.
///
/// Gets sender name and preview text directly from the UI message model,
/// which works for all message types (channels, DMs, anon DMs, anon groups).
pub fn wire_reply_clicked(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window.on_reply_clicked(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_str = timestamp_ns.to_string();

        if sender_id_str.is_empty() || timestamp_str.is_empty() {
            tracing::error!("Reply clicked with empty values");
            return;
        }

        let Some(w) = weak_window.upgrade() else { return };

        // Get reply info directly from the UI message model (works for all message types)
        let messages = w.get_messages();
        let mut sender_name = String::new();
        let mut reply_preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(m) = messages.row_data(i) {
                if m.sender_id.as_str() == sender_id_str
                    && m.timestamp_ns.as_str() == timestamp_str
                {
                    sender_name = m.sender_name.to_string();
                    let content = m.content.to_string();
                    reply_preview = if content.chars().count() > 50 {
                        format!("{}...", content.chars().take(47).collect::<String>())
                    } else {
                        content
                    };
                    break;
                }
            }
        }

        if sender_name.is_empty() {
            sender_name = sender_id_str.clone();
        }

        w.set_composer_is_replying(true);
        w.set_composer_reply_to_sender(sender_id_str.clone().into());
        w.set_composer_reply_to_sender_name(sender_name.clone().into());
        w.set_composer_reply_to_timestamp(timestamp_str.clone().into());
        w.set_composer_reply_preview(reply_preview.clone().into());
        w.set_composer_placeholder("Type your reply...".into());

        tracing::info!(
            sender = %sender_name,
            timestamp = %timestamp_str,
            preview = %reply_preview,
            "Reply mode activated"
        );
    });
}

/// Wire react click callback.
pub fn wire_react_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_react_clicked(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_str = timestamp_ns.to_string();
        let weak = weak_window.clone();
        tracing::debug!(sender = %sender_id_str, timestamp = %timestamp_str, "React clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                tracing::info!(sender = %sender_id_str, timestamp = %timestamp_str, "Emoji picker requested");
                
                // Populate initial emojis (Smileys)
                let emojis = emoji_loader::get_emoji_list(0);
                w.set_reaction_picker_emojis(slint::ModelRc::new(slint::VecModel::from(emojis)));
            }
        });
    });

    let weak_category = window.as_weak();
    window.on_reaction_picker_category_changed(move |category: i32| {
        let weak = weak_category.clone();
        tracing::debug!("Reaction picker category changed: {}", category);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let emojis = emoji_loader::get_emoji_list(category);
                w.set_reaction_picker_emojis(slint::ModelRc::new(slint::VecModel::from(emojis)));
            }
        });
    });

    let weak_search = window.as_weak();
    window.on_reaction_picker_search_changed(move |query: SharedString| {
        let weak = weak_search.clone();
        let query_str = query.to_string().to_lowercase();
        tracing::debug!("Reaction picker emoji search: {}", query_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if query_str.is_empty() {
                    let emojis = emoji_loader::get_emoji_list(0);
                    w.set_reaction_picker_emojis(slint::ModelRc::new(slint::VecModel::from(emojis)));
                } else {
                    let emojis = emoji_loader::search_emojis(&query_str);
                    w.set_reaction_picker_emojis(slint::ModelRc::new(slint::VecModel::from(emojis)));
                }
            }
        });
    });
}

/// Wire cancel reply callback.
pub fn wire_cancel_reply(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_reply(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Clear reply state
                w.set_composer_is_replying(false);
                w.set_composer_reply_to_sender("".into());
                w.set_composer_reply_to_sender_name("".into());
                w.set_composer_reply_to_timestamp("".into());
                w.set_composer_reply_preview("".into());
                w.set_composer_placeholder("Type a message...".into());
                tracing::debug!("Cancelled reply mode");
            }
        });
    });
}

/// Wire thread click callback.
pub fn wire_thread_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let message_service = ui_ctx.messages().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();
    let channel_service = ui_ctx.channels().clone();

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        let channel_service = channel_service.clone();

        let msg_id: MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for thread: {}", e);
                return;
            }
        };

        let channel_id = match chat_state.get().active_channel_id {
            Some(id) => id,
            None => {
                tracing::warn!("Cannot open thread: no active channel");
                return;
            }
        };

        // Update state
        chat_state.open_thread(channel_id, msg_id);

        // Load thread data and update UI
        crate::bin_ui::features::thread::load_thread_data(
            weak,
            chat_state,
            message_service,
            user_service,
            attachment_service,
            channel_service,
            channel_id,
            msg_id,
        );
    });
}

/// Wire close image viewer callback.
pub fn wire_close_image_viewer(window: &MainWindow) {
    window.on_close_image_viewer(|| {
        // Stop any GIF animation when closing the viewer
        crate::bin_ui::gif_state::stop_gif_animation();
        tracing::debug!("Image viewer closed");
    });
}
