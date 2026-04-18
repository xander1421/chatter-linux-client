//! Friend requests modal callbacks.
//!
//! Uses UserService for all friend request operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use crate::bin_ui::converters::get_initials;
use crate::{DMListItemData, MainWindow};

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
                Ok(result) => {
                    tracing::info!("Accepted friend request from {}", friend_id);

                    // If we got a DM channel ID, add it to the sidebar
                    if let Some(channel_id) = result.dm_channel_id {
                        add_dm_to_sidebar_on_accept(
                            weak.clone(),
                            ui_ctx.clone(),
                            &channel_id,
                            &user_id_str,
                        )
                        .await;
                    }

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

/// Add a DM channel to the sidebar after friend request acceptance.
async fn add_dm_to_sidebar_on_accept(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: &str,
    other_user_id: &str,
) {
    // Parse channel ID
    let channel_id_parsed: chatter_core::domain::ids::ChannelId = match channel_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid channel ID {}: {}", channel_id, e);
            return;
        }
    };

    // Parse other user ID
    let other_user_id_parsed: chatter_core::domain::UserId = match other_user_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid user ID {}: {}", other_user_id, e);
            return;
        }
    };

    // Fetch channel and sync memberships
    match ui_ctx
        .get_channel_with_memberships(&channel_id_parsed)
        .await
    {
        Ok(_channel) => {
            // Fetch the OTHER user's profile to get their display name
            let other_user_name = match ui_ctx.get_user_profile(&other_user_id_parsed).await {
                Ok(profile) => {
                    if profile.display_name.is_empty() {
                        other_user_id.to_string()
                    } else {
                        profile.display_name.clone()
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch user profile for {}: {}", other_user_id, e);
                    // Fallback to user ID if we can't get the profile
                    other_user_id.to_string()
                }
            };

            // Create DMListItemData inside event loop (slint::Image is not Send)
            let other_user_id_owned = other_user_id.to_string();
            let channel_id_owned = channel_id.to_string();
            let initials = get_initials(&other_user_name);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    // Get existing DM list and append new DM
                    let current_dms = w.get_dm_conversations();
                    let mut dm_vec: Vec<DMListItemData> = Vec::new();

                    // Copy existing DMs
                    for i in 0..current_dms.row_count() {
                        if let Some(dm) = current_dms.row_data(i) {
                            dm_vec.push(dm);
                        }
                    }

                    // Add new DM
                    let dm_item = DMListItemData {
                        channel_id: channel_id_owned.into(),
                        user_id: other_user_id_owned.clone().into(),
                        display_name: other_user_name.clone().into(),
                        avatar_url: SharedString::default(),
                        avatar_source: slint::Image::default(),
                        has_avatar: false,
                        initials: initials.into(),
                        presence_status: "online".into(),
                        last_message: SharedString::default(),
                        timestamp: SharedString::default(),
                        unread_count: 0,
                        is_typing: false,
                        is_anon: false,
                        is_selected: false,
                    };
                    dm_vec.push(dm_item);

                    // Update UI
                    w.set_dm_conversations(ModelRc::new(VecModel::from(dm_vec)));
                    tracing::info!(
                        "Added DM to sidebar for user {} ({})",
                        other_user_name,
                        other_user_id_owned
                    );
                }
            });
        }
        Err(e) => {
            tracing::error!("Failed to fetch DM channel {}: {}", channel_id, e);
        }
    }
}
