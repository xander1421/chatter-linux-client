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

/// Tell the user a send did not happen.
///
/// Every early return in the send pipeline has already cleared the composer, the
/// reply context and the staged attachments — which is fine when the message went
/// out, and silent data loss when it did not. This is not optional politeness:
/// with the server unreachable, a failed send is the *expected* outcome, and it
/// has to be visible.
///
/// The message text itself comes from chatter-core (`format_send_error`); this
/// only carries it to the surface.
/// Say that a send is on the wire, or (with "") that it no longer is.
///
/// The composer is cleared the instant send is tapped and the message is not
/// appended until the round trip returns — a window of up to 10s + 30s per
/// attachment during which the app showed no evidence that anything had been
/// sent. This is the evidence. It is deliberately a status line rather than an
/// optimistic bubble: a bubble that might still fail is a claim we cannot make
/// yet, and this app has an explicit rule against saying a thing arrived before
/// it has.
fn set_send_status(weak: &slint::Weak<MainWindow>, status: &str) {
    let weak = weak.clone();
    let status = status.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_composer_send_status(status.into());
        }
    });
}

fn report_send_failure(weak: &slint::Weak<MainWindow>, message: String) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        w.set_composer_send_status("".into());
        w.set_toast_message(message.into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
        w.global::<crate::Haptics>().invoke_error();
    });
}

/// Show `ChatterClient::channel_send_notice` in place of the composer for
/// `channel_id`: the stored answer at once, then the server's.
pub fn refresh_send_notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        set_send_notice(&weak, &ui_ctx, channel_id, ui_ctx.channel_send_notice(&channel_id));
        let notice = ui_ctx.refresh_channel_send_notice(&channel_id).await;
        set_send_notice(&weak, &ui_ctx, channel_id, notice);
    });
}

/// Re-render the notice from the stored membership, e.g. after a live
/// `MemberUpdated` (mute, unmute, mute expiry) already refreshed it.
pub fn show_send_notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
) {
    set_send_notice(weak, ui_ctx, channel_id, ui_ctx.channel_send_notice(&channel_id));
}

/// Apply a notice only while its channel is still the one on screen.
fn set_send_notice(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: chatter_core::domain::ids::ChannelId,
    notice: String,
) {
    if ui_ctx.chat_state().get().active_channel_id != Some(channel_id) {
        return;
    }
    let read_only = ui_ctx.is_former_channel(&channel_id);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_composer_notice(notice.into());
            w.set_conversation_read_only(read_only);
        }
    });
}

/// Every conversation switch goes through `ChatState` (see the draft watcher
/// in `features::composer`): clear the notice on leaving, recompute it for a
/// channel on entering. Live mute changes arrive as `MemberUpdated`
/// ([`show_send_notice`]); the open re-reads the roster in case one was missed.
fn wire_send_notice(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut current = ui_ctx.chat_state().get().active_channel_id;
    let mut rx = ui_ctx.chat_state().subscribe();
    let weak = window.as_weak();
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        loop {
            let snapshot = match rx.recv().await {
                Ok(snap) => snap,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if snapshot.active_channel_id == current {
                continue;
            }
            current = snapshot.active_channel_id;
            let weak_clear = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_clear.upgrade() {
                    w.set_composer_notice("".into());
                    w.set_conversation_read_only(false);
                }
            });
            if let Some(channel_id) = current {
                refresh_send_notice(&weak, &ctx, channel_id);
            }
        }
    });
}

pub fn wire_send(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_send_notice(window, ui_ctx);
    let chat_state = ui_ctx.chat_state().clone();
    let auth_state = ui_ctx.auth_state().clone();
    let ui_ctx_jarvis = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_send(move |text: slint::SharedString| {
        let send_started = std::time::Instant::now();
        let text = text.to_string();
        let pending_attachments = chat_state.get_pending_attachments();
        let has_attachments = !pending_attachments.is_empty();

        if text.trim().is_empty() && !has_attachments { return; }

        // Mesh conversation open: serverless, so it must not reach the gateway
        // pipeline — or the Jarvis interceptions below, which would answer a
        // slash command instead of broadcasting it. Core picks the transport
        // (public room / geohash cell / peer).
        #[cfg(feature = "mesh")]
        if crate::bin_ui::features::mesh::intercept_send(&ui_ctx_jarvis, &text, weak_window.clone()) {
            return;
        }

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

        // @Jarvis request? Core decides: a whole-word mention in a
        // conversation where AI is on. Anything else is sent as written.
        #[cfg(feature = "jarvis")]
        if let Some((context, query)) =
            ui_ctx_jarvis.jarvis_mention(&chat_state.active_conversation(), &text)
        {
            let ui_ctx = ui_ctx_jarvis.clone();
            let weak = weak_window.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::jarvis::handle_jarvis_mention(&ui_ctx, &query, context, weak).await;
            });
            return;
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
            let send_timeout = if matches!(conversation, ActiveConversation::Channel(_)) {
                std::time::Duration::from_secs(35 + 30 * pending_attachments.len() as u64)
            } else if pending_attachments.is_empty() {
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT
            } else {
                std::time::Duration::from_secs(10 + 30 * pending_attachments.len() as u64)
            };

            // Attachments are the case that hurts — they carry the long timeout
            // and the big upload — so they get counted. A plain text send is
            // usually sub-second, but it is the same black hole when the server
            // is slow, so it says so too.
            set_send_status(
                &weak,
                &match pending_attachments.len() {
                    0 => "Sending\u{2026}".to_string(),
                    1 => "Sending 1 attachment\u{2026}".to_string(),
                    n => format!("Sending {n} attachments\u{2026}"),
                },
            );

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
                    // `format_send_error` exists to produce exactly this string
                    // and its return value used to be dropped on the floor, so a
                    // send against a dead server cleared the composer and
                    // reported nothing at all.
                    report_send_failure(&weak, ui_ctx.format_send_error(&e, "send"));
                    // A refusal means we are muted or were removed, and no
                    // event told us: re-read the roster and swap the composer
                    // for the notice.
                    if e.is_refused() {
                        if let ActiveConversation::Channel(channel_id) = conversation {
                            refresh_send_notice(&weak, &ui_ctx, channel_id);
                        }
                    }
                    return;
                }
                Err(_) => {
                    tracing::error!(
                        timeout_s = send_timeout.as_secs(),
                        "Message send timed out"
                    );
                    report_send_failure(
                        &weak,
                        "The server did not answer, so the message was not sent.".to_string(),
                    );
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
            let reply_thumbnail_rgba = ui_result.reply_thumbnail_bytes.as_ref()
                .and_then(|bytes| image::load_from_memory(bytes).ok())
                .map(|img| img.to_rgba8());
            let reply_media = ui_result.reply_parent_media.clone();
            let msg_id = ui_result.message_id_effective.clone();
            let delivery_status = ui_result.delivery_status;
            let bootstrap_initiated = ui_result.bootstrap_initiated;
            let sent_at = ui_result.sent_at;
            let failure_notice = ui_result.failure_notice.clone();
            let failure_reason = ui_result.failure_reason.clone().unwrap_or_default();

            tracing::info!(msg_id = %msg_id, "[UI_SEND] scheduling invoke_from_event_loop for append");
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else {
                    tracing::error!("[UI_SEND] weak.upgrade() returned None; own bubble dropped");
                    return;
                };
                // The bubble is about to be appended, so the status line has
                // done its job. Cleared here rather than earlier so the two
                // never both go missing for a frame.
                w.set_composer_send_status("".into());
                {
                    let slint_attachments: Vec<_> = attachment_display.iter().map(|a| a.to_slint()).collect();
                    let (reply_thumbnail, reply_has_thumbnail, reply_thumb_w, reply_thumb_h) = reply_thumbnail_rgba
                        .as_ref()
                        .map(|rgba| {
                            let (w, h) = rgba.dimensions();
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                rgba.as_raw(), w, h,
                            );
                            (slint::Image::from_rgba8(buf), true, w as i32, h as i32)
                        })
                        .unwrap_or_else(|| (slint::Image::default(), false, 0, 0));
                    let mut msg_item = super::helpers::build_own_message_item(
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
                    msg_item.failure_reason = failure_reason.into();
                    let draft = w.get_composer_text();
                    super::helpers::append_message_to_ui(&w, msg_item);
                    tracing::debug!(elapsed_ms = send_started.elapsed().as_millis(), "send: bubble appended");
                    w.set_composer_text(draft);
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

                    // Stored but not delivered: the bubble says "not sent"
                    // (tap to retry); the reason is told once, here.
                    if let Some(notice) = failure_notice {
                        w.set_toast_message(notice.into());
                        w.set_toast_variant("error".into());
                        w.set_toast_visible(true);
                        w.global::<crate::Haptics>().invoke_error();
                    }

                    crate::bin_ui::features::sidebar::update_sidebar_preview(
                        &w, &ui_ctx_for_sidebar, conversation, &sidebar_preview, &timestamp_str,
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
