//! Friend request actions: accept, decline, cancel.
//!
//! Thin glue — core emits `DomainEvent::FriendRequestsUpdated` after every
//! send / respond / remove, and the sidebar domain-handler dispatcher
//! refreshes the lists. UI here only toggles the loading spinner and
//! handles the accept-side DM sidebar insertion.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub(crate) fn wire_accept_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_friends_accept_request(move |user_id: SharedString| {
            if !begin_action(&weak_window) {
                return;
            }
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let user_id_str = user_id.to_string();

            tokio::spawn(async move {
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

                        // DMs are channel-free — the sidebar is built from the local
                        // friends list (refresh_chat_list). The Friends modal is
                        // server-backed, so re-fetch it too or the accepter's modal
                        // stays stale until reload.
                        crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                            weak.clone(),
                            ui_ctx.clone(),
                        );
                        let weak_refresh = weak.clone();
                        let ui_ctx_refresh = ui_ctx.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_refresh.upgrade() {
                                crate::bin_ui::features::sidebar::refresh_chat_list(
                                    &w,
                                    &ui_ctx_refresh,
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to accept friend request: {}", e);
                        report_failure(
                            &weak,
                            "The request was not accepted — it is still pending.",
                        );
                    }
                }
                set_loading(&weak, false);
            });
        });
}

pub(crate) fn wire_decline_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_friends_decline_request(move |user_id: SharedString| {
            if !begin_action(&weak_window) {
                return;
            }
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let user_id_str = user_id.to_string();

            tokio::spawn(async move {
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
                    Err(e) => {
                        tracing::error!("Failed to decline friend request: {}", e);
                        report_failure(
                            &weak,
                            "The request was not declined — it is still pending.",
                        );
                    }
                }
                set_loading(&weak, false);
            });
        });
}

pub(crate) fn wire_cancel_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_friends_cancel_request(move |user_id: SharedString| {
            if !begin_action(&weak_window) {
                return;
            }
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let user_id_str = user_id.to_string();

            tokio::spawn(async move {
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
                    Err(e) => {
                        tracing::error!("Failed to cancel friend request: {}", e);
                        report_failure(
                            &weak,
                            "The request was not cancelled — it is still pending.",
                        );
                    }
                }
                set_loading(&weak, false);
            });
        });
}

/// Put a failed friend action in front of the user.
///
/// Accepting or declining a request is usually the very first thing a new
/// account does, and every failure path here used to be a `tracing::error!` and
/// nothing else: the row stayed exactly as it was, so the only reading available
/// was "the button does not work". The toast is the app's existing error surface
/// (see `features::chat::send_message`); the haptic is there because on a phone
/// the toast can be behind the modal that is still open.
fn report_failure(weak: &slint::Weak<MainWindow>, message: &str) {
    let weak = weak.clone();
    let message = message.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        w.global::<crate::ModalHostState>()
            .set_friends_loading(false);
        w.set_toast_message(message.into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
        w.global::<crate::Haptics>().invoke_error();
    });
}

fn begin_action(weak: &slint::Weak<MainWindow>) -> bool {
    let Some(w) = weak.upgrade() else {
        return false;
    };
    let host = w.global::<crate::ModalHostState>();
    if host.get_friends_loading() {
        return false;
    }
    host.set_friends_loading(true);
    true
}

fn set_loading(weak: &slint::Weak<MainWindow>, loading: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::ModalHostState>()
                .set_friends_loading(loading);
        }
    });
}
