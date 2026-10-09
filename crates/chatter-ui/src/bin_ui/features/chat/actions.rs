//! Message action callbacks (reply, forward, copy).
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::UserId;

use crate::{ForwardTarget, MainWindow, run_ui};

/// Wire reply-message callback from context menu.
/// Gets sender name and preview from the UI message model directly.
pub fn wire_reply_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_reply_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();

        // Synchronous, like `reply_clicked`: the rows and composer read here
        // are the ones the menu was opened over. A queued callback could run
        // after a navigation or sign-out and target another conversation.
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        // Get reply info directly from UI message model (works for all message types)
        let messages = w.get_messages();
        let mut display_name = String::new();
        let mut preview_text = String::new();
        for i in 0..messages.row_count() {
            if let Some(m) = messages.row_data(i) {
                if m.sender_id.as_str() == sender_id_str
                    && m.timestamp_ns.as_str() == timestamp_ns_str
                {
                    display_name = m.sender_name.to_string();
                    let content = m.content.to_string();
                    preview_text = if content.chars().count() > 50 {
                        format!("{}...", content.chars().take(47).collect::<String>())
                    } else {
                        content
                    };
                    break;
                }
            }
        }
        if display_name.is_empty() {
            display_name = sender_id_str.clone();
        }
        if let Ok(author) = sender_id_str.parse::<UserId>() {
            display_name = ui_ctx.quote_author_name(&author, || display_name);
        }

        super::send_message::show_reply(
            &w,
            chatter_core::client::ComposerReply {
                sender_id: sender_id_str,
                timestamp: timestamp_ns_str,
                sender_name: display_name,
                preview: preview_text,
                thumbnail: None,
            },
        );
        tracing::debug!("Reply mode activated (context menu)");
    });
}

/// Wire forward-message-action callback from context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_forward_message_action(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_forward_message_action(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        let conversation = ui_ctx.chat_state().active_conversation();

        // Move service lookups to background task
        tokio::spawn(async move {
            // Parse sender_id for Signal-style lookup
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for forward");
                    return;
                }
            };

            // Parse timestamp for Signal-style lookup
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for forward");
                    return;
                }
            };

            // Keep the existing text preview; empty bodies need an attachment
            // summary from the same captured conversation, author and clock.
            let handle = ui_ctx.conversation(conversation);
            let text = handle.get_message_text(&target_sender_id, target_sent_timestamp)
                .ok()
                .flatten();
            let preview = super::forward_preview::message_preview(
                text.as_deref(),
                |text| ui_ctx.truncate_message_preview(text),
                || handle
                    .get_message_attachment_ids(&target_sender_id, target_sent_timestamp)
                    .unwrap_or_default()
                    .iter()
                    // Keep unresolved attachments in the count as generic files.
                    .map(|id| ui_ctx.get_attachment(id).map(|a| a.metadata))
                    .collect::<Vec<_>>(),
            );

            run_ui!(weak, |w| {
                open_forward_modal(
                    &w,
                    &ui_ctx,
                    vec![format!("{sender_id_str}:{timestamp_ns_str}")],
                    preview,
                );
            });
        });
    });
}

/// Open the forward picker for `sources` ("sender_id:timestamp_ns" keys, in
/// the order they are to arrive). Stashes the sources for the confirm handler
/// (the modal blocks conversation switches, but confirm runs later).
pub(super) fn open_forward_modal(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    sources: Vec<String>,
    preview: String,
) {
    // The destination list is core's — which conversations exist, which the
    // current mode may show, and Saved Messages. The UI renders it.
    let targets = forward_targets(ui_ctx, "");
    let sources: Vec<SharedString> = sources.into_iter().map(Into::into).collect();
    w.set_forward_message_preview(preview.into());
    w.set_forward_sources(ModelRc::from(std::rc::Rc::new(VecModel::from(sources))));
    w.set_forward_search_text("".into());
    w.set_forward_targets(ModelRc::from(std::rc::Rc::new(VecModel::from(targets))));
    crate::bin_ui::features::back::push(&w, "forward-message");
}

/// Wire the forward picker's search box. The filter itself is core's — this
/// re-asks for the destination list narrowed by what was typed.
pub fn wire_forward_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_forward_search_changed(move |query: SharedString| {
        // Synchronous, core-ranked — same shape as the unified search palette's
        // conversation list. A local read per keystroke over the sidebar's
        // conversations, not a network call.
        if let Some(w) = weak_window.upgrade() {
            let targets = forward_targets(&ui_ctx, query.as_str());
            w.set_forward_targets(ModelRc::from(std::rc::Rc::new(VecModel::from(targets))));
        }
    });
}

/// Core's forward destinations in the row shape the picker renders.
fn forward_targets(ui_ctx: &Arc<ChatterClient>, query: &str) -> Vec<ForwardTarget> {
    ui_ctx
        .forward_destinations(query)
        .into_iter()
        .map(|d| ForwardTarget {
            avatar_source: crate::bin_ui::features::modals::target_avatar(ui_ctx, &d.kind, &d.id),
            is_anonymous: matches!(
                d.kind,
                chatter_core::domain::message::ConversationType::AnonDm
                    | chatter_core::domain::message::ConversationType::AnonGroup
            ),
            id: d.id.into(),
            name: d.name.into(),
            target_type: d.kind.as_str().into(),
        })
        .collect()
}

/// Wire copy-message callback to copy message text to clipboard.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_copy_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_copy_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();

        let conversation = ui_ctx.chat_state().active_conversation();

        // Move service lookups and clipboard to background thread
        std::thread::spawn(move || {
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for copy");
                    return;
                }
            };
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for copy");
                    return;
                }
            };

            // Lookup message text via ConversationHandle (polymorphic across all types)
            let handle = ui_ctx.conversation(conversation);
            let text = match handle.get_message_text(&target_sender_id, target_sent_timestamp) {
                Ok(Some(t)) => t,
                Ok(None) => {
                    tracing::debug!("No text content to copy");
                    return;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to look up message for copy");
                    return;
                }
            };

            copy_text(&text);
        });
    });
}

/// Put `text` on the system clipboard (Android via JNI, desktop via arboard).
/// Blocking: call off the UI thread.
pub fn copy_text(text: &str) {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    match arboard::Clipboard::new() {
        Ok(mut clipboard) => {
            if let Err(e) = clipboard.set_text(text) {
                tracing::error!(error = %e, "Failed to copy to clipboard");
            }
        }
        Err(e) => tracing::error!(error = %e, "Failed to access clipboard"),
    }
    #[cfg(target_os = "android")]
    if let Err(e) = crate::platform::android::clipboard::set_text("Message", text) {
        tracing::error!(error = %e, "Failed to copy to clipboard");
    }
    #[cfg(target_os = "ios")]
    tracing::warn!("Clipboard not yet supported on iOS");
}

/// Wire download-message callback from context menu.
/// Downloads all attachments from a message to the user's Downloads folder.
pub fn wire_download_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_download_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();

        let conversation = ui_ctx.chat_state().active_conversation();
        let weak = weak.clone();
        let revision = ui_ctx.auth_state().get().session_revision;

        std::thread::spawn(move || {
            // Parse sender_id for Signal-style lookup
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for download");
                    return;
                }
            };

            // Parse timestamp for Signal-style lookup
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for download");
                    return;
                }
            };

            // Look up attachment IDs via ConversationHandle (polymorphic across all types)
            let handle = ui_ctx.conversation(conversation);
            let attachment_ids = match handle.get_message_attachment_ids(&target_sender_id, target_sent_timestamp) {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to look up message for download");
                    return;
                }
            };

            if attachment_ids.is_empty() {
                tracing::debug!("No attachments to download");
                return;
            }

            for att_id in attachment_ids {
                let attachment = match ui_ctx.get_attachment(&att_id) {
                    Some(att) => att,
                    None => {
                        tracing::warn!("Attachment {} not found", att_id);
                        continue;
                    }
                };

                #[cfg(target_os = "android")]
                {
                    let client = ui_ctx.clone();
                    let weak = weak.clone();
                    ui_ctx.runtime_handle().spawn(async move {
                        let result = async {
                            if client.auth_state().get().session_revision != revision {
                                return Err("Session changed".to_string());
                            }
                            if attachment.content_location == chatter_core::domain::attachment::ContentLocation::NotDownloaded {
                                client.download_attachment_to_filesystem(&att_id).await
                                    .map_err(|_| "Unable to download attachment".to_string())?;
                            }
                            crate::platform::android::media_export::save_attachment(&client, &att_id, revision)
                        }.await;
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            if client.auth_state().get().session_revision != revision { return; }
                            let (message, variant) = match result {
                                Ok(()) => ("Saved to Chatter downloads".to_string(), "success"),
                                Err(error) => (error, "error"),
                            };
                            w.set_toast_message(message.into());
                            w.set_toast_variant(variant.into());
                            w.set_toast_visible(true);
                        });
                    });
                    continue;
                }

                #[cfg(not(target_os = "android"))]
                {
                    let client = ui_ctx.clone();
                    let weak = weak.clone();
                    ui_ctx.runtime_handle().spawn(async move {
                        let result = async {
                            if client.auth_state().get().session_revision != revision {
                                return Err("Session changed".to_string());
                            }
                            if attachment.content_location == chatter_core::domain::attachment::ContentLocation::NotDownloaded {
                                client.download_attachment_to_filesystem(&att_id).await
                                    .map_err(|_| "Unable to download attachment".to_string())?;
                            }
                            if client.auth_state().get().session_revision != revision {
                                return Err("Session changed".to_string());
                            }
                            crate::bin_ui::features::chat_actions::media_viewer::export(&client, &att_id, false)
                        }.await;
                        let _ = weak.upgrade_in_event_loop(move |w| {
                            if client.auth_state().get().session_revision != revision { return; }
                            let (message, variant) = match result {
                                Ok(message) => (message, "success"),
                                Err(error) => (error, "error"),
                            };
                            w.set_toast_message(message.into());
                            w.set_toast_variant(variant.into());
                            w.set_toast_visible(true);
                        });
                    });
                }
            }
        });
    });
}
