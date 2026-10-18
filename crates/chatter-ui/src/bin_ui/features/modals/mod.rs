//! Modal callbacks.
//!
//! Handles modal dialogs (channel creation, profile actions, friend requests,
//! friends management, settings, sessions, join channel, channel settings).

mod anon_dm_request;
mod anon_group_invite;
mod channel_invites;
mod channel_settings;
mod create_channel;
mod forward;
pub(crate) mod friends;
mod join_channel;
pub mod link_approve;
// `pub` so the ui_wiring harness can wire ONLY the profile callbacks (incl.
// the username-link share wires) against a real offline ChatterClient.
pub mod profile;
mod sessions;
// `pub` so the VALIDATED-UI headless harness can wire ONLY the settings
// callbacks against a real offline ChatterClient, without invoking the full
// `wire_all` (which spawns a call-orchestrator subscriber that needs a live
// Slint event loop). Registration-only; no behavioural change to the app.
#[cfg(feature = "jarvis")]
pub mod local_ai;
pub mod settings;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub use anon_dm_request::spawn_anon_dm_request_handler;
pub use anon_group_invite::{
    refresh_group_invites, spawn_anon_group_invite_handler, wire_group_invite_callbacks,
};
pub use channel_invites::load_channel_invites;
pub use channel_settings::{
    apply_private_group_roster, open_channel_settings, open_invite_member_modal,
    open_private_group_invite_modal, refresh_members_if_open, wire_channel_settings,
};
pub(crate) use channel_settings::{
    present_settings_error, settings_open_allowed, settings_refresh_allowed,
    show_channel_settings_shell,
};
pub use forward::{target_avatar, wire_forward_message};
pub use friends::wire_friends_modal;
pub use profile::{open_user_profile, open_user_profile_in_conversation};

/// Wire modal callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    create_channel::wire_create_channel(window, ui_ctx);
    create_channel::wire_invite_to_private_group(window, ui_ctx);
    join_channel::wire_join_channel(window, ui_ctx);
    profile::wire_profile_actions(window, ui_ctx);
    forward::wire_forward_message(window, ui_ctx);
    friends::wire_friends_modal(window, ui_ctx);
    settings::wire_settings_callbacks(window, ui_ctx);
    #[cfg(feature = "jarvis")]
    local_ai::wire(window, ui_ctx);
    sessions::wire_session_callbacks(window, ui_ctx);
    channel_settings::wire_channel_settings(window, ui_ctx);
    anon_group_invite::wire_group_invite_callbacks(window, ui_ctx);
    channel_invites::wire_channel_invite_callbacks(window, ui_ctx);
    channel_invites::wire_channel_invites_clicked(window, ui_ctx);
    link_approve::wire(window, ui_ctx);
}
