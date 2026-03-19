//! Friend request actions: send, accept, decline, cancel.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::channel::ChannelService;
use chatter_core::services::user::{FriendRequestFilter, UserService};

use crate::bin_ui::converters::{get_initials, FriendRequestSendData};
use crate::{DMListItemData, FriendRequestData, MainWindow};

pub(crate) fn wire_send_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_send_request(move |user_id: SharedString| {
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

            match ui_ctx.send_friend_request(&friend_id, None).await {
                Ok(_) => {
                    tracing::info!("Friend request sent to {}", user_id_str);

                    // Refresh sent requests list
                    refresh_sent_requests(weak.clone(), ui_ctx.users().clone()).await;

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            // Refresh search results to show updated state
                            let query = w.get_friends_search_query().to_string();
                            if !query.is_empty() {
                                w.invoke_friends_search_users(query.into());
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to send friend request: {}", e);
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

/// Refresh the sent friend requests list in the UI.
pub(crate) async fn refresh_sent_requests(
    weak_window: slint::Weak<MainWindow>,
    user_service: Arc<UserService>,
) {
    match user_service
        .list_friend_requests(FriendRequestFilter::Sent, 50, 0)
        .await
    {
        Ok(friendships) => {
            // Fetch profiles for each friend request to get display names
            // Use Send-safe type to pass across thread boundary
            let mut sent_requests: Vec<FriendRequestSendData> = Vec::with_capacity(friendships.len());

            for f in friendships {
                let friend_id = f.friend_id;
                let friend_id_str = friend_id.to_string();

                // Try to fetch profile from server
                let (display_name, username, avatar_url) = match user_service.get_user_profile(&friend_id).await {
                    Ok(profile) => {
                        let name = if profile.display_name.is_empty() {
                            friend_id_str.clone()
                        } else {
                            profile.display_name
                        };
                        let avatar = profile.avatar_url.unwrap_or_default();
                        (name.clone(), name, avatar)
                    }
                    Err(e) => {
                        tracing::debug!("Failed to fetch profile for {}: {}", friend_id_str, e);
                        (friend_id_str.clone(), friend_id_str.clone(), String::new())
                    }
                };

                sent_requests.push(FriendRequestSendData {
                    user_id: friend_id_str,
                    display_name,
                    username,
                    avatar_url,
                    avatar_bytes: None, // Regular friends use avatar_url from server
                    avatar_decoded: None,
                    sent_at: String::new(),
                });
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    // Convert Send-safe data to Slint types inside event loop
                    let sent_slint: Vec<FriendRequestData> = sent_requests.into_iter().map(|r| r.to_slint()).collect();
                    w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_slint)));
                }
            });
        }
        Err(e) => {
            tracing::debug!("Failed to refresh sent friend requests: {}", e);
        }
    }
}

pub(crate) fn wire_accept_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_accept_request(move |user_id: SharedString| {
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

            match ui_ctx.respond_friend_request(&friend_id, true).await {
                Ok(result) => {
                    tracing::info!("Accepted friend request from {}", user_id_str);

                    // Check if we got a DM channel ID
                    if let Some(ref channel_id) = result.dm_channel_id {
                        tracing::info!("DM channel created for friendship: {}", channel_id);
                        // Add the DM to the sidebar using services
                        add_dm_to_sidebar(
                            weak.clone(),
                            ui_ctx.channels().clone(),
                            ui_ctx.users().clone(),
                            channel_id,
                            &user_id_str,
                        )
                        .await;
                    } else {
                        tracing::warn!("No channel_id received in friend accept response");
                    }

                    // Refresh friend requests list to remove the accepted request
                    refresh_received_requests(weak.clone(), ui_ctx.users().clone()).await;

                    let weak_done = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept friend request: {}", e);
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

/// Add a DM channel to the sidebar after friend request acceptance.
async fn add_dm_to_sidebar(
    weak_window: slint::Weak<MainWindow>,
    channel_service: Arc<ChannelService>,
    user_service: Arc<UserService>,
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

    // Fetch channel and sync memberships using ChannelService
    match channel_service
        .get_channel_with_memberships(&channel_id_parsed)
        .await
    {
        Ok(_channel) => {
            // Fetch the OTHER user's profile to get their display name using UserService
            let other_user_name = match user_service.get_user_profile(&other_user_id_parsed).await {
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
                    };
                    dm_vec.push(dm_item);

                    // Update UI
                    w.set_dm_conversations(ModelRc::new(VecModel::from(dm_vec)));
                    tracing::info!("Added DM to sidebar for user {} ({})", other_user_name, other_user_id_owned);
                }
            });
        }
        Err(e) => {
            tracing::error!("Failed to fetch DM channel {}: {}", channel_id, e);
        }
    }
}

/// Refresh the received friend requests list in the UI.
pub(crate) async fn refresh_received_requests(
    weak_window: slint::Weak<MainWindow>,
    user_service: Arc<UserService>,
) {
    match user_service
        .list_friend_requests(FriendRequestFilter::Received, 50, 0)
        .await
    {
        Ok(friendships) => {
            // Fetch profiles for each friend request to get display names
            // Use Send-safe type to pass across thread boundary
            let mut received_requests: Vec<FriendRequestSendData> = Vec::with_capacity(friendships.len());

            for f in friendships {
                let friend_id = f.friend_id;
                let friend_id_str = friend_id.to_string();

                // Try to fetch profile from server
                let (display_name, username, avatar_url) = match user_service.get_user_profile(&friend_id).await {
                    Ok(profile) => {
                        let name = if profile.display_name.is_empty() {
                            friend_id_str.clone()
                        } else {
                            profile.display_name
                        };
                        let avatar = profile.avatar_url.unwrap_or_default();
                        (name.clone(), name, avatar)
                    }
                    Err(e) => {
                        tracing::debug!("Failed to fetch profile for {}: {}", friend_id_str, e);
                        (friend_id_str.clone(), friend_id_str.clone(), String::new())
                    }
                };

                received_requests.push(FriendRequestSendData {
                    user_id: friend_id_str,
                    display_name,
                    username,
                    avatar_url,
                    avatar_bytes: None, // Regular friends use avatar_url from server
                    avatar_decoded: None,
                    sent_at: String::new(),
                });
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    // Convert Send-safe data to Slint types inside event loop
                    let received_slint: Vec<FriendRequestData> = received_requests.into_iter().map(|r| r.to_slint()).collect();
                    w.set_friends_received_requests(ModelRc::new(VecModel::from(received_slint)));
                }
            });
        }
        Err(e) => {
            tracing::debug!("Failed to refresh received friend requests: {}", e);
        }
    }
}

pub(crate) fn wire_decline_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_decline_request(move |user_id: SharedString| {
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

            match ui_ctx.respond_friend_request(&friend_id, false).await {
                Ok(_) => {
                    tracing::info!("Declined friend request from {}", user_id_str);

                    // Refresh received requests to remove the declined one
                    refresh_received_requests(weak.clone(), ui_ctx.users().clone()).await;

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to decline friend request: {}", e);
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

pub(crate) fn wire_cancel_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_cancel_request(move |user_id: SharedString| {
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

            // Cancelling a sent request is done via remove_friend
            match ui_ctx.remove_friend(&friend_id).await {
                Ok(_) => {
                    tracing::info!("Cancelled friend request to {}", user_id_str);

                    // Refresh sent requests to remove the cancelled one
                    refresh_sent_requests(weak.clone(), ui_ctx.users().clone()).await;

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to cancel friend request: {}", e);
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
