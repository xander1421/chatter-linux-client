//! Chat action callbacks.
//!
//! Handles message interactions (reply, react, thread, more).

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::MessageId;

use crate::MainWindow;

/// Wire message action callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_message_clicked(window);
    wire_reply_clicked(window, ctx);
    wire_react_clicked(window);
    wire_thread_clicked(window, ctx);
    wire_more_clicked(window);
}

fn wire_message_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_message_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("Message clicked: {}", message_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::trace!("Message selected: {}", message_id_str);
            }
        });
    });
}

fn wire_reply_clicked(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let message_repo = ctx.message_repo.clone();

    window.on_reply_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        let message_repo = message_repo.clone();

        tokio::spawn(async move {
            let msg_id: MessageId = match message_id_str.parse() {
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

fn wire_react_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_react_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("React clicked for message: {}", message_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Emoji picker requested for message: {}", message_id_str);
            }
        });
    });
}

fn wire_thread_clicked(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let chat_state = ctx.chat_state.clone();

    window.on_thread_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let _weak = weak_window.clone();
        let chat_state = chat_state.clone();

        tokio::spawn(async move {
            let msg_id: MessageId = match message_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid message ID for thread: {}", e);
                    return;
                }
            };

            let channel_id = match chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot open thread: no active channel");
                    return;
                }
            };

            chat_state.open_thread(channel_id, msg_id);
            tracing::info!("Thread opened for message: {} in channel: {}", msg_id, channel_id);
        });
    });
}

fn wire_more_clicked(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_more_clicked(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak_window.clone();
        tracing::debug!("More clicked for message: {}", message_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                tracing::info!("Context menu requested for message: {}", message_id_str);
            }
        });
    });
}
