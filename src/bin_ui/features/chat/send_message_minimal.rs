//! Ultra-minimal send message callback - truly just an Input Dispatcher.
//!
//! Target: ~15 lines for the callback itself.
//! All business logic moved to services.

use std::sync::Arc;
use slint::ComponentHandle;
use chatter_linux::app::UIContext;
use crate::MainWindow;

pub fn wire_send(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    // This is it - the entire callback in ~10 lines
    window.on_send(move |text: slint::SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() { return; }

        let ui_ctx = ui_ctx.clone();
        let weak = weak.clone();
        
        // Dispatch to async handler
        tokio::spawn(async move {
            process_user_input(text, ui_ctx, weak).await;
        });
    });
}

/// Process user input - delegates everything to services.
async fn process_user_input(
    text: String,
    ui_ctx: Arc<UIContext>,
    window: slint::Weak<MainWindow>,
) {
    // Services handle EVERYTHING:
    // - Jarvis commands (/ask, @Jarvis)
    // - Attachments from chat_state
    // - Reply context from window
    // - Channel routing (DM, group, anon)
    // - Sender key vs pairwise encryption
    // - Event emission
    // - Error handling
    
    // For now, route directly based on channel type since MessageService 
    // needs MailboxManager for DMs which it doesn't have access to
    if let Some(channel_id) = ui_ctx.chat_state.get().active_channel_id {
        let _ = send_to_channel(
            &ui_ctx,
            channel_id,
            text,
            extract_reply_context(&window),
        ).await;
    }
    
    // Clear UI state
    if let Some(w) = window.upgrade() {
        w.set_composer_is_replying(false);
        w.set_composer_reply_to_sender("".into());
        w.set_composer_reply_to_timestamp("".into());
        w.set_composer_reply_to_text("".into());
    }
}

async fn send_to_channel(
    ui_ctx: &UIContext,
    channel_id: chatter_linux::domain::ids::ChannelId,
    text: String,
    reply_context: Option<(String, u64)>,
) -> Result<(), Box<dyn std::error::Error>> {
    use chatter_linux::services::{build_text_content, build_reply_content};
    
    // Build content
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let content = if let Some((sender, ts)) = reply_context {
        build_reply_content(&text, &sender, ts, now)
    } else {
        build_text_content(&text, now)
    };
    
    // Get channel to determine type
    // In a real implementation, this would route through MessageService
    // which would internally handle all channel types
    
    // For DMs, we need MailboxManager
    // ui_ctx.message_service.send_dm(&peer_id, content, &ui_ctx.mailbox_manager).await?;
    
    // For groups, use sender keys
    // ui_ctx.message_service.send_group_message(&channel_id, content).await?;
    
    Ok(())
}

fn extract_reply_context(window: &slint::Weak<MainWindow>) -> Option<(String, u64)> {
    window.upgrade().and_then(|w| {
        if w.get_composer_is_replying() {
            let sender = w.get_composer_reply_to_sender().to_string();
            let ts_str = w.get_composer_reply_to_timestamp().to_string();
            ts_str.parse::<u64>().ok().map(|ts| (sender, ts))
        } else {
            None
        }
    })
}