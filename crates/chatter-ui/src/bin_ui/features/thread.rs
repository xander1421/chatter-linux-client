//! Thread panel callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::domain::message::Message;
use chatter_core::state::ChatState;

use crate::bin_ui::converters::messages_to_display_batch;
use crate::{MainWindow, MessageItemData};

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
            if let Ok(msg) = ui_ctx
                .send_channel_message(None, &thread.channel_id, &text, vec![], vec![], Some(&thread.root_message_id), vec![], false)
                .await
            {
                chat_state.update_thread_reply_count(thread.root_message_id, thread.reply_count + 1);
                // Show our own reply in the panel immediately — the send path
                // stores it locally but emits no MessageReceived event, so
                // without this it only appeared after re-opening the thread.
                let reply = thread_reply_display(&ui_ctx, &msg);
                let current_user_id = ui_ctx
                    .session_current_user_id()
                    .map(|id| id.to_string())
                    .unwrap_or_default();
                let root_id = thread.root_message_id;
                let _ = slint::invoke_from_event_loop(move || { if let Some(w) = weak.upgrade() {
                    w.set_thread_composer_text(SharedString::default());
                    apply_live_thread_reply(
                        &w, &root_id, reply.to_slint_with_user(&current_user_id), true,
                    );
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
        // Cursor = the oldest reply on screen. Read on the UI thread: a Slint
        // Weak never upgrades inside tokio::spawn.
        let Some(w) = weak.upgrade() else { return };
        let Some(oldest) = w.get_thread_replies().row_data(0)
            .and_then(|row| row.message_id.parse::<MessageId>().ok()) else { return };
        tokio::spawn(async move {
            let thread = match chat_state.active_thread() { Some(t) => t, None => return };
            let root = thread.root_message_id;
            let Ok(page) = ui_ctx.list_thread_replies(&root, Some(&oldest), 50) else { return };
            if page.is_empty() { return; }
            let display = messages_to_display_batch(
                &page,
                |m| ui_ctx.message_sender_display_name(m),
                |att_id| ui_ctx.get_attachment(att_id),
                |m| ui_ctx.reply_author_display_name(m),
                |m| ui_ctx.reply_parent_missing_for(m),
                |m| ui_ctx.reply_parent_media(m),
            );
            let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                // The user may have switched threads while this page loaded.
                if w.get_thread_root_message_id() != root.to_string().as_str() { return; }
                let current = w.get_thread_replies();
                let mut rows: Vec<MessageItemData> =
                    display.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
                rows.extend((0..current.row_count()).filter_map(|i| current.row_data(i)));
                w.set_thread_replies(ModelRc::new(VecModel::from(rows)));
            });
        });
    });
}

/// Route a thread reply into the live UI (must run on the UI thread).
///
/// Both halves of "a reply just happened": append it to the thread panel when
/// that thread is the one on screen, and bump the "N replies" pill on the root
/// bubble in the main list. Used by the receive path and by our own send, so a
/// reply shows up without re-navigating the channel.
pub fn apply_live_thread_reply(
    w: &MainWindow,
    root_message_id: &MessageId,
    reply: MessageItemData,
    panel_shows_this_thread: bool,
) {
    if panel_shows_this_thread {
        let replies = w.get_thread_replies();
        let already_present = (0..replies.row_count())
            .filter_map(|i| replies.row_data(i))
            .any(|m| m.message_id == reply.message_id);
        if !already_present {
            match replies.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                Some(model) => model.push(reply),
                None => {
                    let mut all: Vec<MessageItemData> =
                        (0..replies.row_count()).filter_map(|i| replies.row_data(i)).collect();
                    all.push(reply);
                    w.set_thread_replies(ModelRc::new(VecModel::from(all)));
                }
            }
            w.set_thread_reply_count(w.get_thread_reply_count() + 1);
        }
    }

    bump_root_reply_count(w, root_message_id);
}

/// Patch an edited message into the open thread panel (must run on the UI
/// thread). The panel keeps its own model, so the main-list patch never reaches
/// it; without this an edited reply kept its old text until the thread was
/// reopened. Also refreshes the root preview when the root itself was edited.
pub fn apply_thread_edit(
    w: &MainWindow,
    message_id: &str,
    target_sent_timestamp: u64,
    new_text: &str,
) {
    patch_edited_row(&w.get_thread_replies(), message_id, target_sent_timestamp, new_text);
    if !message_id.is_empty() && w.get_thread_root_message_id().as_str() == message_id {
        w.set_thread_root_content_preview(
            chatter_core::domain::message::truncate_preview_text(new_text, 100).into(),
        );
    }
}

/// Find the edited row — by sent timestamp (Signal-style identity) first, by
/// message id only when there is no timestamp — and apply the edit. Returns
/// whether a row matched.
fn patch_edited_row(
    rows: &ModelRc<MessageItemData>,
    message_id: &str,
    target_sent_timestamp: u64,
    new_text: &str,
) -> bool {
    let ts = target_sent_timestamp.to_string();
    let hit = (0..rows.row_count()).find(|&i| {
        rows.row_data(i).is_some_and(|m| {
            if target_sent_timestamp > 0 {
                m.timestamp_ns.as_str() == ts
            } else {
                m.message_id.as_str() == message_id
            }
        })
    });
    let (Some(i), Some(model)) = (hit, rows.as_any().downcast_ref::<VecModel<MessageItemData>>())
    else {
        return false;
    };
    let Some(mut row) = model.row_data(i) else { return false };
    crate::bin_ui::converters::apply_edit(&mut row, new_text);
    model.set_row_data(i, row);
    true
}

/// Increment the "N replies" indicator on a thread root bubble in the main list.
fn bump_root_reply_count(w: &MainWindow, root_message_id: &MessageId) {
    let root_id = root_message_id.to_string();
    let messages = w.get_messages();
    let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() else {
        return;
    };
    for i in 0..messages.row_count() {
        let Some(msg) = messages.row_data(i) else { continue };
        if msg.message_id.as_str() != root_id {
            continue;
        }
        let mut updated = msg;
        updated.reply_count += 1;
        updated.reply_count_text =
            crate::bin_ui::converters::format_count(updated.reply_count, "reply", "replies").into();
        model.set_row_data(i, updated);
        return;
    }
}

/// Convert a freshly sent thread reply for display in the panel.
///
/// Returns the thread-safe display data — `MessageItemData` holds a
/// `slint::Image` and can't cross the thread hop.
pub fn thread_reply_display(
    ui_ctx: &Arc<ChatterClient>,
    message: &Message,
) -> crate::bin_ui::converters::MessageDisplayData {
    let mut display = messages_to_display_batch(
        std::slice::from_ref(message),
        |m| ui_ctx.message_sender_display_name(m),
        |att_id| ui_ctx.get_attachment(att_id),
        |m| ui_ctx.reply_author_display_name(m),
        |m| ui_ctx.reply_parent_missing_for(m),
        |m| ui_ctx.reply_parent_media(m),
    );
    display.remove(0)
}

pub fn load_thread_data(
    window: slint::Weak<MainWindow>,
    _chat_state: ChatState,
    ui_ctx: Arc<ChatterClient>,
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
                .map(|t| chatter_core::domain::message::truncate_preview_text(t, 100))
                .unwrap_or_else(|| "[Attachment]".to_string());
            let replies = ui_ctx.list_thread_replies(&root_message_id, None, 50).unwrap_or_default();
            // The true total, not the page size: the panel offers "earlier
            // replies" while it shows fewer than this.
            let total_replies = ui_ctx.thread_reply_count(&root_message_id);
            let display = messages_to_display_batch(
                &replies,
                |m| ui_ctx.message_sender_display_name(m),
                |att_id| ui_ctx.get_attachment(att_id),
                |m| ui_ctx.reply_author_display_name(m),
                |m| ui_ctx.reply_parent_missing_for(m),
                |m| ui_ctx.reply_parent_media(m),
            );
            let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || { if let Some(w) = window.upgrade() {
                w.set_thread_panel_visible(true);
                w.set_thread_root_message_id(root_message_id.to_string().into());
                w.set_thread_root_sender_name(sender_name.into());
                w.set_thread_root_content_preview(content_preview.into());
                w.set_thread_reply_count(total_replies as i32);
                w.set_thread_loading(false);
                let slint_replies: Vec<_> = display.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
                w.set_thread_replies(ModelRc::new(VecModel::from(slint_replies)));
            }});
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, ts: &str, text: &str) -> MessageItemData {
        MessageItemData {
            message_id: id.into(),
            timestamp_ns: ts.into(),
            content: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn edit_patches_the_matching_thread_reply_only() {
        let rows: ModelRc<MessageItemData> =
            ModelRc::new(VecModel::from(vec![row("a", "100", "one"), row("b", "200", "two")]));

        assert!(patch_edited_row(&rows, "b", 200, "two edited"));
        assert_eq!(rows.row_data(1).unwrap().content, "two edited");
        assert!(rows.row_data(1).unwrap().is_edited);
        assert_eq!(rows.row_data(0).unwrap().content, "one");

        // Timestamp is the identity: a message-id match on another row is not enough.
        assert!(!patch_edited_row(&rows, "a", 999, "wrong"));
        assert_eq!(rows.row_data(0).unwrap().content, "one");
    }
}
