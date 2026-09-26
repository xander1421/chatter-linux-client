//! Friends list actions: view profile, start DM, remove friend, and the
//! Blocked tab's unblock / remove.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub(crate) fn wire_view_profile(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let auth_state = ui_ctx.auth_state().clone();
    let user_state = ui_ctx.user_state().clone();
    let ui_ctx_profile = ui_ctx.clone();

    window.on_friends_view_profile(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_id_str = user_id.to_string();
        let auth_state = auth_state.clone();
        let user_state = user_state.clone();
        let ui_ctx = ui_ctx_profile.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                match user_id_str.parse::<chatter_core::domain::ids::UserId>() {
                    Ok(uid) => {
                        crate::bin_ui::features::modals::open_user_profile(
                            &w, &uid, &auth_state, &user_state, Some(&ui_ctx),
                        );
                        crate::bin_ui::features::back::push(&w, "user-profile");
                    }
                    Err(e) => {
                        tracing::error!("Invalid user ID in view profile: {}", e);
                    }
                }
            }
        });
    });
}

pub(crate) fn wire_start_dm(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_start_dm(move |user_id: SharedString| {
        let _ = &ui_ctx;
        let weak = weak_window.clone();
        let user_id_str = user_id.to_string();
        // DMs are channel-free — the conversation key is the peer's user_id.
        let dm_id = user_id_str.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                crate::bin_ui::features::back::reset_to_chats(&w);
                w.invoke_dm_selected(dm_id.clone().into());
                tracing::debug!("Starting DM with user {}", user_id_str);
            }
        });
    });
}

pub(crate) fn wire_unblock_user(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_friends_unblock_user(move |user_id: SharedString| {
        run_blocked_action(weak_window.clone(), ui_ctx.clone(), user_id.to_string(), false);
    });
}

pub(crate) fn wire_remove_blocked_user(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_friends_remove_blocked_user(move |user_id: SharedString| {
        run_blocked_action(weak_window.clone(), ui_ctx.clone(), user_id.to_string(), true);
    });
}

/// Unblock (or, with `remove`, unfriend and unblock) from Friends › Blocked,
/// then refresh every list a block changes: the Blocked tab, the Friends tab
/// and the DM sidebar (an unblocked friend's DM comes back with its history).
fn run_blocked_action(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    user_id_str: String,
    remove: bool,
) {
    if let Some(w) = weak.upgrade() {
        w.set_friends_loading(true);
    }
    tokio::spawn(async move {
        match user_id_str.parse() {
            Ok(id) => {
                let result = if remove {
                    ui_ctx.remove_blocked_contact(&id).await
                } else {
                    ui_ctx.unblock_profile_user(&id).await
                };
                match result {
                    Ok(()) => {
                        crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                            weak.clone(), ui_ctx.clone(),
                        );
                        super::refresh_blocked_users(weak.clone(), &ui_ctx);
                    }
                    Err(e) => tracing::error!("Blocked-user action failed: {}", e),
                }
            }
            Err(_) => tracing::error!("Invalid user ID: {}", user_id_str),
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_friends_loading(false);
                crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            }
        });
    });
}
