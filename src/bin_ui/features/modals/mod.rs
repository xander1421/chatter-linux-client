//! Modal callbacks.
//!
//! Handles modal dialogs (channel creation, profile actions, friend requests,
//! friends management, settings, sessions, join channel, channel settings).

mod channel_settings;
mod create_channel;
mod friend_requests;
mod friends;
mod join_channel;
mod profile;
mod sessions;
mod settings;

use chatter_linux::app::AppContext;

use crate::MainWindow;

pub use channel_settings::open_channel_settings;

/// Wire modal callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    create_channel::wire_create_channel(window, ctx);
    join_channel::wire_join_channel(window, ctx);
    profile::wire_profile_actions(window, ctx);
    friend_requests::wire_friend_requests(window, ctx);
    friends::wire_friends_modal(window, ctx);
    settings::wire_settings_callbacks(window, ctx);
    sessions::wire_session_callbacks(window, ctx);
    channel_settings::wire_channel_settings(window, ctx);
}
