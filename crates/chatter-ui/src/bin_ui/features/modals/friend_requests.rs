//! Friend requests modal callbacks.
//!
//! Uses UserService for all friend request operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use crate::MainWindow;

pub fn wire_friend_requests(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Switch tab
    let weak_window = window.as_weak();
    window.on_friend_requests_switch_tab(move |tab: SharedString| {
        let weak = weak_window.clone();
        let tab_str = tab.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_friend_requests_tab(tab_str.into());
                tracing::debug!("Switched to friend requests tab");
            }
        });
    });

    // Accept friend request
    let weak_window = window.as_weak();
    let ui_ctx_accept = ui_ctx.clone();

    window.on_friend_request_accept(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_accept.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friend_requests_loading(true);
                }
            });

            let friend_id: UserId = match user_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.respond_friend_request(&friend_id, true).await {
                Ok(_) => {
                    tracing::info!("Accepted friend request from {}", friend_id);

                    // DMs are channel-free — the sidebar is built from the
                    // friends list, so a plain refresh picks up the new DM.
                    let weak_refresh = weak.clone();
                    let ui_ctx_refresh = ui_ctx.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_refresh.upgrade() {
                            crate::bin_ui::features::auth::initial_data::refresh_dm_list(&w, &ui_ctx_refresh);
                        }
                    });

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Decline friend request
    let weak_window = window.as_weak();
    let ui_ctx_decline = ui_ctx.clone();

    window.on_friend_request_decline(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_decline.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friend_requests_loading(true);
                }
            });

            let friend_id: UserId = match user_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.respond_friend_request(&friend_id, false).await {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                            tracing::info!("Declined friend request from {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to decline friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Cancel friend request
    let weak_window = window.as_weak();
    let ui_ctx_cancel = ui_ctx.clone();

    window.on_friend_request_cancel(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_cancel.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friend_requests_loading(true);
                }
            });

            let friend_id: UserId = match user_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.remove_friend(&friend_id).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                            tracing::info!("Cancelled friend request to {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to cancel friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Forward message
    let weak_window = window.as_weak();
    window.on_forward_message(move |target_id: SharedString| {
        let weak = weak_window.clone();
        let target_id_str = target_id.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_forward_loading(false);
                w.set_active_modal("".into());
                tracing::info!(
                    "Forward message to: {} (forwarding not yet implemented)",
                    target_id_str
                );
            }
        });
    });
}

