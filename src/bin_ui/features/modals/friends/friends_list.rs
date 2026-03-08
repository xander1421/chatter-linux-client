//! Friends list actions: view profile, start DM, remove friend, unblock user.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_linux::app::UIContext;

use crate::MainWindow;

pub(crate) fn wire_view_profile(window: &MainWindow, _ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();

    window.on_friends_view_profile(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_id_str = user_id.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Close friends modal and open profile modal
                w.set_active_modal("user-profile".into());
                w.set_profile_user_id(user_id_str.clone().into());
                tracing::debug!("Opening profile for user {}", user_id_str);
            }
        });
    });
}

pub(crate) fn wire_start_dm(window: &MainWindow, _ui_ctx: &Arc<UIContext>) {
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

pub(crate) fn wire_remove_friend(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let user_service = ui_ctx.user_service.clone();

    window.on_friends_remove_friend(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_service = user_service.clone();
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

            match user_service.remove_friend(&friend_id).await {
                Ok(_) => {
                    tracing::info!("Removed friend {}", user_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to remove friend: {}", e);
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

pub(crate) fn wire_unblock_user(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let user_service = ui_ctx.user_service.clone();

    window.on_friends_unblock_user(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_service = user_service.clone();
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

            match user_service.unblock_user(&blocked_user_id).await {
                Ok(_) => {
                    tracing::info!("Unblocked user {}", user_id_str);
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
