//! Message action callbacks (reply, forward, copy).
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::UserId;

use crate::{ForwardTarget, MainWindow, run_ui};

/// Wire reply-message callback from context menu.
/// Gets sender name and preview from the UI message model directly.
pub fn wire_reply_message(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window.on_reply_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();

        // Get reply info directly from UI message model (works for all message types)
        run_ui!(weak_window, |w| {
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

            w.set_composer_is_replying(true);
            w.set_composer_reply_to_sender(sender_id_str.clone().into());
            w.set_composer_reply_to_sender_name(display_name.clone().into());
            w.set_composer_reply_to_timestamp(timestamp_ns_str.clone().into());
            w.set_composer_reply_preview(preview_text.clone().into());
            w.set_composer_placeholder("Type your reply...".into());
            tracing::info!(
                sender = %display_name,
                timestamp = %timestamp_ns_str,
                preview = %preview_text,
                "Reply mode activated (context menu)"
            );
        });
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

            // Lookup message text via ConversationHandle (polymorphic across all types)
            let handle = ui_ctx.conversation(conversation);
            let preview = handle.get_message_text(&target_sender_id, target_sent_timestamp)
                .ok()
                .flatten()
                .map(|t| ui_ctx.truncate_message_preview(&t))
                .unwrap_or_else(|| "Message".to_string());

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
    w.set_active_modal("forward-message".into());
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

    window.on_download_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();

        let conversation = ui_ctx.chat_state().active_conversation();

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

            let download_dir = dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));

            for att_id in attachment_ids {
                let attachment = match ui_ctx.get_attachment(&att_id) {
                    Some(att) => att,
                    None => {
                        tracing::warn!("Attachment {} not found", att_id);
                        continue;
                    }
                };

                let filename = attachment.metadata.name.clone();
                let dest_path = download_dir.join(&filename);

                // Check if attachment needs downloading first
                let needs_download = attachment.content_location == chatter_core::domain::attachment::ContentLocation::NotDownloaded
                    && attachment.remote_url.is_some()
                    && attachment.encryption_key.is_some();

                if needs_download {
                    // Download first, then save
                    let ui_ctx_dl = ui_ctx.clone();
                    ui_ctx.runtime_handle().spawn(async move {
                        match ui_ctx_dl.download_attachment_to_filesystem(&att_id).await {
                            Ok(()) => {
                                match ui_ctx_dl.decrypt_attachment_auto(&att_id) {
                                    Ok(buffer) => {
                                        if let Err(e) = std::fs::write(&dest_path, buffer.as_slice()) {
                                            tracing::error!(error = %e, "Failed to save attachment");
                                        } else {
                                            tracing::info!("Downloaded attachment to {}", dest_path.display());
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "Failed to decrypt attachment");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to download attachment");
                            }
                        }
                    });
                } else {
                    // Already downloaded, just decrypt and save
                    match ui_ctx.decrypt_attachment_auto(&att_id) {
                        Ok(buffer) => {
                            if let Err(e) = std::fs::write(&dest_path, buffer.as_slice()) {
                                tracing::error!(error = %e, "Failed to save attachment");
                            } else {
                                tracing::info!("Saved attachment to {}", dest_path.display());
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to decrypt attachment");
                        }
                    }
                }
            }
        });
    });
}
