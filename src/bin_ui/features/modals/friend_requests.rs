//! Friend requests modal callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::UserId;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::services::channel::converters::proto_to_domain_channel;
use chatter_linux::services::user_service::UserService;
use chatter_linux::storage::ChannelRepository;

use crate::bin_ui::converters::get_initials;
use crate::{DMListItemData, MainWindow};

pub fn wire_friend_requests(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let user_repo = ctx.user_repo.clone();
    let channel_repo = ctx.channel_repo.clone();

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
    let grpc_pool_accept = grpc_pool.clone();
    let settings_repo_accept = settings_repo.clone();
    let user_repo_accept = user_repo.clone();
    let channel_repo_accept = channel_repo.clone();

    window.on_friend_request_accept(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_accept.clone();
        let settings_repo = settings_repo_accept.clone();
        let user_repo = user_repo_accept.clone();
        let channel_repo = channel_repo_accept.clone();
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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service
                .respond_friend_request(&mut user_client, &friend_id, true)
                .await
            {
                Ok(result) => {
                    tracing::info!("Accepted friend request from {}", friend_id);

                    // If we got a DM channel ID, add it to the sidebar
                    if let Some(channel_id) = result.dm_channel_id {
                        add_dm_to_sidebar_on_accept(
                            weak.clone(),
                            grpc_pool.clone(),
                            channel_repo.clone(),
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
    let grpc_pool_decline = grpc_pool.clone();
    let settings_repo_decline = settings_repo.clone();
    let user_repo_decline = user_repo.clone();

    window.on_friend_request_decline(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_decline.clone();
        let settings_repo = settings_repo_decline.clone();
        let user_repo = user_repo_decline.clone();
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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service
                .respond_friend_request(&mut user_client, &friend_id, false)
                .await
            {
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
    let grpc_pool_cancel = grpc_pool.clone();
    let settings_repo_cancel = settings_repo.clone();
    let user_repo_cancel = user_repo.clone();

    window.on_friend_request_cancel(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_cancel.clone();
        let settings_repo = settings_repo_cancel.clone();
        let user_repo = user_repo_cancel.clone();
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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.remove_friend(&mut user_client, &friend_id).await {
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
