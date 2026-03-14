//! Simplified send message callback - acts as an Input Dispatcher.
//!
//! This is the refactored version that delegates to services instead of 
//! containing business logic. The goal is ~15 lines instead of 839.

use std::sync::Arc;
use slint::ComponentHandle;
use chatter_linux::app::UIContext;
use crate::MainWindow;
use super::typing::clear_outgoing_typing;

pub fn wire_send(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_send(move |text: slint::SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() && ui_ctx.chat_state.get_pending_attachments().is_empty() {
            return;
        }

        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        
        tokio::spawn(async move {
            dispatch_input(text, ui_ctx, weak).await;
        });
    });
}

async fn dispatch_input(
    text: String,
    ui_ctx: Arc<UIContext>,
    window: slint::Weak<MainWindow>,
) {
    // 1. Clear typing indicator
    clear_outgoing_typing();
    
    // 2. Check if Jarvis handles this input
    let jarvis_context = build_jarvis_context(&ui_ctx, &window);
    let author_id = ui_ctx.auth_state.get_current_user_id();
    
    if ui_ctx.jarvis_service.process_input(&text, &jarvis_context, author_id.as_ref())
        .await.unwrap_or(false) {
        return; // Jarvis handled it
    }

    // 3. Get active channel
    let Some(channel_id) = ui_ctx.chat_state.get().active_channel_id else {
        return;
    };

    // 4. Build message with reply context
    let content = build_message_content(text, &window);

    // 5. Send (MessageService handles routing, attachments, and events)
    let _ = ui_ctx.message_service.send_message(channel_id, content).await;
    
    // 6. Clear UI state
    ui_ctx.chat_state.clear_pending_attachments();
    clear_reply_state(&window);
}

fn build_jarvis_context(
    ui_ctx: &Arc<UIContext>,
    window: &slint::Weak<MainWindow>,
) -> chatter_linux::services::jarvis::JarvisContext {
    let channel_id = ui_ctx.chat_state.get().active_channel_id;
    let channel_type = channel_id
        .and_then(|id| ui_ctx.channel_service.get_channel_type(&id).ok())
        .unwrap_or(chatter_linux::domain::channel::ChannelType::TextChannel);
    
    chatter_linux::services::jarvis::JarvisContext {
        channel_id,
        channel_type,
        is_thread: false,
        thread_root_id: None,
    }
}

fn build_message_content(
    text: String,
    window: &slint::Weak<MainWindow>,
) -> chatter_linux::proto::content_v1::Content {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    
    // Check for reply context
    if let Some(w) = window.upgrade() {
        if w.get_composer_is_replying() {
            let sender = w.get_composer_reply_to_sender().to_string();
            let timestamp_str = w.get_composer_reply_to_timestamp().to_string();
            
            if !sender.is_empty() && !timestamp_str.is_empty() {
                if let Ok(ts) = timestamp_str.parse::<u64>() {
                    return chatter_linux::services::build_reply_content(
                        &text,
                        &sender,
                        ts,
                        now,
                    );
                }
            }
        }
    }

    // Regular text message
    chatter_linux::services::build_text_content(&text, now)
}

fn clear_reply_state(window: &slint::Weak<MainWindow>) {
    if let Some(w) = window.upgrade() {
        w.set_composer_is_replying(false);
        w.set_composer_reply_to_sender("".into());
        w.set_composer_reply_to_timestamp("".into());
        w.set_composer_reply_to_text("".into());
    }
}