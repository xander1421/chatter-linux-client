//! Message action callbacks.
//!
//! Handles message click, reply, react, and thread actions.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use crate::bin_ui::features::chat::emoji_loader;
use crate::MainWindow;

/// Wire message click callback: the only click a bubble reports is a tap on
/// its "not sent" label, which retries the send (Signal's "tap to retry").
/// A stored failure reason is a label, not a reason to ignore the tap.
pub fn wire_message_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_message_clicked(move |message_id: SharedString| {
        let Some(w) = weak_window.upgrade() else { return };
        let Some(sent_at) = own_failed_timestamp(&w, &message_id) else { return };

        crate::bin_ui::features::auth::inbox::domain_handler::set_own_status_at(&w, &[sent_at], "sending");
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let message_id = message_id.to_string();
        tokio::spawn(async move {
            let (message, notice) = match ui_ctx.retry_failed_message(&message_id).await {
                Ok(message) => (Some(message), None),
                Err(e) => {
                    let message = message_id.parse::<MessageId>().ok()
                        .and_then(|id| ui_ctx.get_message_local(&id));
                    (message, Some(ui_ctx.format_send_error(&e, "send")))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let model = w.get_messages();
                for i in 0..model.row_count() {
                    let Some(mut row) = model.row_data(i) else { continue };
                    if row.message_id.as_str() != message_id { continue; }
                    if let Some(message) = &message {
                        row.message_id = message.id.to_string().into();
                        row.timestamp_ns = message.sent_at.to_string().into();
                        row.delivery_status = if message.delivery_status.is_empty() { "sent".into() }
                            else { message.delivery_status.clone().into() };
                        row.failure_reason = message.failure_reason.clone().unwrap_or_default().into();
                    } else {
                        row.delivery_status = "failed".into();
                    }
                    model.set_row_data(i, row);
                    break;
                }
                if let Some(notice) = notice {
                    w.set_toast_message(notice.into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                    w.global::<crate::Haptics>().invoke_error();
                }
            });
        });
    });
}

/// `sent_at` of our own bubble `message_id` when it says "not sent".
fn own_failed_timestamp(w: &MainWindow, message_id: &str) -> Option<u64> {
    let messages = w.get_messages();
    (0..messages.row_count())
        .filter_map(|i| messages.row_data(i))
        .find(|m| m.message_id.as_str() == message_id)
        .filter(|m| m.is_own && m.delivery_status == "failed")
        .and_then(|m| m.timestamp_ns.parse().ok())
}

/// Wire quote/reply-bubble click callback: jump to the quoted parent.
pub fn wire_quote_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_quote_clicked(move |parent_id: SharedString| {
        if parent_id.is_empty() {
            return;
        }
        if let Some(w) = weak_window.upgrade() {
            crate::bin_ui::features::header::jump_to_message(&w, &ui_ctx, parent_id.as_str());
        }
    });
}

/// Wire reply click callback.
///
/// Gets sender name and preview text directly from the UI message model,
/// which works for all message types (channels, DMs, anon DMs, anon groups).
pub fn wire_reply_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

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
        let mut reply_thumbnail = slint::Image::default();
        let mut reply_has_thumbnail = false;

        for i in 0..messages.row_count() {
            if let Some(m) = messages.row_data(i) {
                if m.sender_id.as_str() == sender_id_str
                    && m.timestamp_ns.as_str() == timestamp_str
                {
                    sender_name = m.sender_name.to_string();
                    let content = m.content.to_string();

                    // Build text preview (truncate if needed)
                    let text_preview = if content.chars().count() > 50 {
                        format!("{}...", content.chars().take(47).collect::<String>())
                    } else {
                        content.clone()
                    };

                    // Build attachment indicator and extract thumbnail
                    let mut attachment_label = String::new();
                    if m.has_attachments {
                        let att_model = m.attachments;
                        let att_count = att_model.row_count();
                        let mut image_count = 0;
                        // (emoji, filename) so audio/document/code/archive keep their own icon
                        let mut file_entries: Vec<(&'static str, String)> = Vec::new();

                        for j in 0..att_count {
                            if let Some(att) = att_model.row_data(j) {
                                if att.is_image {
                                    image_count += 1;
                                    // Grab the first image thumbnail for the composer
                                    if !reply_has_thumbnail && att.has_thumbnail {
                                        reply_thumbnail = att.thumbnail;
                                        reply_has_thumbnail = true;
                                    }
                                } else {
                                    let emoji = match att.category.as_str() {
                                        "audio" => "\u{1F3B5}",
                                        "video" => "\u{1F3AC}",
                                        "document" => "\u{1F4C4}",
                                        "code" => "\u{1F4DD}",
                                        "archive" => "\u{1F4E6}",
                                        _ => "\u{1F4CE}",
                                    };
                                    file_entries.push((emoji, att.filename.to_string()));
                                }
                            }
                        }

                        // Build descriptive label
                        let mut parts: Vec<String> = Vec::new();
                        if image_count == 1 {
                            parts.push("\u{1F4F7} Photo".to_string());
                        } else if image_count > 1 {
                            parts.push(format!("\u{1F4F7} {} Photos", image_count));
                        }
                        if file_entries.len() == 1 {
                            let (emoji, name) = &file_entries[0];
                            parts.push(format!("{} {}", emoji, name));
                        } else if file_entries.len() > 1 {
                            parts.push(format!("\u{1F4CE} {} files", file_entries.len()));
                        }
                        attachment_label = parts.join(", ");
                    }

                    // Combine text + attachment label
                    reply_preview = match (text_preview.is_empty(), attachment_label.is_empty()) {
                        (false, false) => format!("{} \u{2022} {}", text_preview, attachment_label),
                        (true, false) => attachment_label,
                        (false, true) => text_preview,
                        (true, true) => String::new(),
                    };

                    break;
                }
            }
        }

        if sender_name.is_empty() {
            sender_name = sender_id_str.clone();
        }
        if let Ok(author) = sender_id_str.parse::<chatter_core::domain::UserId>() {
            sender_name = ui_ctx.quote_author_name(&author, || sender_name);
        }

        w.set_composer_is_replying(true);
        w.set_composer_reply_to_sender(sender_id_str.clone().into());
        w.set_composer_reply_to_sender_name(sender_name.clone().into());
        w.set_composer_reply_to_timestamp(timestamp_str.clone().into());
        w.set_composer_reply_preview(reply_preview.clone().into());
        w.set_composer_reply_has_thumbnail(reply_has_thumbnail);
        w.set_composer_reply_thumbnail(reply_thumbnail);
        w.set_composer_placeholder("Type your reply...".into());

        tracing::info!(
            sender = %sender_name,
            timestamp = %timestamp_str,
            preview = %reply_preview,
            has_thumbnail = %reply_has_thumbnail,
            "Reply mode activated"
        );
    });
}

/// Wire react click callback.
pub fn wire_react_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_react_clicked(move |sender_id: SharedString, timestamp_ns: SharedString, _x: f32, _y: f32| {
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
                w.set_composer_reply_has_thumbnail(false);
                w.set_composer_reply_thumbnail(slint::Image::default());
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
    let ui_ctx = ui_ctx.clone();

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let ui_ctx = ui_ctx.clone();

        let msg_id: MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for thread: {}", e);
                return;
            }
        };

        // Threads are a channel feature (Signal has none in 1:1 chats). The
        // thread send path is the channel send; anywhere else a reply was
        // accepted by the panel and then lost.
        let channel_id = match chat_state.active_conversation() {
            chatter_core::state::ActiveConversation::Channel(id) => id,
            _ => {
                tracing::warn!("Cannot open thread: active conversation is not a channel");
                return;
            }
        };

        // Update state
        chat_state.open_thread(channel_id, msg_id);

        // Load thread data and update UI
        crate::bin_ui::features::thread::load_thread_data(
            weak,
            chat_state,
            ui_ctx,
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
