//! Message action callbacks (reply, forward, copy).
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;

use crate::MainWindow;

/// Wire reply-message callback from context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_reply_message(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_reply_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let weak = weak_window.clone();
        let message_repo = message_repo.clone();

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

            // Signal-style lookup: (sender_id, sent_at)
            let reply_preview = match message_repo.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                Ok(Some(msg)) => {
                    let sender = msg.sender_id.map(|id| id.to_string()).unwrap_or_default();
                    let content = msg.as_text().unwrap_or("").to_string();
                    let preview = if content.len() > 50 {
                        format!("{}...", &content[..47])
                    } else {
                        content
                    };
                    format!("Reply to {}: {}", sender, preview)
                }
                _ => "Reply to message".to_string(),
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
pub fn wire_forward_message_action(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_forward_message_action(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

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

        // Signal-style lookup: (sender_id, sent_at)
        let preview = match message_repo.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
            Ok(Some(msg)) => {
                let content = msg.as_text().unwrap_or("").to_string();
                if content.len() > 100 {
                    format!("{}...", &content[..97])
                } else {
                    content
                }
            }
            _ => "Message".to_string(),
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
}

/// Wire copy-message callback to copy message text to clipboard.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_copy_message(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_copy_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_repo = message_repo.clone();

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

        // Signal-style lookup: (sender_id, sent_at)
        let message = match message_repo.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
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
        let text = match &message.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Mixed(m) => match &m.text {
                Some(t) => t.clone(),
                None => {
                    tracing::debug!("Mixed content has no text");
                    return;
                }
            },
            _ => {
                tracing::debug!("Cannot copy non-text message content");
                return;
            }
        };

        // Copy text to clipboard using arboard
        std::thread::spawn(move || {
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
