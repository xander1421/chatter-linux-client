//! Friend request actions: send, accept, decline, cancel.
//!
//! Thin glue — core emits `DomainEvent::FriendRequestsUpdated` after every
//! send / respond / remove, and the sidebar domain-handler dispatcher
//! refreshes the lists. UI here only toggles the loading spinner and
//! handles the accept-side DM sidebar insertion.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub(crate) fn wire_send_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_send_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            set_loading(&weak, true);

            let friend_id = match user_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid user ID: {}", user_id_str);
                    set_loading(&weak, false);
                    return;
                }
            };

            match ui_ctx.send_friend_request(&friend_id, None).await {
                Ok(_) => {
                    tracing::info!("Friend request sent to {}", user_id_str);
                    // Core emits FriendRequestsUpdated; dispatcher updates list.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            let query = w.get_friends_search_query().to_string();
                            if !query.is_empty() {
                                w.invoke_friends_search_users(query.into());
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to send friend request: {}", e);
                    set_loading(&weak, false);
                }
            }
        });
    });
}

pub(crate) fn wire_accept_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_accept_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            set_loading(&weak, true);

            let friend_id = match user_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid user ID: {}", user_id_str);
                    set_loading(&weak, false);
                    return;
                }
            };

            match ui_ctx.respond_friend_request(&friend_id, true).await {
                Ok(_) => {
                    tracing::info!("Accepted friend request from {}", user_id_str);
                    // DMs are channel-free — the sidebar is built from the
                    // friends list, so a plain refresh picks up the new DM.
                    let weak_refresh = weak.clone();
                    let ui_ctx_refresh = ui_ctx.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_refresh.upgrade() {
                            crate::bin_ui::features::auth::initial_data::refresh_dm_list(&w, &ui_ctx_refresh);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept friend request: {}", e);
                }
            }
            set_loading(&weak, false);
        });
    });
}

pub(crate) fn wire_decline_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_decline_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            set_loading(&weak, true);

            let friend_id = match user_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid user ID: {}", user_id_str);
                    set_loading(&weak, false);
                    return;
                }
            };

            match ui_ctx.respond_friend_request(&friend_id, false).await {
                Ok(_) => tracing::info!("Declined friend request from {}", user_id_str),
                Err(e) => tracing::error!("Failed to decline friend request: {}", e),
            }
            set_loading(&weak, false);
        });
    });
}

pub(crate) fn wire_cancel_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_cancel_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            set_loading(&weak, true);

            let friend_id = match user_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid user ID: {}", user_id_str);
                    set_loading(&weak, false);
                    return;
                }
            };

            match ui_ctx.remove_friend(&friend_id).await {
                Ok(_) => tracing::info!("Cancelled friend request to {}", user_id_str),
                Err(e) => tracing::error!("Failed to cancel friend request: {}", e),
            }
            set_loading(&weak, false);
        });
    });
}

fn set_loading(weak: &slint::Weak<MainWindow>, loading: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_friends_loading(loading);
        }
    });
}

