//! Thread panel callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, MessageId};
use chatter_core::state::ChatState;

use crate::bin_ui::converters::messages_to_display_batch;
use crate::MainWindow;

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_close_thread(window, ui_ctx);
    wire_send_thread_reply(window, ui_ctx);
    wire_load_more_thread_replies(window, ui_ctx);
}

fn wire_close_thread(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    window.on_close_thread_clicked(move || {
        let weak = weak_window.clone();
        if let Some(thread) = chat_state.active_thread() { chat_state.close_thread(thread.root_message_id); }
        let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
            w.set_thread_panel_visible(false);
            w.set_thread_replies(ModelRc::new(VecModel::default()));
        }});
    });
}

fn wire_send_thread_reply(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let ui_ctx = ui_ctx.clone();
    window.on_send_thread_reply(move |text: SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() { return; }
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let ui_ctx = ui_ctx.clone();
        tokio::spawn(async move {
            let thread = match chat_state.active_thread() { Some(t) => t, None => return };
            if let Ok(_msg) = ui_ctx
                .send_channel_message(None, &thread.channel_id, &text, vec![], vec![], Some(&thread.root_message_id), vec![], false)
                .await
            {
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

fn wire_load_more_thread_replies(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let chat_state = ui_ctx.chat_state().clone();
    let ui_ctx = ui_ctx.clone();
    window.on_load_more_thread_replies(move || {
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let ui_ctx = ui_ctx.clone();
        tokio::spawn(async move {
            let thread = match chat_state.active_thread() { Some(t) => t, None => return };
            if let Ok(replies) = ui_ctx.list_thread_replies(&thread.root_message_id, None, 50) {
                let display = messages_to_display_batch(
                    &replies,
                    |uid| ui_ctx.display_name(uid),
                    |att_id| ui_ctx.get_attachment(att_id),
                    |pid| ui_ctx.reply_parent_missing(pid),
                );
                let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
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
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
    root_message_id: MessageId,
) {
    tokio::spawn(async move {
        if let Some(root_message) = ui_ctx.get_message_local(&root_message_id) {
            let sender_name = root_message
                .sender_id
                .as_ref()
                .map(|id| ui_ctx.display_name(id))
                .unwrap_or_else(|| "Anonymous".to_string());
            let content_preview = root_message
                .as_text()
                .map(|t| if t.len() > 100 { format!("{}...", &t[..97]) } else { t.to_string() })
                .unwrap_or_else(|| "[Attachment]".to_string());
            let replies = ui_ctx.list_thread_replies(&root_message_id, None, 50).unwrap_or_default();
            let channel_name = ui_ctx
                .get_channel_local(&channel_id)
                .map(|c| c.name)
                .unwrap_or_else(|| "Unknown".to_string());
            let display = messages_to_display_batch(
                &replies,
                |uid| ui_ctx.display_name(uid),
                |att_id| ui_ctx.get_attachment(att_id),
                |pid| ui_ctx.reply_parent_missing(pid),
            );
            let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
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
