//! Message action callbacks (reply, forward, copy).
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;

use crate::MainWindow;

/// Wire reply-message callback from context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_reply_message(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let message_service = ui_ctx.message_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_reply_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let weak = weak_window.clone();
        let message_service = message_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let anon_group_service = anon_group_service.clone();

        // Get conversation context
        let (selected_dm_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                (w.get_selected_dm_id().to_string(), w.get_selected_private_group_id().to_string())
            })
            .unwrap_or_default();

        tokio::spawn(async move {
            // Parse sender_id for Signal-style lookup
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for reply");
                    return;
                }
            };

            // Parse timestamp for Signal-style lookup
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for reply");
                    return;
                }
            };

            // Signal-style lookup with conversation-aware routing
            let reply_preview = if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
                // Anon DM lookup
                let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_dm_service.get_message_by_sender_timestamp(&anon_dm_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => {
                        let sender = msg.sender_id.to_string();
                        let content = msg.as_text().unwrap_or_default();
                        let preview = if content.chars().count() > 50 {
                            format!("{}...", content.chars().take(47).collect::<String>())
                        } else {
                            content
                        };
                        format!("Reply to {}: {}", sender, preview)
                    }
                    _ => "Reply to message".to_string(),
                }
            } else if !selected_private_group_id.is_empty() {
                // Anon group lookup
                let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_group_service.get_message_by_sender_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => {
                        let sender = msg.sender_id.to_string();
                        let content = msg.as_text().unwrap_or_default();
                        let preview = if content.chars().count() > 50 {
                            format!("{}...", content.chars().take(47).collect::<String>())
                        } else {
                            content
                        };
                        format!("Reply to {}: {}", sender, preview)
                    }
                    _ => "Reply to message".to_string(),
                }
            } else {
                // Regular message lookup
                match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => {
                        let sender = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                        let content = msg.as_text().unwrap_or("").to_string();
                        let preview = if content.chars().count() > 50 {
                            let truncated: String = content.chars().take(47).collect();
                            format!("{}...", truncated)
                        } else {
                            content
                        };
                        format!("Reply to {}: {}", sender, preview)
                    }
                    _ => "Reply to message".to_string(),
                }
            };

            let placeholder = reply_preview.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_placeholder(placeholder.into());
                    tracing::info!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        "Reply mode activated (Signal-style lookup)"
                    );
                }
            });
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
                    Ok(Some(msg)) => {
                        let content = msg.as_text().unwrap_or_default();
                        if content.chars().count() > 100 {
                            format!("{}...", content.chars().take(97).collect::<String>())
                        } else {
                            content
                        }
                    }
                    _ => "Message".to_string(),
                }
            } else if !selected_private_group_id.is_empty() {
                // Anon group lookup
                let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                    Ok(id) => id,
                    Err(_) => return,
                };
                match anon_group_service.get_message_by_sender_timestamp(&anon_group_id, &target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => {
                        let content = msg.as_text().unwrap_or_default();
                        if content.chars().count() > 100 {
                            format!("{}...", content.chars().take(97).collect::<String>())
                        } else {
                            content
                        }
                    }
                    _ => "Message".to_string(),
                }
            } else {
                // Regular message lookup
                match message_service.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                    Ok(Some(msg)) => {
                        let content = msg.as_text().unwrap_or("").to_string();
                        if content.chars().count() > 100 {
                            let truncated: String = content.chars().take(97).collect();
                            format!("{}...", truncated)
                        } else {
                            content
                        }
                    }
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
        });
    });
}
