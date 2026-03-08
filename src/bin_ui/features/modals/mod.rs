//! Modal callbacks.
//!
//! Handles modal dialogs (channel creation, profile actions, friend requests,
//! friends management, settings, sessions, join channel, channel settings).

mod anon_dm_request;
mod anon_group_invite;
mod channel_settings;
mod create_channel;
mod friend_requests;
mod friends;
mod join_channel;
mod profile;
mod sessions;
mod settings;

use std::sync::Arc;

use chatter_linux::app::UIContext;

use crate::MainWindow;

pub use anon_dm_request::spawn_anon_dm_request_handler;
pub use anon_group_invite::spawn_anon_group_invite_handler;
pub use channel_settings::open_channel_settings;

/// Wire modal callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    create_channel::wire_create_channel(window, ui_ctx);
    join_channel::wire_join_channel(window, ui_ctx);
    profile::wire_profile_actions(window, ui_ctx);
    friend_requests::wire_friend_requests(window, ui_ctx);
    friends::wire_friends_modal(window, ui_ctx);
    settings::wire_settings_callbacks(window, ui_ctx);
    sessions::wire_session_callbacks(window, ui_ctx);
    channel_settings::wire_channel_settings(window, ui_ctx);
}
