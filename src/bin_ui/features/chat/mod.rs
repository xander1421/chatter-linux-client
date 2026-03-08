//! Chat callbacks.
//!
//! Handles sending messages to channels and DMs, typing indicators,
//! reactions, edits, deletes, pins, and other message actions.

mod actions;
pub mod emoji_loader;
mod context_menu;
mod delete;
mod edit;
mod helpers;
mod pin;
mod reactions;
mod send_message;
mod typing;

use std::sync::Arc;

use chatter_linux::app::UIContext;

use crate::MainWindow;

pub use helpers::{
    get_dm_channel_from_peer, send_channel_delivery_receipt, send_channel_read_receipts,
    send_dm_delivery_receipt, send_dm_read_receipts,
};

/// Wire chat callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    send_message::wire_send(window, ui_ctx);
    typing::wire_typing_changed(window, ui_ctx);
    reactions::wire_reaction_selected(window, ui_ctx);
    context_menu::wire_more_clicked(window, ui_ctx);
    edit::wire_edit_message(window, ui_ctx);
    edit::wire_send_edit(window, ui_ctx);
    edit::wire_cancel_edit(window);
    delete::wire_delete_message(window, ui_ctx);
    delete::wire_delete_everyone_message(window, ui_ctx);
    pin::wire_pin_message(window, ui_ctx);
    actions::wire_reply_message(window, ui_ctx);
    actions::wire_forward_message_action(window, ui_ctx);
    actions::wire_copy_message(window, ui_ctx);
    emoji_loader::wire_emoji_loader(window);
}
