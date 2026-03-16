//! Message action callbacks (reply, forward, copy).
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::app::UIContext;
use chatter_core::domain::UserId;

use crate::{ForwardTarget, MainWindow};

/// Wire reply-message callback from context menu.
/// Gets sender name and preview from the UI message model directly.
pub fn wire_reply_message(window: &MainWindow, _ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();

    window.on_reply_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let weak = weak_window.clone();

        // Get reply info directly from UI message model (works for all message types)
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };

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
pub fn wire_forward_message_action(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_forward_message_action(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        let conversation = ui_ctx.chat_state.active_conversation();

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
            let handle = chatter_core::services::conversation::ConversationHandle::new(
                conversation, ui_ctx.clone(),
            );
            let preview = handle.get_message_text(&target_sender_id, target_sent_timestamp)
                .ok()
                .flatten()
                .map(|t| chatter_core::services::gateway::shared::truncate_preview(&t))
                .unwrap_or_else(|| "Message".to_string());

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_forward_message_preview(preview.into());

                    // Populate forward targets from sidebar data
                    let mut targets: Vec<ForwardTarget> = Vec::new();

                    // Add DM conversations
                    let dms = w.get_dm_conversations();
                    for i in 0..dms.row_count() {
                        if let Some(dm) = dms.row_data(i) {
                            targets.push(ForwardTarget {
                                id: dm.channel_id.clone(),
                                name: dm.display_name.clone(),
                                avatar_url: dm.avatar_url.clone(),
                                target_type: "dm".into(),
                            });
                        }
                    }

                    // Add channels
                    let channels = w.get_channels();
                    for i in 0..channels.row_count() {
                        if let Some(ch) = channels.row_data(i) {
                            targets.push(ForwardTarget {
                                id: ch.channel_id.clone(),
                                name: ch.name.clone(),
                                avatar_url: "".into(),
                                target_type: "channel".into(),
                            });
                        }
                    }

                    // Add private groups
                    let groups = w.get_private_groups();
                    for i in 0..groups.row_count() {
                        if let Some(g) = groups.row_data(i) {
                            targets.push(ForwardTarget {
                                id: g.group_id.clone(),
                                name: g.name.clone(),
                                avatar_url: "".into(),
                                target_type: "channel".into(),
                            });
                        }
                    }

                    w.set_forward_targets(ModelRc::from(
                        std::rc::Rc::new(VecModel::from(targets)),
                    ));

                    w.set_active_modal("forward-message".into());
                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        "Forward modal opened (Signal-style lookup)"
                    );
                }
            });
        });
    });
}

/// Wire copy-message callback to copy message text to clipboard.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_copy_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_copy_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();

        let conversation = ui_ctx.chat_state.active_conversation();

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
            let handle = chatter_core::services::conversation::ConversationHandle::new(
                conversation, ui_ctx.clone(),
            );
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

            // Copy text to clipboard using arboard
            #[cfg(not(target_os = "android"))]
            match arboard::Clipboard::new() {
                Ok(mut clipboard) => {
                    if let Err(e) = clipboard.set_text(&text) {
                        tracing::error!(error = %e, "Failed to copy to clipboard");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            timestamp = %timestamp_ns_str,
                            "Copied message text to clipboard (Signal-style lookup)"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to access clipboard");
                }
            }
            #[cfg(target_os = "android")]
            tracing::warn!("Clipboard not yet supported on Android");
        });
    });
}

/// Wire download-message callback from context menu.
/// Downloads all attachments from a message to the user's Downloads folder.
pub fn wire_download_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();

    window.on_download_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = ui_ctx.clone();

        let conversation = ui_ctx.chat_state.active_conversation();

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
            let handle = chatter_core::services::conversation::ConversationHandle::new(
                conversation, ui_ctx.clone(),
            );
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
                let attachment = match ui_ctx.attachment_service.get(&att_id) {
                    Ok(Some(att)) => att,
                    Ok(None) => {
                        tracing::warn!("Attachment {} not found", att_id);
                        continue;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to get attachment {}", att_id);
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
                    let attachment_service = ui_ctx.attachment_service.clone();
                    ui_ctx.runtime_handle.spawn(async move {
                        match attachment_service.download_to_filesystem(&att_id).await {
                            Ok(()) => {
                                // Now decrypt and save
                                match attachment_service.decrypt_auto(&att_id) {
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
                    match ui_ctx.attachment_service.decrypt_auto(&att_id) {
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
