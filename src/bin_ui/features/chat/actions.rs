//! Message action callbacks (reply, forward, copy).

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;

use crate::MainWindow;

/// Wire reply-message callback from context menu.
/// This mirrors the behavior of reply_clicked in chat_actions.rs.
pub fn wire_reply_message(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();

    window.on_reply_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let message_repo = message_repo.clone();

        tokio::spawn(async move {
            let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid message ID for reply: {}", e);
                    return;
                }
            };

            let reply_preview = match message_repo.get(&msg_id) {
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
                    tracing::info!("Reply mode activated for message: {}", msg_id);
                }
            });
        });
    });
}

/// Wire forward-message-action callback from context menu.
/// Opens the forward modal with the selected message.
pub fn wire_forward_message_action(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let weak_window = window.as_weak();

    window.on_forward_message_action(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // Look up message to show preview in forward modal
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for forward: {}", e);
                return;
            }
        };

        let preview = match message_repo.get(&msg_id) {
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
                tracing::debug!("Forward modal opened for message: {}", message_id_str);
            }
        });
    });
}

/// Wire copy-message callback to copy message text to clipboard.
pub fn wire_copy_message(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();

    window.on_copy_message(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let message_repo = message_repo.clone();

        // Parse message ID
        let msg_id: chatter_linux::domain::MessageId = match message_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid message ID for copy: {}", e);
                return;
            }
        };

        // Look up the message
        let message = match message_repo.get(&msg_id) {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tracing::error!("Message not found for copy: {}", msg_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to look up message for copy: {}", e);
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
                        tracing::error!("Failed to copy to clipboard: {}", e);
                    } else {
                        tracing::debug!("Copied message text to clipboard");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to access clipboard: {}", e);
                }
            }
        });
    });
}
