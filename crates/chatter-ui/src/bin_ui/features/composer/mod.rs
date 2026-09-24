//! Composer callbacks.
//!
//! Handles message composer actions (attach, emoji, gif, mentions, drag/drop).

mod attachments;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod drag_drop;
mod emoji;
mod format_text;
mod gif;
mod mentions;
mod text;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire composer callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    attachments::wire_attachment_clicked(window, ui_ctx.chat_state().clone(), ui_ctx);
    attachments::wire_remove_attachment(window, ui_ctx.chat_state().clone());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    attachments::wire_paste_image(window, ui_ctx.chat_state().clone());
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    drag_drop::wire_drag_drop(window, ui_ctx.chat_state().clone());
    emoji::wire_emoji_clicked(window);
    gif::wire_gif_clicked(window, ui_ctx);
    gif::wire_gif_selected(window, ui_ctx);
    gif::wire_gif_search(window, ui_ctx);
    mentions::wire_mention_triggered(
        window,
        ui_ctx.clone(),
        ui_ctx.chat_state().clone(),
    );
    mentions::wire_mention_selected(window);
    text::wire_process_text(window);
    format_text::wire_format_text(window);
    wire_per_conversation_drafts(window, ui_ctx);
}

/// Each conversation keeps its own unsent text (OPEN-ITEMS §112x). Every
/// conversation switch goes through `ChatState`, so watch it: on a change of
/// active conversation, park the composer under the one left and restore the
/// one entered. Switching never touches the composer itself, so at this point
/// it still holds the text typed in the conversation being left.
fn wire_per_conversation_drafts(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let key = |d: &chatter_core::state::ChatStateData| {
        d.active_channel_id.map(|id| id.to_string()).unwrap_or_default()
    };
    let mut current = key(&ui_ctx.chat_state().get());
    let mut rx = ui_ctx.chat_state().subscribe();
    let weak = slint::ComponentHandle::as_weak(window);
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        loop {
            let snapshot = match rx.recv().await {
                Ok(snap) => snap,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let entering = key(&snapshot);
            if entering == current {
                continue;
            }
            let leaving = std::mem::replace(&mut current, entering.clone());
            let ctx = ctx.clone();
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Left mid-edit: the edit is cancelled and the draft it
                    // held aside is what gets parked, not the edit text.
                    crate::bin_ui::features::chat::edit::end_edit(&w);
                    let text = w.get_composer_text().to_string();
                    w.set_composer_text(ctx.swap_composer_draft(&leaving, text, &entering).into());
                }
            });
        }
    });
}
