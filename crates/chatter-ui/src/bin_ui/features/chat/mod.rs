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
#[cfg(feature = "ui-test-debug")]
#[doc(hidden)]
pub mod send_message;
#[cfg(not(feature = "ui-test-debug"))]
pub(crate) mod send_message;
mod typing;

use slint::ComponentHandle;
use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

#[doc(hidden)]
pub use edit::apply_message_edit_in_conversation;
#[doc(hidden)]
pub use helpers::remove_messages_in_conversation;
pub(crate) use helpers::{edit_messages, recompute_channel_continuations, remove_messages_where};
#[doc(hidden)]
pub use helpers::{refresh_reaction_in_ui, refresh_scoped_reaction_in_ui};
// Re-exported so the ui_wiring harness can wire ONLY the forward-picker search
// against a real offline ChatterClient, without the full `wire` (which needs a
// live event loop). Registration-only; no behavioural change to the app.
pub(crate) use actions::forward_sources;
pub use actions::wire_forward_search;
pub use send_message::{refresh_send_notice, show_send_notice};

/// Wire chat callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.on_sender_profile_requested(move |value| {
        let Some(w) = weak.upgrade() else { return };
        let Ok(user_id) = value.parse::<chatter_core::domain::ids::UserId>() else {
            return;
        };
        #[cfg(feature = "mesh")]
        if crate::bin_ui::features::mesh::active().is_some() {
            crate::bin_ui::features::mesh::open_sender_sheet(&w, &client, &user_id);
            return;
        }
        super::modals::open_user_profile_in_conversation(
            &w,
            &user_id,
            Some(&client),
            client.active_conversation(),
        );
        super::back::push(&w, "user-profile");
    });
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
