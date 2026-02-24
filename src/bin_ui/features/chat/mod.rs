//! Chat callbacks.
//!
//! Handles sending messages to channels and DMs, typing indicators,
//! reactions, edits, deletes, pins, and other message actions.

mod actions;
mod context_menu;
mod delete;
mod edit;
mod helpers;
mod pin;
mod reactions;
mod send_message;
mod typing;

use chatter_linux::app::AppContext;

use crate::MainWindow;

pub use helpers::{get_dm_channel_from_peer, send_dm_read_receipts};

/// Wire chat callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    send_message::wire_send(window, ctx);
    typing::wire_typing_changed(window, ctx);
    reactions::wire_reaction_selected(window, ctx);
    context_menu::wire_more_clicked(window, ctx);
    edit::wire_edit_message(window, ctx);
    edit::wire_send_edit(window, ctx);
    edit::wire_cancel_edit(window);
    delete::wire_delete_message(window, ctx);
    delete::wire_delete_everyone_message(window, ctx);
    pin::wire_pin_message(window, ctx);
    actions::wire_reply_message(window, ctx);
    actions::wire_forward_message_action(window, ctx);
    actions::wire_copy_message(window, ctx);
}
