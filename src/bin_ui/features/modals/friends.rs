//! Friends modal callbacks.
//!
//! Handles the comprehensive friends management modal including search,
//! friend requests, and blocked users.
//!
//! Uses UserService for all user operations - no direct network calls.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::sync::Arc;

use chatter_linux::app::AppContext;
use chatter_linux::services::channel::ChannelService;
use chatter_linux::services::user_service::{FriendRequestFilter, UserService};
use std::collections::HashSet;

use crate::bin_ui::converters::get_initials;
use crate::{DMListItemData, MainWindow, UserSearchResult};

/// Wire friends modal callbacks.
pub fn wire_friends_modal(window: &MainWindow, ctx: &AppContext) {
    wire_search_users(window, ctx);
    wire_send_request(window, ctx);
    wire_accept_request(window, ctx);
    wire_decline_request(window, ctx);
    wire_cancel_request(window, ctx);
    wire_view_profile(window, ctx);
    wire_start_dm(window, ctx);
    wire_remove_friend(window, ctx);
    wire_unblock_user(window, ctx);
}

fn wire_search_users(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());

    window.on_friends_search_users(move |query: SharedString| {
        let weak = weak_window.clone();
        let user_service = user_service.clone();
        let query_str = query.to_string();

        if query_str.trim().is_empty() {
            return;
        }

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_search_loading(true);
                }
            });

            // Use UserService for all operations
            let (search_result, friends_result, requests_result) = tokio::join!(
                user_service.search_users(&query_str, 20, 0),
                user_service.list_friends(100, 0),
                user_service.list_friend_requests(FriendRequestFilter::Sent, 100, 0)
            );

            // Build friend ID set from friendships
            let friend_ids: HashSet<String> = friends_result
                .map(|friendships| {
                    friendships
                        .into_iter()
                        .map(|f| f.friend_id.to_string())
                        .collect()
                })
                .unwrap_or_default();

            // Build pending request ID set (outgoing requests we sent)
            let pending_ids: HashSet<String> = requests_result
                .map(|friendships| {
                    friendships
                        .into_iter()
                        .map(|f| f.friend_id.to_string())
                        .collect()
                })
                .unwrap_or_default();

            match search_result {
                Ok(profiles) => {
                    let results: Vec<UserSearchResult> = profiles
                        .into_iter()
                        .map(|p| {
                            let user_id_str = p.user_id.to_string();
                            let is_friend = friend_ids.contains(&user_id_str);
                            let request_pending = pending_ids.contains(&user_id_str);
                            UserSearchResult {
                                user_id: user_id_str.clone().into(),
                                display_name: p.display_name.into(),
                                username: user_id_str.into(),
                                avatar_url: p.avatar_url.unwrap_or_default().into(),
                                is_friend,
                                request_pending,
                            }
                        })
                        .collect();

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let model = Rc::new(VecModel::from(results));
                            w.set_friends_search_results(ModelRc::from(model));
                            w.set_friends_search_loading(false);
                            tracing::debug!("User search completed");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("User search failed: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_search_loading(false);
                        }
                    });
                }
            }
        });
    });
}

fn wire_send_request(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());

    window.on_friends_send_request(move |user_id: SharedString| {
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

            match user_service.send_friend_request(&friend_id, None).await {
                Ok(_) => {
                    tracing::info!("Friend request sent to {}", user_id_str);

                    // Refresh sent requests list
                    refresh_sent_requests(weak.clone(), user_service.clone()).await;

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
async fn refresh_sent_requests(weak_window: slint::Weak<MainWindow>, user_service: Arc<UserService>) {
    match user_service
        .list_friend_requests(FriendRequestFilter::Sent, 50, 0)
        .await
    {
        Ok(friendships) => {
            // Fetch profiles for each friend request to get display names
            let mut sent_requests: Vec<crate::FriendRequestData> = Vec::with_capacity(friendships.len());

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

                sent_requests.push(crate::FriendRequestData {
                    user_id: friend_id_str.into(),
                    display_name: display_name.into(),
                    username: username.into(),
                    avatar_url: avatar_url.into(),
                    sent_at: "".into(),
                });
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_requests)));
                }
            });
        }
        Err(e) => {
            tracing::debug!("Failed to refresh sent friend requests: {}", e);
        }
    }
}

fn wire_accept_request(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());
    let channel_service = Arc::new(ctx.channel_service());

    window.on_friends_accept_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let user_service = user_service.clone();
        let channel_service = channel_service.clone();
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

            match user_service.respond_friend_request(&friend_id, true).await {
                Ok(result) => {
                    tracing::info!("Accepted friend request from {}", user_id_str);

                    // Check if we got a DM channel ID
                    if let Some(ref channel_id) = result.dm_channel_id {
                        tracing::info!("DM channel created for friendship: {}", channel_id);
                        // Add the DM to the sidebar using services
                        add_dm_to_sidebar(
                            weak.clone(),
                            channel_service.clone(),
                            user_service.clone(),
                            channel_id,
                            &user_id_str,
                        )
                        .await;
                    } else {
                        tracing::warn!("No channel_id received in friend accept response");
                    }

                    // Refresh friend requests list to remove the accepted request
                    refresh_received_requests(weak.clone(), user_service.clone()).await;

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
    let channel_id_parsed: chatter_linux::domain::ids::ChannelId = match channel_id.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid channel ID {}: {}", channel_id, e);
            return;
        }
    };

    // Parse other user ID
    let other_user_id_parsed: chatter_linux::domain::UserId = match other_user_id.parse() {
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

            // Create DMListItemData and add to UI
            let other_user_id_owned = other_user_id.to_string();
            let channel_id_owned = channel_id.to_string();
            let dm_item = DMListItemData {
                channel_id: channel_id_owned.into(),
                user_id: other_user_id_owned.clone().into(),
                display_name: other_user_name.clone().into(),
                avatar_url: SharedString::default(),
                initials: get_initials(&other_user_name).into(),
                presence_status: "online".into(),
                last_message: SharedString::default(),
                timestamp: SharedString::default(),
                unread_count: 0,
            };

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
async fn refresh_received_requests(weak_window: slint::Weak<MainWindow>, user_service: Arc<UserService>) {
    match user_service
        .list_friend_requests(FriendRequestFilter::Received, 50, 0)
        .await
    {
        Ok(friendships) => {
            // Fetch profiles for each friend request to get display names
            let mut received_requests: Vec<crate::FriendRequestData> = Vec::with_capacity(friendships.len());

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

                received_requests.push(crate::FriendRequestData {
                    user_id: friend_id_str.into(),
                    display_name: display_name.into(),
                    username: username.into(),
                    avatar_url: avatar_url.into(),
                    sent_at: "".into(),
                });
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_window.upgrade() {
                    w.set_friends_received_requests(ModelRc::new(VecModel::from(received_requests)));
                }
            });
        }
        Err(e) => {
            tracing::debug!("Failed to refresh received friend requests: {}", e);
        }
    }
}

fn wire_decline_request(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());

    window.on_friends_decline_request(move |user_id: SharedString| {
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

            match user_service.respond_friend_request(&friend_id, false).await {
                Ok(_) => {
                    tracing::info!("Declined friend request from {}", user_id_str);

                    // Refresh received requests to remove the declined one
                    refresh_received_requests(weak.clone(), user_service.clone()).await;

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

fn wire_cancel_request(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());

    window.on_friends_cancel_request(move |user_id: SharedString| {
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

            // Cancelling a sent request is done via remove_friend
            match user_service.remove_friend(&friend_id).await {
                Ok(_) => {
                    tracing::info!("Cancelled friend request to {}", user_id_str);

                    // Refresh sent requests to remove the cancelled one
                    refresh_sent_requests(weak.clone(), user_service.clone()).await;

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

fn wire_view_profile(window: &MainWindow, _ctx: &AppContext) {
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

fn wire_start_dm(window: &MainWindow, _ctx: &AppContext) {
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
fn get_dm_channel_from_user(window: &MainWindow, user_id: &str) -> Option<String> {
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

fn wire_remove_friend(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());

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

fn wire_unblock_user(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let user_service = Arc::new(ctx.user_service());

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
