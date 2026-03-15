//! Message action callbacks (reply, forward, copy).
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;

use crate::MainWindow;

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
    let message_service = ui_ctx.message_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_forward_message_action(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_service = message_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let anon_group_service = anon_group_service.clone();
        let weak = weak_window.clone();

        // Get conversation context (quick UI reads)
        let (selected_dm_id, selected_private_group_id) = weak
            .upgrade()
            .map(|w| {
                (w.get_selected_dm_id().to_string(), w.get_selected_private_group_id().to_string())
            })
            .unwrap_or_default();

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

            // Signal-style lookup with conversation-aware routing (now in background)
            let preview = if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
                // Anon DM lookup
                let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_dm_service.get_message_by_sender_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => chatter_linux::services::gateway::shared::truncate_preview(&msg.as_text().unwrap_or_default()),
                    _ => "Message".to_string(),
                }
            } else if !selected_private_group_id.is_empty() {
                // Anon group lookup
                let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_group_service.get_message_by_sender_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => chatter_linux::services::gateway::shared::truncate_preview(&msg.as_text().unwrap_or_default()),
                    _ => "Message".to_string(),
                }
            } else {
                // Regular message lookup
                match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => chatter_linux::services::gateway::shared::truncate_preview(&msg.as_text().unwrap_or("").to_string()),
                    _ => "Message".to_string(),
                }
            };

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_forward_message_preview(preview.into());
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
    let message_service = ui_ctx.message_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_copy_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_service = message_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let anon_group_service = anon_group_service.clone();

        // Get conversation context (quick UI reads)
        let (selected_dm_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                (w.get_selected_dm_id().to_string(), w.get_selected_private_group_id().to_string())
            })
            .unwrap_or_default();

        // Move service lookups and clipboard to background thread
        std::thread::spawn(move || {
            // Parse sender_id for Signal-style lookup
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for copy");
                    return;
                }
            };

            // Parse timestamp for Signal-style lookup
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for copy");
                    return;
                }
            };

            // Signal-style lookup with conversation-aware routing (now in background)
            let text: Option<String> = if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
                // Anon DM lookup
                let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_dm_service.get_message_by_sender_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg.as_text(),
                    _ => {
                        tracing::error!("Anon DM message not found for copy");
                        return;
                    }
                }
            } else if !selected_private_group_id.is_empty() {
                // Anon group lookup
                let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_group_service.get_message_by_sender_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg.as_text(),
                    _ => {
                        tracing::error!("Anon group message not found for copy");
                        return;
                    }
                }
            } else {
                // Regular message lookup
                let message = match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => {
                        tracing::error!(
                            sender_id = %target_sender_id,
                            timestamp = target_sent_timestamp,
                            "Message not found for copy (Signal-style lookup)"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up message for copy");
                        return;
                    }
                };

                // Extract text from message content
                use chatter_linux::domain::message::MessageContent;
                match &message.content {
                    MessageContent::Text(s) => Some(s.clone()),
                    MessageContent::Mixed(m) => m.text.clone(),
                    _ => {
                        tracing::debug!("Cannot copy non-text message content");
                        return;
                    }
                }
            };

            let text = match text {
                Some(t) => t,
                None => {
                    tracing::debug!("No text content to copy");
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
    let message_service = ui_ctx.message_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let grpc_pool = ui_ctx.grpc_pool.clone();
    let runtime_handle = ui_ctx.runtime_handle.clone();
    let weak_window = window.as_weak();

    window.on_download_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_service = message_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let anon_group_service = anon_group_service.clone();
        let attachment_service = attachment_service.clone();
        let grpc_pool = grpc_pool.clone();
        let runtime_handle = runtime_handle.clone();

        // Get conversation context
        let (selected_dm_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                (w.get_selected_dm_id().to_string(), w.get_selected_private_group_id().to_string())
            })
            .unwrap_or_default();

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

            // Look up message to get attachment IDs (Signal-style with conversation-aware routing)
            let attachment_ids = if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
                // Anon DM lookup
                let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(error = %e, "Invalid anon DM ID for download");
                        return;
                    }
                };
                match anon_dm_service.get_message_by_sender_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg.attachment_ids(),
                    Ok(None) => {
                        tracing::error!("Anon DM message not found for download");
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up anon DM message for download");
                        return;
                    }
                }
            } else if !selected_private_group_id.is_empty() {
                // Anon group lookup
                let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(error = %e, "Invalid anon group ID for download");
                        return;
                    }
                };
                match anon_group_service.get_message_by_sender_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg.attachment_ids(),
                    Ok(None) => {
                        tracing::error!("Anon group message not found for download");
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up anon group message for download");
                        return;
                    }
                }
            } else {
                // Regular message
                match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => msg.attachments,
                    Ok(None) => {
                        tracing::error!("Message not found for download");
                        return;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to look up message for download");
                        return;
                    }
                }
            };

            if attachment_ids.is_empty() {
                tracing::debug!("No attachments to download");
                return;
            }

            let download_dir = dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));

            for att_id in attachment_ids {
                let attachment = match attachment_service.get(&att_id) {
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
                let needs_download = attachment.content_location == chatter_linux::domain::attachment::ContentLocation::NotDownloaded
                    && attachment.remote_url.is_some()
                    && attachment.encryption_key.is_some();

                let attachment_service_clone = attachment_service.clone();
                let grpc_pool_clone = grpc_pool.clone();
                let att_id_clone = att_id;
                let dest_path_clone = dest_path.clone();

                if needs_download {
                    // Download first, then save
                    runtime_handle.spawn(async move {
                        match attachment_service_clone.download_to_filesystem(&grpc_pool_clone, &att_id_clone).await {
                            Ok(()) => {
                                // Now decrypt and save
                                match attachment_service_clone.decrypt_auto(&att_id_clone) {
                                    Ok(buffer) => {
                                        if let Err(e) = std::fs::write(&dest_path_clone, buffer.as_slice()) {
                                            tracing::error!(error = %e, "Failed to save attachment");
                                        } else {
                                            tracing::info!("Downloaded attachment to {}", dest_path_clone.display());
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
                    match attachment_service.decrypt_auto(&att_id) {
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
