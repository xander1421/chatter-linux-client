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

pub(crate) use private_requests::{BlockedUserRow, blocked_user_rows, refresh_blocked_users};

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use slint::ComponentHandle;

use crate::MainWindow;

/// Wire friends modal callbacks.
pub fn wire_friends_modal(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Tab switch → refresh that tab's data (push events keep lists fresh too;
    // this covers anything missed while the modal was closed).
    wire_switch_tab(window, ui_ctx);

    // Friend requests (accept, decline, cancel)
    requests::wire_accept_request(window, ui_ctx);
    requests::wire_decline_request(window, ui_ctx);
    requests::wire_cancel_request(window, ui_ctx);

    // Friends list actions
    friends_list::wire_view_profile(window, ui_ctx);
    friends_list::wire_start_dm(window, ui_ctx);
    friends_list::wire_unblock_user(window, ui_ctx);
    friends_list::wire_remove_blocked_user(window, ui_ctx);

    // Privacy-first friend discovery
    friend_code::wire_generate_friend_code(window, ui_ctx);
    friend_code::wire_connect_via_friend_code(window, ui_ctx);

    // Anonymous DM (private) request actions
    private_requests::wire_accept_private_request(window, ui_ctx);
    private_requests::wire_decline_private_request(window, ui_ctx);
    private_requests::wire_block_private_request(window, ui_ctx);
}

fn wire_switch_tab(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_friends_modal_switch_tab(move |tab| {
        match tab.as_str() {
            "friends" => {
                crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                    weak.clone(),
                    ui_ctx.clone(),
                );
            }
            "requests" => {
                let ui_ctx = ui_ctx.clone();
                tokio::spawn(async move {
                    // Emits FriendRequestsUpdated; the domain handler rebuilds
                    // the Received/Sent rows.
                    ui_ctx.refresh_friend_requests().await;
                });
            }
            "blocked" => {
                private_requests::refresh_blocked_users(weak.clone(), &ui_ctx);
            }
            "find" => {
                let Some(w) = weak.upgrade() else { return };
                let private_mode = ui_ctx.anon_mode_enabled();
                match ui_ctx.current_friend_code(private_mode) {
                    Ok(code) => w.set_friends_my_code(code.into()),
                    Err(error) => {
                        w.set_friends_my_code("".into());
                        tracing::warn!("Could not load current friend code: {}", error);
                    }
                }
                w.set_friends_show_qr(false);
            }
            _ => {}
        }
    });
}
