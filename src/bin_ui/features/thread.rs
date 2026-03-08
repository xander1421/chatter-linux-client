//! Thread panel callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::{ChannelId, MessageId};
use chatter_linux::services::attachment::AttachmentService;
use chatter_linux::services::channel::ChannelService;
use chatter_linux::services::message::MessageService;
use chatter_linux::services::user::UserService;
use chatter_linux::state::ChatState;

use crate::bin_ui::converters::message_to_display_data;
use crate::MainWindow;

pub fn wire(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    wire_close_thread(window, ui_ctx);
    wire_send_thread_reply(window, ui_ctx);
    wire_load_more_thread_replies(window, ui_ctx);
}

fn wire_close_thread(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state.clone();
    window.on_close_thread_clicked(move || {
        let weak = weak_window.clone();
        if let Some(thread) = chat_state.active_thread() { chat_state.close_thread(thread.root_message_id); }
        let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
            w.set_thread_panel_visible(false);
            w.set_thread_replies(ModelRc::new(VecModel::default()));
        }});
    });
}

fn wire_send_thread_reply(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state.clone();
    let message_service = ui_ctx.message_service.clone();
    window.on_send_thread_reply(move |text: SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() { return; }
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_service = message_service.clone();
        tokio::spawn(async move {
            let thread = match chat_state.active_thread() { Some(t) => t, None => return };
            if let Ok(_msg) = message_service.send(None, &thread.channel_id, &text, vec![], vec![], Some(&thread.root_message_id), vec![], false).await {
                chat_state.update_thread_reply_count(thread.root_message_id, thread.reply_count + 1);
                let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
                    w.set_thread_composer_text(SharedString::default());
                    let count = w.get_thread_reply_count();
                    w.set_thread_reply_count(count + 1);
                }});
            }
        });
    });
}

fn wire_load_more_thread_replies(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state.clone();
    let message_service = ui_ctx.message_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    window.on_load_more_thread_replies(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        tokio::spawn(async move {
            let thread = match chat_state.active_thread() { Some(t) => t, None => return };
            if let Ok(replies) = message_service.list_for_thread(&thread.root_message_id, None, 50) {
                let mut display = Vec::new();
                for msg in &replies {
                    let sender_name = msg.sender_id.as_ref().map(|id| user_service.get_display_name(id)).unwrap_or_else(|| "Anonymous".to_string());
                    let resolved: Vec<_> = msg.attachments.iter().filter_map(|id| attachment_service.get(id).ok().flatten()).collect();
                    display.push(message_to_display_data(msg, &sender_name, &resolved));
                }
                // Get current user ID for is_own calculation
                let current_user_id = user_service.current_user_id().ok().map(|id| id.to_string()).unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
                    let slint_messages: Vec<_> = display.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
                    w.set_thread_replies(ModelRc::new(VecModel::from(slint_messages)));
                }});
            }
        });
    });
}

pub fn load_thread_data(
    window: slint::Weak<MainWindow>,
    _chat_state: ChatState,
    message_service: Arc<MessageService>,
    user_service: Arc<UserService>,
    attachment_service: Arc<AttachmentService>,
    channel_service: Arc<ChannelService>,
    channel_id: ChannelId,
    root_message_id: MessageId,
) {
    tokio::spawn(async move {
        if let Ok(Some(root_message)) = message_service.get_message_local(&root_message_id) {
            let sender_name = root_message.sender_id.as_ref().map(|id| user_service.get_display_name(id)).unwrap_or_else(|| "Anonymous".to_string());
            let content_preview = root_message.as_text().map(|t| if t.len() > 100 { format!("{}...", &t[..97]) } else { t.to_string() }).unwrap_or_else(|| "[Attachment]".to_string());
            let replies = message_service.list_for_thread(&root_message_id, None, 50).unwrap_or_default();
            let channel_name = channel_service.get_channel_local(&channel_id).ok().flatten().map(|c| c.name).unwrap_or_else(|| "Unknown".to_string());
            let mut display = Vec::new();
            for msg in &replies {
                let reply_sender_name = msg.sender_id.as_ref().map(|id| user_service.get_display_name(id)).unwrap_or_else(|| "Anonymous".to_string());
                let resolved: Vec<_> = msg.attachments.iter().filter_map(|id| attachment_service.get(id).ok().flatten()).collect();
                display.push(message_to_display_data(msg, &reply_sender_name, &resolved));
            }
            // Get current user ID for is_own calculation
            let current_user_id = user_service.current_user_id().ok().map(|id| id.to_string()).unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || { if let Some(w) = window.upgrade() {
                w.set_thread_panel_visible(true);
                w.set_thread_root_message_id(root_message_id.to_string().into());
                w.set_thread_root_sender_name(sender_name.into());
                w.set_thread_root_content_preview(content_preview.into());
                w.set_thread_reply_count(replies.len() as i32);
                w.set_thread_channel_name(channel_name.into());
                w.set_thread_loading(false);
                let slint_replies: Vec<_> = display.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
                w.set_thread_replies(ModelRc::new(VecModel::from(slint_replies)));
            }});
        }
    });
}
