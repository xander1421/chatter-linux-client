//! Friends list actions: view profile, start DM, remove friend, unblock user.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::{MainWindow, UserSearchResult};

/// Wire the friends-modal user search (F-UI-3).
///
/// Previously a dead wire: `friends-search-users` was emitted with NO Rust
/// handler and `friends-search-results` was never populated. Now each keystroke
/// runs `ChatterClient::search_users` (exact `nickname.discriminator` lookup —
/// the only privacy-preserving discovery, matching the invite-member search) and
/// populates the bound results list.
pub(crate) fn wire_search_users(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_search_users(move |query: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let query = query.to_string();
        let has_query = !query.trim().is_empty();

        // Show loading immediately for a real query; resolve off the UI thread.
        let weak_loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_loading.upgrade() {
                w.set_friends_search_loading(has_query);
            }
        });

        tokio::spawn(async move {
            let hits = ui_ctx.search_users(&query).await.unwrap_or_default();
            let rows: Vec<UserSearchResult> = hits
                .into_iter()
                .map(|h| UserSearchResult {
                    user_id: h.user_id.to_string().into(),
                    display_name: h.display_name.into(),
                    username: h.username.into(),
                    is_friend: h.is_friend,
                    request_pending: false,
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_friends_search_results(ModelRc::new(VecModel::from(rows)));
                    w.set_friends_search_loading(false);
                }
            });
        });
    });
}

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
                w.set_active_modal("none".into());
                w.invoke_dm_selected(dm_id.clone().into());
                tracing::debug!("Starting DM with user {}", user_id_str);
            }
        });
    });
}

pub(crate) fn wire_remove_friend(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_remove_friend(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            // Parse user ID
            let friend_id = match user_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid user ID: {}", user_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                ui_ctx.remove_friend(&friend_id),
            ).await {
                Ok(Ok(_)) => {
                    tracing::info!("Removed friend {}", user_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to remove friend: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Remove friend timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
            }
        });
    });
}

pub(crate) fn wire_unblock_user(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_unblock_user(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            // Parse user ID
            let blocked_user_id = match user_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid user ID: {}", user_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.unblock_profile_user(&blocked_user_id).await {
                Ok(()) => {
                    tracing::info!("Unblocked user {}", user_id_str);
                    crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                        weak.clone(), ui_ctx.clone(),
                    );
                    super::refresh_blocked_users(weak.clone(), &ui_ctx);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to unblock user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
            }
        });
    });
}
