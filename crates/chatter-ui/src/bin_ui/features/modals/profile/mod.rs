//! Profile modal callbacks.
//!
//! Uses UserService for all profile operations - no direct network calls.

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use slint::ComponentHandle;
use std::sync::Arc;

mod avatar_edit;
mod navigation;
mod open;
mod relationship;
mod safety;
mod share_link;
mod shared;

pub use open::{open_user_profile, open_user_profile_in_conversation};
use share_link::{set_share_link_props, wire_share_link};
use shared::{SHARED_PAGE, dm_conversation, load_shared_tabs, shared_sender, wire_shared_content};

pub(crate) fn retire_avatar_edit() {
    avatar_edit::retire();
}

pub fn wire_profile_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_share_link(window, ui_ctx);
    wire_shared_content(window, ui_ctx);
    navigation::wire(window, ui_ctx);
    relationship::wire(window, ui_ctx);
    navigation::wire_message(window, ui_ctx);
    avatar_edit::wire(window, ui_ctx);
    safety::wire(window, ui_ctx);
}
