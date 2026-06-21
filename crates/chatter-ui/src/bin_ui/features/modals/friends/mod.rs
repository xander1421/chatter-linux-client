//! Friends modal callbacks.
//!
//! Handles the comprehensive friends management modal including friend codes,
//! friend requests, and blocked users.
//!
//! NOTE: Username search has been removed for privacy. Users must use
//! friend codes (chatter://connect/{user_id}) to connect.
//!
//! Uses UserService for all user operations - no direct network calls.

mod friend_code;
mod friends_list;
mod private_requests;
mod requests;

pub(crate) use private_requests::refresh_blocked_users;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire friends modal callbacks.
pub fn wire_friends_modal(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // NOTE: wire_search_users removed - username search disabled for privacy
    // Users must use friend codes for discovery

    // Friend requests (send, accept, decline, cancel)
    requests::wire_send_request(window, ui_ctx);
    requests::wire_accept_request(window, ui_ctx);
    requests::wire_decline_request(window, ui_ctx);
    requests::wire_cancel_request(window, ui_ctx);

    // Friends list actions
    friends_list::wire_view_profile(window, ui_ctx);
    friends_list::wire_start_dm(window, ui_ctx);
    friends_list::wire_remove_friend(window, ui_ctx);
    friends_list::wire_unblock_user(window, ui_ctx);

    // Privacy-first friend discovery
    friend_code::wire_generate_friend_code(window, ui_ctx);
    friend_code::wire_connect_via_friend_code(window, ui_ctx);
    friend_code::wire_connect_private(window, ui_ctx);

    // Anonymous DM (private) request actions
    private_requests::wire_accept_private_request(window, ui_ctx);
    private_requests::wire_decline_private_request(window, ui_ctx);
    private_requests::wire_block_private_request(window, ui_ctx);
}
