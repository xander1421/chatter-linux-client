//! Friends modal callbacks.
//!
//! Handles the comprehensive friends management modal including search,
//! friend requests, and blocked users.

use futures::StreamExt;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::sync::Arc;

use chatter_linux::app::AppContext;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::network::grpc::user_client::UserClient;
use chatter_linux::proto::chat_v1::{friend_requests_filter, user_event};
use chatter_linux::services::channel::converters::proto_to_domain_channel;
use chatter_linux::storage::ChannelRepository;
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
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_friends_search_users(move |query: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
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

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            // First, search users
            let search_result = user_client.search_users(&query_str, 20, 0).await;

            // Then fetch friend IDs and pending requests to check status
            let mut friends_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            let mut requests_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            let (friends_result, requests_result) = tokio::join!(
                friends_client.list_friends(100, 0),
                requests_client.list_friend_requests(friend_requests_filter::Filter::Sent, 100, 0)
            );

            // Build friend ID set
            let friend_ids: HashSet<String> = friends_result
                .map(|r| r.friends.into_iter().map(|f| f.user_id).collect())
                .unwrap_or_default();

            // Build pending request ID set (outgoing requests we sent)
            let pending_ids: HashSet<String> = requests_result
                .map(|r| r.requests.into_iter().map(|req| req.user_id).collect())
                .unwrap_or_default();

            match search_result {
                Ok(response) => {
                    let results: Vec<UserSearchResult> = response
                        .results
                        .into_iter()
                        .map(|u| {
                            let is_friend = friend_ids.contains(&u.user_id);
                            let request_pending = pending_ids.contains(&u.user_id);
                            UserSearchResult {
                                user_id: u.user_id.clone().into(),
                                display_name: u.display_name.clone().into(),
                                username: u.user_id.clone().into(),
                                avatar_url: u.avatar_url.into(),
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
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_friends_send_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            match user_client.send_friend_request(&user_id_str, None).await {
                Ok(stream) => {
                    // Drain the response stream to complete the request
                    tokio::pin!(stream);
                    while let Some(_) = stream.next().await {}

                    tracing::info!("Friend request sent to {}", user_id_str);

                    // Refresh sent requests list
                    let weak_refresh = weak.clone();
                    let grpc_pool_refresh = grpc_pool.clone();
                    refresh_sent_requests(weak_refresh, grpc_pool_refresh).await;

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
async fn refresh_sent_requests(weak_window: slint::Weak<MainWindow>, grpc_pool: Arc<GrpcClientPool>) {
    use chatter_linux::proto::chat_v1::friend_requests_filter;

    let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    match user_client
        .list_friend_requests(friend_requests_filter::Filter::Sent, 50, 0)
        .await
    {
        Ok(response) => {
            let sent_requests: Vec<crate::FriendRequestData> = response
                .requests
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    sent_at: "".into(),
                })
                .collect();

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
    let grpc_pool = ctx.grpc_pool.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_friends_accept_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let channel_repo = channel_repo.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            match user_client
                .respond_friend_request(
                    &user_id_str,
                    chatter_linux::proto::chat_v1::FriendStatus::Accepted,
                    None,
                )
                .await
            {
                Ok(mut stream) => {
                    tracing::info!("Accepted friend request from {}", user_id_str);

                    let mut channel_id_found = None;

                    // Process stream to get the FriendStatusChanged event with channel_id
                    while let Some(event_result) = stream.next().await {
                        match event_result {
                            Ok(event) => {
                                tracing::debug!("Friend accept stream event: {:?}", event.event);
                                if let Some(user_event::Event::FriendStatusChanged(status)) = event.event {
                                    tracing::debug!(
                                        "FriendStatusChanged: user_id={}, status={:?}, channel_id='{}'",
                                        status.user_id, status.status, status.channel_id
                                    );
                                    if !status.channel_id.is_empty() {
                                        channel_id_found = Some(status.channel_id.clone());
                                        tracing::info!(
                                            "DM channel created for friendship: {}",
                                            status.channel_id
                                        );
                                        // Add the DM to the sidebar
                                        add_dm_to_sidebar(
                                            weak.clone(),
                                            grpc_pool.clone(),
                                            channel_repo.clone(),
                                            &status.channel_id,
                                            &user_id_str,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Error in friend response stream: {}", e);
                            }
                        }
                    }

                    if channel_id_found.is_none() {
                        tracing::warn!("No channel_id received in friend accept response stream");
                    }

                    // Refresh friend requests list to remove the accepted request
                    refresh_received_requests(weak.clone(), grpc_pool.clone()).await;

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
    grpc_pool: Arc<GrpcClientPool>,
    channel_repo: Arc<ChannelRepository>,
    channel_id: &str,
    other_user_id: &str,
) {
    // Fetch channel from server
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    match channel_client.get_channel(channel_id).await {
        Ok(proto_channel) => {
            // Convert proto to domain and save to local repo
            match proto_to_domain_channel(&proto_channel) {
                Ok(channel) => {
                    if let Err(e) = channel_repo.insert(&channel) {
                        tracing::warn!("Failed to save DM channel to local repo: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to convert proto channel: {}", e);
                }
            }

            // Create DMListItemData and add to UI
            let other_user_name = proto_channel.name.clone();
            let other_user_id_owned = other_user_id.to_string();
            let dm_item = DMListItemData {
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
                    tracing::info!("Added DM to sidebar for user {}", other_user_id_owned);
                }
            });
        }
        Err(e) => {
            tracing::error!("Failed to fetch DM channel {}: {}", channel_id, e);
        }
    }
}

/// Refresh the received friend requests list in the UI.
async fn refresh_received_requests(weak_window: slint::Weak<MainWindow>, grpc_pool: Arc<GrpcClientPool>) {
    use chatter_linux::proto::chat_v1::friend_requests_filter;

    let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    match user_client
        .list_friend_requests(friend_requests_filter::Filter::Received, 50, 0)
        .await
    {
        Ok(response) => {
            let received_requests: Vec<crate::FriendRequestData> = response
                .requests
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    sent_at: "".into(),
                })
                .collect();

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
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_friends_decline_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            match user_client
                .respond_friend_request(
                    &user_id_str,
                    chatter_linux::proto::chat_v1::FriendStatus::Declined,
                    None,
                )
                .await
            {
                Ok(stream) => {
                    // Drain the stream to complete the request
                    tokio::pin!(stream);
                    while let Some(_) = stream.next().await {}

                    tracing::info!("Declined friend request from {}", user_id_str);

                    // Refresh received requests to remove the declined one
                    refresh_received_requests(weak.clone(), grpc_pool.clone()).await;

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
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_friends_cancel_request(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            // Cancelling a sent request is done via remove_friend
            match user_client.remove_friend(&user_id_str).await {
                Ok(_) => {
                    tracing::info!("Cancelled friend request to {}", user_id_str);

                    // Refresh sent requests to remove the cancelled one
                    refresh_sent_requests(weak.clone(), grpc_pool.clone()).await;

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
                w.set_selected_dm_id(user_id_str.clone().into());
                w.set_selected_channel_id("".into());
                tracing::debug!("Starting DM with user {}", user_id_str);
            }
        });
    });
}

fn wire_remove_friend(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_friends_remove_friend(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            match user_client.remove_friend(&user_id_str).await {
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
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_friends_unblock_user(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            match user_client.unblock_user(&user_id_str).await {
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
