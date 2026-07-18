//! Send message callback for channels and DMs.
//!
//! All conversation types follow the same pipeline:
//! 1. Auth check + user info
//! 2. Upload attachments (all types pre-upload)
//! 3. Build quote context
//! 4. Build OutgoingIntent per conversation type
//! 5. Route through gateway
//! 6. Handle result + update UI

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::gateway;
use chatter_core::state::ActiveConversation;

use crate::bin_ui::converters::attachment_to_display_data;
use super::typing::clear_outgoing_typing;
use crate::{MainWindow, PendingAttachmentData};

pub fn wire_send(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let chat_state = ui_ctx.chat_state().clone();
    let auth_state = ui_ctx.auth_state().clone();
    let ui_ctx_jarvis = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_send(move |text: slint::SharedString| {
        let text = text.to_string();
        let pending_attachments = chat_state.get_pending_attachments();
        let has_attachments = !pending_attachments.is_empty();

        if text.trim().is_empty() && !has_attachments { return; }

        // Local AI channel: every send routes to Jarvis, nothing networks.
        #[cfg(feature = "jarvis")]
        if let Some(id) = chat_state.get().active_channel_id {
            if chatter_core::client::is_jarvis_channel(&id) {
                if text.trim().is_empty() { return; }
                let ui_ctx = ui_ctx_jarvis.clone();
                let weak = weak_window.clone();
                tokio::spawn(async move {
                    crate::bin_ui::features::jarvis::handle_jarvis_channel_send(ui_ctx, text, weak).await;
                });
                return;
            }
        }

        // Check for Jarvis slash commands first (/ask, /summarize, /knowledge, /remember)
        #[cfg(feature = "jarvis")]
        if let Some(cmd) = crate::bin_ui::features::jarvis::parse_slash_command(&text) {
            let ui_ctx = ui_ctx_jarvis.clone();
            let channel_id = chat_state.get().active_channel_id;
            let weak = weak_window.clone();
            tokio::spawn(async move {
                handle_jarvis_slash_command(&ui_ctx, cmd, channel_id.as_ref(), weak).await;
            });
            return;
        }

        // Check for @Jarvis mention - intercept before sending to channel
        #[cfg(feature = "jarvis")]
        if crate::bin_ui::features::jarvis::contains_jarvis_mention(&text) {
            if let Some(query) = crate::bin_ui::features::jarvis::extract_jarvis_query(&text) {
                let ui_ctx = ui_ctx_jarvis.clone();
                let channel_id = chat_state.get().active_channel_id;
                let weak = weak_window.clone();
                tokio::spawn(async move {
                    crate::bin_ui::features::jarvis::handle_jarvis_mention(
                        &ui_ctx,
                        &query,
                        channel_id.as_ref(),
                        weak,
                    ).await;
                });
                return;
            }
        }

        // Clear outgoing typing state (Signal: clearTypingTimers on message send)
        clear_outgoing_typing(&ui_ctx_jarvis);
        if has_attachments { chat_state.clear_pending_attachments(); }

        // Track user activity for presence updates
        {
            let ui_ctx = ui_ctx_jarvis.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::auth::presence_integration::track_activity(&ui_ctx).await;
            });
        }

        let auth_state = auth_state.clone();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_jarvis.clone();

        // Determine active conversation from chat state
        let conversation = chat_state.active_conversation();
        if matches!(conversation, ActiveConversation::None) { return; }

        // Get reply state from window
        let (reply_to_sender, reply_to_timestamp, reply_sender_name, reply_preview_text) = weak.upgrade().map(|w| {
            let is_replying = w.get_composer_is_replying();
            let reply_sender = if is_replying { w.get_composer_reply_to_sender().to_string() } else { String::new() };
            let reply_ts = if is_replying { w.get_composer_reply_to_timestamp().to_string() } else { String::new() };
            let reply_name = if is_replying { w.get_composer_reply_to_sender_name().to_string() } else { String::new() };
            let reply_preview = if is_replying { w.get_composer_reply_preview().to_string() } else { String::new() };
            (reply_sender, reply_ts, reply_name, reply_preview)
        }).unwrap_or_default();

        // Clear reply state immediately after capturing
        let is_reply = !reply_to_sender.is_empty() && !reply_to_timestamp.is_empty();
        if is_reply {
            let weak_clear_reply = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear_reply.upgrade() {
                    w.set_composer_is_replying(false);
                    w.set_composer_reply_to_sender("".into());
                    w.set_composer_reply_to_sender_name("".into());
                    w.set_composer_reply_to_timestamp("".into());
                    w.set_composer_reply_preview("".into());
                    w.set_composer_reply_has_thumbnail(false);
                    w.set_composer_reply_thumbnail(slint::Image::default());
                    w.set_composer_placeholder("Type a message...".into());
                }
            });
        }

        if has_attachments {
            let weak_clear = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear.upgrade() {
                    w.set_pending_attachments(ModelRc::new(VecModel::from(Vec::<PendingAttachmentData>::new())));
                    w.set_composer_placeholder("Type a message...".into());
                }
            });
        }

        // ================================================================
        // Send pipeline — core orchestrates, UI renders the result.
        // ================================================================
        tokio::spawn(async move {
            if !auth_state.is_logged_in() { return; }
            let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();
            let current_user_name = auth_state.get().display_name.unwrap_or_else(|| "Me".to_string());

            // Quote reference — preview text is filled in by core.
            let quote = if is_reply {
                let reply = match super::helpers::parse_reply_context(&reply_to_sender, &reply_to_timestamp) {
                    Some(r) => r,
                    None => return,
                };
                Some(gateway::QuoteIntent {
                    target_author_id: reply.sender_id,
                    target_timestamp: reply.timestamp,
                    target_preview: None,
                })
            } else {
                None
            };

            // Scale timeout for attachments: base 10s + 30s per attachment.
            let send_timeout = if pending_attachments.is_empty() {
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT
            } else {
                std::time::Duration::from_secs(10 + 30 * pending_attachments.len() as u64)
            };

            tracing::info!(?conversation, text_len = text.len(), "[UI_SEND] entering send_message_ui");
            let ui_result = match tokio::time::timeout(
                send_timeout,
                ui_ctx.send_message_ui(conversation, &text, &pending_attachments, quote, None),
            ).await {
                Ok(Ok(r)) => {
                    tracing::info!(msg_id = %r.message_id_effective, sent_at = r.sent_at, "[UI_SEND] send_message_ui returned Ok");
                    r
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "[UI_SEND] send_message_ui returned Err");
                    ui_ctx.format_send_error(&e, "send");
                    return;
                }
                Err(_) => {
                    tracing::error!("Message send timed out");
                    return;
                }
            };

            // Decode attachment thumbnails off the UI thread.
            let mut attachment_display: Vec<_> = ui_result
                .attachments
                .iter()
                .map(attachment_to_display_data)
                .collect();
            for att in &mut attachment_display {
                att.decode_thumbnail();
            }

            let ui_ctx_for_gifs = ui_ctx.clone();
            let ui_ctx_for_sidebar = ui_ctx.clone();
            let sent_text = text.clone();
            let timestamp_str = ui_result.timestamp_display.clone();
            let sidebar_preview = ui_result.sidebar_preview.clone();
            let reply_thumbnail_bytes = ui_result.reply_thumbnail_bytes.clone();
            let reply_media = ui_result.reply_parent_media.clone();
            let msg_id = ui_result.message_id_effective.clone();
            let delivery_status = ui_result.delivery_status;
            let bootstrap_initiated = ui_result.bootstrap_initiated;
            let sent_at = ui_result.sent_at;

            tracing::info!(msg_id = %msg_id, "[UI_SEND] scheduling invoke_from_event_loop for append");
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else {
                    tracing::error!("[UI_SEND] weak.upgrade() returned None; own bubble dropped");
                    return;
                };
                {
                    let slint_attachments: Vec<_> = attachment_display.iter().map(|a| a.to_slint()).collect();
                    let (reply_thumbnail, reply_has_thumbnail, reply_thumb_w, reply_thumb_h) = reply_thumbnail_bytes
                        .as_ref()
                        .and_then(|bytes| image::load_from_memory(bytes).ok())
                        .map(|img| {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                rgba.as_raw(), w, h,
                            );
                            (slint::Image::from_rgba8(buf), true, w as i32, h as i32)
                        })
                        .unwrap_or_else(|| (slint::Image::default(), false, 0, 0));
                    let msg_item = super::helpers::build_own_message_item(
                        &msg_id, &current_user_id, &current_user_name, &sent_text,
                        &timestamp_str, sent_at, slint_attachments,
                        is_reply, &reply_sender_name, &reply_preview_text,
                        reply_thumbnail, reply_has_thumbnail, reply_thumb_w, reply_thumb_h,
                        &reply_media,
                        delivery_status,
                    );
                    {
                        use slint::Model;
                        tracing::info!(
                            msg_id = %msg_id,
                            existing = w.get_messages().row_count(),
                            "[UI_SEND] calling append_message_to_ui"
                        );
                    }
                    super::helpers::append_message_to_ui(&w, msg_item);
                    {
                        use slint::Model;
                        tracing::info!(
                            after = w.get_messages().row_count(),
                            "[UI_SEND] append_message_to_ui done"
                        );
                    }

                    if bootstrap_initiated {
                        w.set_composer_placeholder("Setting up secure channel...".into());
                    }

                    crate::bin_ui::features::sidebar::update_sidebar_preview(
                        &w, &ui_ctx_for_sidebar, conversation, &sidebar_preview, &timestamp_str, false,
                    );

                    crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                        w.as_weak(),
                        ui_ctx_for_gifs,
                    );
                }
            });
        });
    });
}

/// Handle Jarvis slash commands (/ask, /summarize, /knowledge, /remember).
///
/// Dispatch + state recording lives in `ChatterClient::jarvis_execute`.
/// This function only manages the UI thinking-placeholder lifecycle.
#[cfg(feature = "jarvis")]
async fn handle_jarvis_slash_command(
    ui_ctx: &Arc<ChatterClient>,
    cmd: chatter_core::services::jarvis::JarvisCommand,
    channel_id: Option<&chatter_core::domain::ids::ChannelId>,
    weak_window: slint::Weak<MainWindow>,
) {
    use chatter_core::domain::jarvis::JarvisContext;

    let context = match channel_id {
        Some(id) => JarvisContext::channel(id.clone()),
        None => {
            let placeholder_id: chatter_core::domain::ids::ChannelId = uuid::Uuid::nil().into();
            JarvisContext::dm(placeholder_id)
        }
    };
    let author_id = ui_ctx.auth_state().get().current_user_id;

    // Surface the thinking placeholder before kicking off the core call.
    ui_ctx.jarvis_state().set_thinking(true);
    let thinking_id = {
        let weak = weak_window.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_thinking(true);
                let id = crate::bin_ui::features::jarvis::add_jarvis_thinking_placeholder(&w);
                let _ = tx.send(id);
            }
        });
        rx.recv().ok()
    };

    let (query_display, result) = ui_ctx
        .jarvis_execute(cmd, &context, author_id.as_ref())
        .await;
    let response_text = match &result {
        Ok(response) => response.clone(),
        Err(e) => format!("Error: {}", e),
    };

    let jarvis_state = ui_ctx.jarvis_state().clone();
    let _ = slint::invoke_from_event_loop(move || {
        jarvis_state.set_thinking(false);
        if let Some(w) = weak_window.upgrade() {
            w.set_jarvis_thinking(false);
            if let Some(tid) = thinking_id {
                crate::bin_ui::features::jarvis::replace_thinking_with_response(
                    &w, &tid, &query_display, &response_text,
                );
            }
        }
    });
}
