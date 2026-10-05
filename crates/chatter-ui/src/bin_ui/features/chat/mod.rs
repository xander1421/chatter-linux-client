//! Chat callbacks.
//!
//! Handles sending messages to channels and DMs, typing indicators,
//! reactions, edits, deletes, pins, and other message actions.

mod actions;
mod context_menu;
mod delete;
pub mod disappearing;
pub(crate) mod edit;
pub mod emoji_loader;
mod helpers;
pub mod identity_change;
pub mod membership;
mod pin;
mod reactions;
mod selection;
pub(crate) mod send_message;
mod typing;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub(crate) use helpers::{edit_messages, remove_messages_where, update_reaction_in_ui};
// Re-exported so the ui_wiring harness can wire ONLY the forward-picker search
// against a real offline ChatterClient, without the full `wire` (which needs a
// live event loop). Registration-only; no behavioural change to the app.
pub use actions::wire_forward_search;
pub use send_message::{refresh_send_notice, show_send_notice};

/// Wire chat callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
    actions::wire_forward_search(window, ui_ctx);
    actions::wire_copy_message(window, ui_ctx);
    actions::wire_download_message(window, ui_ctx);
    selection::wire_selection(window, ui_ctx);
    identity_change::wire_accept(window, ui_ctx);
    emoji_loader::wire_emoji_loader(window, ui_ctx);
    disappearing::wire(window, ui_ctx);
    membership::wire(window, ui_ctx);
}
