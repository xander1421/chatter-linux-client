//! Friends list actions: view profile, start DM, remove friend, unblock user.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub(crate) fn wire_view_profile(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let auth_state = ui_ctx.auth_state().clone();
    let user_state = ui_ctx.user_state().clone();

    window.on_friends_view_profile(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_id_str = user_id.to_string();
        let auth_state = auth_state.clone();
        let user_state = user_state.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                match user_id_str.parse::<chatter_core::domain::ids::UserId>() {
                    Ok(uid) => {
                        crate::bin_ui::features::modals::open_user_profile(
                            &w, &uid, &auth_state, &user_state,
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

pub(crate) fn wire_start_dm(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window.on_friends_start_dm(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_id_str = user_id.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Close friends modal and select DM
                w.set_active_modal("none".into());

                // Look up channel_id from user_id in DM conversations
                // This ensures selected_dm_id is always channel_id for consistency
                let dm_id = get_dm_channel_from_user(&w, &user_id_str)
                    .unwrap_or_else(|| user_id_str.clone());

                w.set_selected_dm_id(dm_id.clone().into());
                w.set_selected_channel_id("".into());
                tracing::debug!("Starting DM with user {} (channel {})", user_id_str, dm_id);
            }
        });
    });
}

/// Look up DM channel_id from user_id using the conversations model.
pub(crate) fn get_dm_channel_from_user(window: &MainWindow, user_id: &str) -> Option<String> {
    let conversations = window.get_dm_conversations();
    for i in 0..conversations.row_count() {
        if let Some(item) = conversations.row_data(i) {
            if item.user_id.as_str() == user_id {
                return Some(item.channel_id.to_string());
            }
        }
    }
    None
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

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                ui_ctx.unblock_user(&blocked_user_id),
            ).await {
                Ok(Ok(_)) => {
                    tracing::info!("Unblocked user {}", user_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to unblock user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Unblock user timed out");
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
