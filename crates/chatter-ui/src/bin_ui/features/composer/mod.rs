//! Composer callbacks.
//!
//! Handles message composer actions (attach, emoji, gif, mentions, drag/drop).

mod attachments;
#[cfg(not(target_os = "android"))]
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
    attachments::wire_attachment_clicked(window, ui_ctx.chat_state().clone());
    attachments::wire_remove_attachment(window, ui_ctx.chat_state().clone());
    #[cfg(not(target_os = "android"))]
    drag_drop::wire_drag_drop(window, ui_ctx.chat_state().clone());
    emoji::wire_emoji_clicked(window);
    emoji::wire_emoji_selected(window);
    emoji::wire_emoji_category_changed(window);
    emoji::wire_emoji_search_changed(window);
    gif::wire_gif_clicked(window, ui_ctx);
    gif::wire_gif_selected(window, ui_ctx);
    gif::wire_gif_search(window, ui_ctx);
    mentions::wire_mention_triggered(
        window,
        ui_ctx.channels().clone(),
        ui_ctx.users().clone(),
        ui_ctx.chat_state().clone(),
    );
    mentions::wire_mention_selected(window);
    text::wire_process_text(window);
    format_text::wire_format_text(window);
}
