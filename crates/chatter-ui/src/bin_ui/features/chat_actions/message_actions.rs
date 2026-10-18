//! Message action callbacks.
//!
//! Handles message click, reply, react, and thread actions.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use crate::MainWindow;
use crate::bin_ui::features::chat::emoji_loader;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;

/// Wire message click callback: the only click a bubble reports is a tap on
/// its "not sent" label, which retries the send (Signal's "tap to retry").
/// A stored failure reason is a label, not a reason to ignore the tap.
pub fn wire_message_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_message_clicked(move |message_id: SharedString| {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let Some(sent_at) = message_id
            .parse::<MessageId>()
            .ok()
            .and_then(|id| ui_ctx.retryable_message_timestamp(&id))
        else {
            return;
        };

        crate::bin_ui::features::auth::inbox::domain_handler::set_own_status_at(
            &w,
            &[sent_at],
            "sending",
        );
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let message_id = message_id.to_string();
        tokio::spawn(async move {
            let slow_window = weak.clone();
            let retry = chatter_core::client::await_send_with_notice(
                ui_ctx.retry_failed_message(&message_id),
                std::time::Duration::from_secs(30),
                move || {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = slow_window.upgrade() else {
                            return;
                        };
                        w.set_toast_message(
                            "Delivery is taking longer. The original retry is still running."
                                .into(),
                        );
                        w.set_toast_variant("info".into());
                        w.set_toast_visible(true);
                    });
                },
            )
            .await;
            let (message, notice) = match retry {
                Ok(message) => (Some(message), None),
                Err(e) => {
                    let message = message_id
                        .parse::<MessageId>()
                        .ok()
                        .and_then(|id| ui_ctx.get_message_local(&id));
                    (message, Some(ui_ctx.format_send_error(&e, "send")))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let model = w.get_messages();
                for i in 0..model.row_count() {
                    let Some(mut row) = model.row_data(i) else {
                        continue;
                    };
                    if row.message_id.as_str() != message_id {
                        continue;
                    }
                    if let Some(message) = &message {
                        row.message_id = message.id.to_string().into();
                        row.timestamp_ns = message.sent_at.to_string().into();
                        row.delivery_status = if message.delivery_status.is_empty() {
                            "sent".into()
                        } else {
                            chatter_core::services::message::display_delivery_status(
                                &message.delivery_status,
                                message.failure_reason.as_deref(),
                            )
                            .into()
                        };
                        row.failure_reason =
                            message.failure_reason.clone().unwrap_or_default().into();
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

        let Some(w) = weak_window.upgrade() else {
            return;
        };

        // Get reply info directly from the UI message model (works for all message types)
        let messages = w.get_messages();
        let mut sender_name = String::new();
        let mut reply_preview = String::new();
        let mut reply_thumbnail = slint::Image::default();
        let mut reply_has_thumbnail = false;

        for i in 0..messages.row_count() {
            if let Some(m) = messages.row_data(i) {
                if m.sender_id.as_str() == sender_id_str && m.timestamp_ns.as_str() == timestamp_str
                {
                    sender_name = m.sender_name.to_string();

                    let attachments = m.attachments;
                    let mut preview_attachments = Vec::new();
                    if m.has_attachments {
                        for j in 0..attachments.row_count() {
                            if let Some(att) = attachments.row_data(j) {
                                if att.is_image && !reply_has_thumbnail && att.has_thumbnail {
                                    reply_thumbnail = att.thumbnail.clone();
                                    reply_has_thumbnail = true;
                                }
                                preview_attachments.push(chatter_core::client::preview::ActionPreviewAttachment {
                                    is_image: att.is_image,
                                    category: att.category.to_string(),
                                    filename: att.filename.to_string(),
                                });
                            }
                        }
                    }
                    reply_preview = chatter_core::client::preview::message_action_preview(
                        m.content.as_str(), &preview_attachments);

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

        crate::bin_ui::features::chat::send_message::show_reply_image(
            &w,
            chatter_core::client::ComposerReply {
                sender_id: sender_id_str,
                timestamp: timestamp_str,
                sender_name,
                preview: reply_preview,
                thumbnail: None,
            },
            reply_has_thumbnail.then_some(reply_thumbnail),
        );
        tracing::debug!(has_thumbnail = %reply_has_thumbnail, "Reply mode activated");
    });
}

/// Wire react click callback.
pub fn wire_react_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_react_clicked(move |sender_id: SharedString, timestamp_ns: SharedString, _x: f32, _y: f32| {
        let sender_id_str = sender_id.to_string();
        let timestamp_str = timestamp_ns.to_string();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        tracing::debug!(sender = %sender_id_str, timestamp = %timestamp_str, "React clicked");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                tracing::info!(sender = %sender_id_str, timestamp = %timestamp_str, "Emoji picker requested");

                // Recent when the account has history, otherwise Smileys.
                emoji_loader::show_reaction_picker_start(&w, &ui_ctx);
            }
        });
    });
    // The picker's category and search callbacks are `chat::emoji_loader`'s.
}

/// Wire cancel reply callback.
pub fn wire_cancel_reply(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_reply(move || {
        // Cancel belongs to the composer visible at the click, before a
        // subsequent navigation can park it or restore another draft.
        if let Some(w) = weak_window.upgrade() {
            crate::bin_ui::features::chat::send_message::clear_reply(&w);
            tracing::debug!("Cancelled reply mode");
        }
    });
}

/// Wire thread click callback.
pub fn wire_thread_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
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
        let channel_id = match ui_ctx.active_conversation() {
            chatter_core::state::ActiveConversation::Channel(id) => id,
            _ => {
                tracing::warn!("Cannot open thread: active conversation is not a channel");
                return;
            }
        };

        // Update state
        ui_ctx.open_thread(channel_id, msg_id);

        // Load thread data and update UI
        crate::bin_ui::features::thread::load_thread_data(weak, ui_ctx, msg_id);
    });
}

/// Wire close image viewer callback.
pub fn wire_close_image_viewer(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_close_image_viewer(move || {
        super::media_viewer::invalidate();
        if let Some(w) = weak.upgrade() {
            w.set_media_viewer_attachment_id("".into());
            crate::bin_ui::features::header::refresh_visible_media(
                &w,
                &ui_ctx,
                w.get_message_first_visible_row(),
                w.get_message_last_visible_row(),
            );
        }

        // Stop any GIF animation when closing the viewer
        crate::bin_ui::gif_state::stop_gif_animation();
        tracing::debug!("Image viewer closed");
    });
}
