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
mod search;

pub(crate) use private_requests::{BlockedUserRow, blocked_user_rows, refresh_blocked_users, refresh_private_requests};

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::MainWindow;

/// Wire friends modal callbacks.
pub fn wire_friends_modal(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    window.global::<crate::ModalHostState>().on_new_chat_query_changed(move |query| {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::ModalHostState>().set_new_chat_query(query);
            refresh_new_chat_results(&w);
        }
    });
    // Tab switch → refresh that tab's data (push events keep lists fresh too;
    // this covers anything missed while the modal was closed).
    wire_switch_tab(window, ui_ctx);
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.global::<crate::ModalHostState>().on_friends_requests_retry(move || {
        refresh_requests(weak.clone(), client.clone());
    });

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

    // Username-first add (exact @handle search → send request)
    search::wire_search(window, ui_ctx);

    // Anonymous DM (private) request actions
    private_requests::wire_accept_private_request(window, ui_ctx);
    private_requests::wire_decline_private_request(window, ui_ctx);
    private_requests::wire_block_private_request(window, ui_ctx);
}

/// Apply core's search to the Contacts snapshot while retaining its decoded avatars.
pub(crate) fn refresh_new_chat_results(window: &MainWindow) {
    let host = window.global::<crate::ModalHostState>();
    let source = host.get_friends_list();
    let rows: Vec<_> = (0..source.row_count())
        .filter_map(|i| source.row_data(i))
        .map(|friend| chatter_core::ui::sidebar_data::FriendDataSendData {
            user_id: friend.user_id.to_string(),
            display_name: friend.display_name.to_string(),
            username: friend.username.to_string(),
            avatar_bytes: None,
            presence_status: friend.presence_status.to_string(),
            custom_status: friend.custom_status.to_string(),
            is_anon: friend.is_anon,
        })
        .collect();
    let matches = chatter_core::ui::sidebar_data::matching_friend_rows(
        &rows,
        host.get_new_chat_query().as_str(),
    );
    let selected: Vec<_> = matches
        .iter()
        .filter_map(|row| {
            (0..source.row_count())
                .filter_map(|i| source.row_data(i))
                .find(|friend| friend.user_id == row.user_id)
        })
        .collect();
    host.set_new_chat_friends(ModelRc::new(VecModel::from(selected)));
}

fn wire_switch_tab(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_friends_modal_switch_tab(move |tab| {
            match tab.as_str() {
                "friends" => {
                    crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                        weak.clone(),
                        ui_ctx.clone(),
                    );
                }
                "requests" => {
                    refresh_requests(weak.clone(), ui_ctx.clone());
                }
                "blocked" => {
                    let weak = weak.clone();
                    let ui_ctx = ui_ctx.clone();
                    std::thread::spawn(move || {
                        private_requests::refresh_blocked_users(weak, &ui_ctx);
                    });
                }
                "find" => {
                    if let Some(w) = weak.upgrade() {
                        let host = w.global::<crate::ModalHostState>();
                        host.set_friends_my_code("".into());
                        host.set_friends_show_qr(false);
                        host.set_friends_qr_code_image(slint::Image::default());
                    }
                    let weak = weak.clone();
                    let ui_ctx = ui_ctx.clone();
                    std::thread::spawn(move || {
                        let private_mode = ui_ctx.anon_mode_enabled();
                        let code = ui_ctx.current_friend_code(private_mode);
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(w) = weak.upgrade() else { return };
                            let host = w.global::<crate::ModalHostState>();
                            // A later Shield switch owns the currently displayed code.
                            if ui_ctx.anon_mode_enabled() != private_mode {
                                return;
                            }
                            match code {
                                Ok(code) => host.set_friends_my_code(code.into()),
                                Err(error) => {
                                    host.set_friends_my_code("".into());
                                    tracing::warn!("Could not load current friend code: {}", error);
                                }
                            }
                        });
                    });
                }
                _ => {}
            }
        });
}

fn refresh_requests(weak: slint::Weak<MainWindow>, client: Arc<ChatterClient>) {
    tokio::spawn(async move {
        private_requests::refresh_private_requests(weak.clone(), &client).await;
        // Emits FriendRequestsUpdated; the domain handler rebuilds the rows.
        let result = client.refresh_friend_requests().await;
        if let Err(error) = &result { tracing::warn!(%error, "Could not refresh friend requests"); }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.global::<crate::ModalHostState>()
                    .set_friends_requests_error(result.is_err());
            }
        });
    });
}
