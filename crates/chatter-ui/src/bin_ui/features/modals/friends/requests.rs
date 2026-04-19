//! Friend request actions: send, accept, decline, cancel.
//!
//! Thin glue — core emits `DomainEvent::FriendRequestsUpdated` after every
//! send / respond / remove, and the sidebar domain-handler dispatcher
//! refreshes the lists. UI here only toggles the loading spinner and
//! handles the accept-side DM sidebar insertion.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::bin_ui::converters::get_initials;
use crate::{DMListItemData, MainWindow};

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
                Ok(result) => {
                    tracing::info!("Accepted friend request from {}", user_id_str);
                    if let Some(ref channel_id) = result.dm_channel_id {
                        add_dm_to_sidebar(weak.clone(), ui_ctx.clone(), channel_id, &user_id_str).await;
                    } else {
                        tracing::warn!("No channel_id received in friend accept response");
                    }
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

/// Add a DM channel to the sidebar after friend request acceptance.
async fn add_dm_to_sidebar(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: &str,
    other_user_id: &str,
) {
    let channel_id_parsed: chatter_core::domain::ids::ChannelId = match channel_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid channel ID {}: {}", channel_id, e);
            return;
        }
    };
    let other_user_id_parsed: chatter_core::domain::UserId = match other_user_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid user ID {}: {}", other_user_id, e);
            return;
        }
    };

    match ui_ctx.get_channel_with_memberships(&channel_id_parsed).await {
        Ok(_channel) => {
            let other_user_name = match ui_ctx.get_user_profile(&other_user_id_parsed).await {
                Ok(profile) if !profile.display_name.is_empty() => profile.display_name,
                _ => other_user_id.to_string(),
            };

            let other_user_id_owned = other_user_id.to_string();
            let channel_id_owned = channel_id.to_string();
            let initials = get_initials(&other_user_name);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    let current_dms = w.get_dm_conversations();
                    let mut dm_vec: Vec<DMListItemData> = Vec::new();
                    for i in 0..current_dms.row_count() {
                        if let Some(dm) = current_dms.row_data(i) {
                            dm_vec.push(dm);
                        }
                    }
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
                    w.set_dm_conversations(ModelRc::new(VecModel::from(dm_vec)));
                    tracing::info!(
                        "Added DM to sidebar for user {} ({})",
                        other_user_name, other_user_id_owned
                    );
                }
            });
        }
        Err(e) => {
            tracing::error!("Failed to fetch DM channel {}: {}", channel_id, e);
        }
    }
}
