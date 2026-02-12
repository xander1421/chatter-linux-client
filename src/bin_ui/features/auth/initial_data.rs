//! Initial data loading after login.

use std::sync::Arc;

use slint::{ModelRc, VecModel};

use chatter_linux::domain::channel::ChannelType;
use chatter_linux::mailbox::group::GroupCoordinator;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::network::grpc::user_client::UserClient;
use chatter_linux::proto::chat_v1::friend_requests_filter;
use chatter_linux::services::channel::ChannelService;
use chatter_linux::state::AuthState;
use chatter_linux::storage::{AttachmentRepository, ChannelRepository, MessageRepository, SettingsRepository, UserRepository};

use crate::bin_ui::converters::{channel_to_slint, dm_to_slint, get_initials};
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{ChannelListItemData, DMListItemData, FriendRequestData, MainWindow};

/// Load initial data after login or on startup.
pub async fn load_initial_data(
    window_weak: slint::Weak<MainWindow>,
    grpc_pool: Arc<GrpcClientPool>,
    channel_repo: Arc<ChannelRepository>,
    message_repo: Arc<MessageRepository>,
    user_repo: Arc<UserRepository>,
    attachment_repo: Arc<AttachmentRepository>,
    settings_repo: Arc<SettingsRepository>,
    signal_service: Arc<chatter_linux::services::signal_service::SignalService>,
    group_coordinator: Arc<GroupCoordinator>,
    auth_state: AuthState,
) {
    tracing::info!("Loading initial data...");

    // Create channel service and client
    let channel_service = ChannelService::new(settings_repo.clone(), channel_repo.clone(), signal_service, group_coordinator);
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    // Sync channels from server first (fetches and upserts to local storage)
    match channel_service
        .list_channels(&mut channel_client, 100, 0, None)
        .await
    {
        Ok(synced_channels) => {
            for ch in &synced_channels {
                tracing::debug!(
                    "Synced channel: id={}, name={}, type={:?}",
                    ch.id, ch.name, ch.channel_type
                );
            }
            tracing::info!("Synced {} channels from server", synced_channels.len());
        }
        Err(e) => {
            tracing::warn!("Failed to sync channels from server: {}", e);
        }
    }

    // Now get channels from local storage (which has been synced)
    let channels = match channel_service.get_channels_for_user() {
        Ok(ch) => ch,
        Err(e) => {
            tracing::warn!("Failed to load channels from local storage: {}", e);
            Vec::new()
        }
    };

    // Separate channels and DMs
    let mut channel_list: Vec<ChannelListItemData> = Vec::new();
    let mut dm_list: Vec<DMListItemData> = Vec::new();

    for channel in &channels {
        match channel.channel_type {
            ChannelType::DirectMessage => {
                let other_user_name = channel.name.clone();
                let other_user_id = channel.owner_id.to_string();
                dm_list.push(dm_to_slint(channel, &other_user_name, &other_user_id));
            }
            ChannelType::Public | ChannelType::Private => {
                channel_list.push(channel_to_slint(channel));
            }
            ChannelType::SavedMessages => {
                // Saved messages has its own sidebar button, not listed with channels
            }
        }
    }

    // Get user profile info
    let user_display_name = auth_state.get().display_name.clone().unwrap_or_default();
    let user_initials = get_initials(&user_display_name);

    // Determine first channel to select (exclude DMs and SavedMessages)
    let first_channel_id = channels
        .iter()
        .find(|c| {
            !matches!(
                c.channel_type,
                ChannelType::DirectMessage | ChannelType::SavedMessages
            )
        })
        .map(|c| c.id);

    // Clone for loading
    let first_channel_id_for_load = first_channel_id;
    let channel_repo_for_load = channel_repo.clone();
    let message_repo_for_load = message_repo.clone();
    let user_repo_for_load = user_repo.clone();
    let attachment_repo_for_load = attachment_repo.clone();

    // Fetch friend requests from server
    let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    let received_requests = match user_client
        .list_friend_requests(friend_requests_filter::Filter::Received, 50, 0)
        .await
    {
        Ok(response) => {
            tracing::info!("Fetched {} received friend requests", response.requests.len());
            response.requests
        }
        Err(e) => {
            tracing::warn!("Failed to fetch received friend requests: {}", e);
            Vec::new()
        }
    };

    let sent_requests = match user_client
        .list_friend_requests(friend_requests_filter::Filter::Sent, 50, 0)
        .await
    {
        Ok(response) => {
            tracing::info!("Fetched {} sent friend requests", response.requests.len());
            response.requests
        }
        Err(e) => {
            tracing::warn!("Failed to fetch sent friend requests: {}", e);
            Vec::new()
        }
    };

    // Convert to UI types
    let received_ui: Vec<FriendRequestData> = received_requests
        .into_iter()
        .map(|r| FriendRequestData {
            user_id: r.user_id.into(),
            display_name: r.display_name.into(),
            username: r.username.into(),
            avatar_url: r.avatar_url.into(),
            sent_at: "".into(), // Server doesn't provide timestamp currently
        })
        .collect();

    let sent_ui: Vec<FriendRequestData> = sent_requests
        .into_iter()
        .map(|r| FriendRequestData {
            user_id: r.user_id.into(),
            display_name: r.display_name.into(),
            username: r.username.into(),
            avatar_url: r.avatar_url.into(),
            sent_at: "".into(),
        })
        .collect();

    // Update UI with channels, DMs, and friend requests
    let window_weak_ui = window_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak_ui.upgrade() {
            w.set_channels(ModelRc::new(VecModel::from(channel_list)));
            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list)));
            w.set_user_display_name(user_display_name.into());
            w.set_user_initials(user_initials.into());
            w.set_user_presence_status("online".into());

            // Set friend requests
            w.set_friends_received_requests(ModelRc::new(VecModel::from(received_ui)));
            w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_ui)));
        }
    });

    // If there's a channel, select it and load messages
    if let Some(channel_id) = first_channel_id_for_load {
        let channel_id_str = channel_id.to_string();

        let window_weak_select = window_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window_weak_select.upgrade() {
                w.set_selected_channel_id(channel_id_str.into());
            }
        });

        load_channel_into_ui(
            window_weak,
            &channel_id,
            channel_repo_for_load,
            message_repo_for_load,
            user_repo_for_load,
            attachment_repo_for_load,
        )
        .await;
    }

    tracing::info!("Initial data load complete");
}
