//! Initial data loading after login.
//!
//! Uses UIContext services for all operations - no direct repository access.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use chatter_linux::app::UIContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::services::user::FriendRequestFilter;

use crate::bin_ui::converters::{
    channel_to_slint, dm_to_send_data, get_initials, message_to_display_data,
    DMListItemSendData, FriendRequestSendData,
};
use crate::{ChannelListItemData, DMListItemData, FriendRequestData, MainWindow, PrivateGroupListItemData};

/// Load initial data after login or on startup.
pub async fn load_initial_data(window_weak: Weak<MainWindow>, ui_ctx: &Arc<UIContext>) {
    tracing::info!("Loading initial data...");

    // Sync channels from server first (fetches and upserts to local storage)
    match ui_ctx.channel_service.list_channels(100, 0, None).await {
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

    // Pre-fetch anonymous tokens for sealed sender messaging
    // This ensures tokens are available before the first SKDM distribution
    if let Err(e) = ui_ctx.prefetch_tokens(10).await {
        tracing::warn!("Failed to pre-fetch anonymous tokens: {}", e);
    }

    // Now get channels from local storage (which has been synced)
    let channels = match ui_ctx.channel_service.get_channels_for_user() {
        Ok(ch) => ch,
        Err(e) => {
            tracing::error!(
                "Failed to load channels from local storage: {} - showing empty channel list",
                e
            );
            Vec::new()
        }
    };

    // Get current user ID for DM peer detection
    let current_user_id = ui_ctx.auth_state.get().current_user_id;

    // Separate channels and DMs (using Send-safe types for DMs)
    let mut channel_list: Vec<ChannelListItemData> = Vec::new();
    let mut dm_list: Vec<DMListItemSendData> = Vec::new();

    for channel in &channels {
        match channel.channel_type {
            ChannelType::DirectMessage => {
                // For DMs, we need to find the OTHER user (not ourselves)
                let other_user_id = if let Some(current_id) = current_user_id {
                    // Get channel members and find the one that's not us
                    match ui_ctx.channel_service.list_members_local(&channel.id) {
                        Ok(members) => members
                            .iter()
                            .find(|m| m.user_id != current_id)
                            .map(|m| m.user_id.to_string())
                            .unwrap_or_else(|| channel.owner_id.to_string()),
                        Err(_) => channel.owner_id.to_string(),
                    }
                } else {
                    // Fallback to owner_id if we don't know our own ID
                    channel.owner_id.to_string()
                };

                let other_user_name = channel.name.clone();

                // Get the most recent message for this DM to populate preview
                let (last_message, timestamp) =
                    match ui_ctx.message_service.get_messages_local(&channel.id, None, 1) {
                        Ok(messages) if !messages.is_empty() => {
                            let msg = &messages[0];
                            let is_own = current_user_id.map(|id| msg.sender_id == Some(id)).unwrap_or(false);
                            
                            let preview = msg
                                .as_text()
                                .map(|text| {
                                    let mut display_text = text.to_string();
                                    if display_text.chars().count() > 50 {
                                        let truncated: String = display_text.chars().take(47).collect();
                                        display_text = format!("{}...", truncated);
                                    }
                                    
                                    if is_own {
                                        format!("You: {}", display_text)
                                    } else {
                                        display_text
                                    }
                                })
                                .unwrap_or_default();
                            // Convert UTC to local time for display
                            let ts = msg
                                .created_at
                                .with_timezone(&chrono::Local)
                                .format("%H:%M")
                                .to_string();
                            (preview, ts)
                        }
                        _ => (String::new(), String::new()),
                    };

                let mut dm_item = dm_to_send_data(channel, &other_user_name, &other_user_id);
                dm_item.last_message = last_message;
                dm_item.timestamp = timestamp;
                dm_list.push(dm_item);
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
    let user_display_name = ui_ctx
        .auth_state
        .get()
        .display_name
        .clone()
        .unwrap_or_default();
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

    // Fetch friend requests from server using UserService
    let received_requests = match ui_ctx
        .user_service
        .list_friend_requests(FriendRequestFilter::Received, 50, 0)
        .await
    {
        Ok(friendships) => {
            tracing::info!("Fetched {} received friend requests", friendships.len());
            friendships
        }
        Err(e) => {
            tracing::warn!("Failed to fetch received friend requests: {}", e);
            Vec::new()
        }
    };

    let sent_requests = match ui_ctx
        .user_service
        .list_friend_requests(FriendRequestFilter::Sent, 50, 0)
        .await
    {
        Ok(friendships) => {
            tracing::info!("Fetched {} sent friend requests", friendships.len());
            friendships
        }
        Err(e) => {
            tracing::warn!("Failed to fetch sent friend requests: {}", e);
            Vec::new()
        }
    };

    // Convert to Send-safe UI types, fetching profiles for display names
    let mut received_ui: Vec<FriendRequestSendData> = Vec::with_capacity(received_requests.len());
    for f in received_requests {
        let friend_id = f.friend_id;
        let friend_id_str = friend_id.to_string();

        // Try to fetch profile from server
        let (display_name, username, avatar_url) =
            match ui_ctx.user_service.get_user_profile(&friend_id).await {
                Ok(profile) => {
                    let name = if profile.display_name.is_empty() {
                        friend_id_str.clone()
                    } else {
                        profile.display_name
                    };
                    let avatar = profile.avatar_url.unwrap_or_default();
                    (name.clone(), name, avatar)
                }
                Err(_) => (friend_id_str.clone(), friend_id_str.clone(), String::new()),
            };

        received_ui.push(FriendRequestSendData {
            user_id: friend_id_str,
            display_name,
            username,
            avatar_url,
            avatar_bytes: None, // Regular friends use avatar_url from server
            sent_at: String::new(),
        });
    }

    let mut sent_ui: Vec<FriendRequestSendData> = Vec::with_capacity(sent_requests.len());
    for f in sent_requests {
        let friend_id = f.friend_id;
        let friend_id_str = friend_id.to_string();

        // Try to fetch profile from server
        let (display_name, username, avatar_url) =
            match ui_ctx.user_service.get_user_profile(&friend_id).await {
                Ok(profile) => {
                    let name = if profile.display_name.is_empty() {
                        friend_id_str.clone()
                    } else {
                        profile.display_name
                    };
                    let avatar = profile.avatar_url.unwrap_or_default();
                    (name.clone(), name, avatar)
                }
                Err(_) => (friend_id_str.clone(), friend_id_str.clone(), String::new()),
            };

        sent_ui.push(FriendRequestSendData {
            user_id: friend_id_str,
            display_name,
            username,
            avatar_url,
            avatar_bytes: None, // Regular friends use avatar_url from server
            sent_at: String::new(),
        });
    }

    // Load anonymous DMs from local storage and add to DM list
    match ui_ctx.anon_dm_service.list_conversations() {
        Ok(anon_dms) => {
            tracing::info!("Loaded {} anonymous DM conversations", anon_dms.len());
            for dm in anon_dms {
                // Get peer display name from conversation (revealed on accept/init)
                // Do NOT use user_service - these are strangers
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                let peer_id_str = dm.peer_id.to_string();

                // Get latest message for preview
                let (last_message, timestamp) = match ui_ctx.anon_dm_service.get_latest_message(&dm.id) {
                    Ok(Some(msg)) => {
                        let is_own = current_user_id.map(|id| msg.sender_id == id).unwrap_or(false);
                        let preview = msg.as_text()
                            .map(|text| {
                                let mut display_text = text.to_string();
                                if display_text.chars().count() > 50 {
                                    let truncated: String = display_text.chars().take(47).collect();
                                    display_text = format!("{}...", truncated);
                                }
                                if is_own {
                                    format!("You: {}", display_text)
                                } else {
                                    display_text
                                }
                            })
                            .unwrap_or_default();
                        let ts = msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string();
                        (preview, ts)
                    }
                    _ => (String::new(), String::new()),
                };

                // Add to DM list with special prefix to indicate private
                // Use anon_dm: prefix in channel_id to identify these specially
                let dm_item = DMListItemSendData {
                    channel_id: format!("anon_dm:{}", dm.id),
                    user_id: peer_id_str.clone(),
                    display_name: format!("\u{1F512} {}", peer_name), // Lock emoji prefix
                    avatar_url: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    initials: get_initials(&peer_name),
                    presence_status: if dm.status == chatter_linux::domain::anon_dm::AnonDmStatus::Active {
                        "online".to_string()
                    } else {
                        "offline".to_string()
                    },
                    last_message,
                    timestamp,
                    unread_count: dm.unread_count as i32,
                    is_typing: false,
                };
                dm_list.push(dm_item);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load anonymous DM conversations: {}", e);
        }
    }

    // Load pending anon DM requests (message requests) - show as pending in sidebar and modal
    let mut private_requests_ui: Vec<FriendRequestSendData> = Vec::new();
    match ui_ctx.anon_dm_service.list_message_requests() {
        Ok(requests) => {
            tracing::info!("Loaded {} pending anon DM requests", requests.len());
            for dm in requests {
                // Get peer display name from conversation (revealed after ECIES decryption)
                // Do NOT use user_service - these are strangers
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                let peer_id_str = dm.peer_id.to_string();

                // Get intro message if any
                let last_message = match ui_ctx.anon_dm_service.get_latest_message(&dm.id) {
                    Ok(Some(msg)) => msg.as_text().unwrap_or_default(),
                    _ => "Message request".to_string(),
                };

                // Add to DM sidebar
                let dm_item = DMListItemSendData {
                    channel_id: format!("anon_dm:{}", dm.id),
                    user_id: peer_id_str.clone(),
                    display_name: format!("\u{1F4E9} {}", peer_name), // Envelope emoji for pending
                    avatar_url: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    initials: get_initials(&peer_name),
                    presence_status: "pending".to_string(),
                    last_message: last_message.clone(),
                    timestamp: dm.created_at
                        .with_timezone(&chrono::Local)
                        .format("%H:%M")
                        .to_string(),
                    unread_count: 1,
                    is_typing: false,
                };
                dm_list.push(dm_item);

                // Add to friends modal private requests list (use conversation_id as user_id)
                private_requests_ui.push(FriendRequestSendData {
                    user_id: dm.id.to_string(),
                    display_name: peer_name,
                    username: "Private".to_string(),
                    avatar_url: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    sent_at: last_message,
                });
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load pending anon DM requests: {}", e);
        }
    }

    // Load outgoing pending anon DM requests (requests we sent but haven't been accepted)
    match ui_ctx.anon_dm_service.list_outgoing_requests() {
        Ok(requests) => {
            tracing::info!("Loaded {} outgoing anon DM requests", requests.len());
            for dm in requests {
                // Get peer display name from conversation
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                let peer_id_str = dm.peer_id.to_string();

                // Get our last sent message for preview
                let (last_message, timestamp) = match ui_ctx.anon_dm_service.get_latest_message(&dm.id) {
                    Ok(Some(msg)) => {
                        let preview = msg.as_text()
                            .map(|text| {
                                let mut display_text = text.to_string();
                                if display_text.chars().count() > 50 {
                                    let truncated: String = display_text.chars().take(47).collect();
                                    display_text = format!("{}...", truncated);
                                }
                                format!("You: {}", display_text)
                            })
                            .unwrap_or_else(|| "Message request sent".to_string());
                        let ts = msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string();
                        (preview, ts)
                    }
                    _ => ("Waiting for response...".to_string(), dm.created_at.with_timezone(&chrono::Local).format("%H:%M").to_string()),
                };

                // Add to DM sidebar with "Waiting" indicator
                let dm_item = DMListItemSendData {
                    channel_id: format!("anon_dm:{}", dm.id),
                    user_id: peer_id_str.clone(),
                    display_name: format!("\u{1F4EE} {} (pending)", peer_name), // Postbox emoji
                    avatar_url: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    initials: get_initials(&peer_name),
                    presence_status: "offline".to_string(),
                    last_message,
                    timestamp,
                    unread_count: 0,
                    is_typing: false,
                };
                dm_list.push(dm_item);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load outgoing anon DM requests: {}", e);
        }
    }

    // Load private groups from local storage
    let private_groups: Vec<PrivateGroupListItemData> = match ui_ctx.anon_group_service.list_groups() {
        Ok(groups) => groups
            .into_iter()
            .map(|g| {
                let member_count = ui_ctx
                    .anon_group_service
                    .get_members(&g.id)
                    .map(|m| m.len())
                    .unwrap_or(0) as i32;

                // Get the latest message for preview
                let (last_message, timestamp) = match ui_ctx.anon_group_service.get_latest_message(&g.id) {
                    Ok(Some(msg)) => {
                        let preview = msg.as_text()
                            .map(|text| {
                                if text.chars().count() > 50 {
                                    let truncated: String = text.chars().take(47).collect();
                                    format!("{}...", truncated)
                                } else {
                                    text
                                }
                            })
                            .unwrap_or_default();
                        let ts = msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string();
                        (preview, ts)
                    }
                    _ => (String::new(), String::new()),
                };

                PrivateGroupListItemData {
                    group_id: g.id.to_string().into(),
                    name: g.name.into(),
                    member_count,
                    unread_count: 0, // TODO: track unread
                    last_message: last_message.into(),
                    timestamp: timestamp.into(),
                    is_typing: false,
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to load private groups: {}", e);
            Vec::new()
        }
    };

    tracing::info!("Loaded {} private groups", private_groups.len());

    // Update UI with channels, DMs, private groups, and friend requests
    let window_weak_ui = window_weak.clone();
    let pending_message_requests_count = private_requests_ui.len() as i32;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak_ui.upgrade() {
            w.set_channels(ModelRc::new(VecModel::from(channel_list)));
            // Convert Send-safe DM data to Slint types inside event loop
            let dm_list_slint: Vec<DMListItemData> = dm_list.into_iter().map(|d| d.to_slint()).collect();
            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list_slint)));
            w.set_private_groups(ModelRc::new(VecModel::from(private_groups)));
            w.set_user_display_name(user_display_name.into());
            w.set_user_initials(user_initials.into());
            w.set_user_presence_status("online".into());

            // Set friend requests (including private/anonymous DM requests)
            // Convert Send-safe data to Slint types inside event loop
            let received_ui_slint: Vec<FriendRequestData> = received_ui.into_iter().map(|r| r.to_slint()).collect();
            let sent_ui_slint: Vec<FriendRequestData> = sent_ui.into_iter().map(|r| r.to_slint()).collect();
            let private_requests_ui_slint: Vec<FriendRequestData> = private_requests_ui.into_iter().map(|r| r.to_slint()).collect();
            w.set_friends_received_requests(ModelRc::new(VecModel::from(received_ui_slint)));
            w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_ui_slint)));
            w.set_friends_private_requests(ModelRc::new(VecModel::from(private_requests_ui_slint)));

            // Set sidebar badge count for message requests
            w.set_pending_message_requests(pending_message_requests_count);
        }
    });

    // If there's a channel, select it and load messages
    if let Some(channel_id) = first_channel_id {
        let channel_id_str = channel_id.to_string();

        let window_weak_select = window_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window_weak_select.upgrade() {
                w.set_selected_channel_id(channel_id_str.into());
            }
        });

        let current_user_id_str = current_user_id.map(|id| id.to_string());
        load_channel_into_ui_from_services(window_weak, &channel_id, ui_ctx, current_user_id_str)
            .await;
    }

    tracing::info!("Initial data load complete");
}

/// Load channel data and messages into UI using services (no direct repo access).
async fn load_channel_into_ui_from_services(
    window_weak: Weak<MainWindow>,
    channel_id: &ChannelId,
    ui_ctx: &Arc<UIContext>,
    current_user_id: Option<String>,
) {
    // Get channel info from service
    let channel = match ui_ctx.channel_service.get_channel_local(channel_id) {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            tracing::warn!("Channel {} not found in local storage", channel_id);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
        Err(e) => {
            tracing::error!("Failed to get channel {}: {}", channel_id, e);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
    };

    // Fetch recent messages (limit 50)
    let messages = match ui_ctx.message_service.get_messages_local(channel_id, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to get messages for channel {}: {}", channel_id, e);
            Vec::new()
        }
    };

    // Convert messages to display data
    // Messages come from DB in DESC order (newest first), but chat UI shows oldest first
    let mut display_messages = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = if let Some(sender_id) = &msg.sender_id {
            ui_ctx.user_service.get_display_name(sender_id)
        } else {
            "Anonymous".to_string()
        };

        // Resolve attachments for this message
        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| match ui_ctx.attachment_service.get_metadata(att_id) {
                Ok(Some(att)) => {
                    tracing::debug!(
                        "Resolved attachment {} for message {}: local_path={:?}",
                        att_id,
                        msg.id,
                        att.local_path
                    );
                    Some(att)
                }
                Ok(None) => {
                    tracing::warn!(
                        "Attachment {} not found for message {}",
                        att_id,
                        msg.id
                    );
                    None
                }
                Err(e) => {
                    tracing::error!("Error fetching attachment {}: {}", att_id, e);
                    None
                }
            })
            .collect();

        let display_data = message_to_display_data(msg, &sender_name, &resolved_attachments);
        display_messages.push(display_data);
    }

    // Get current user ID for is_own calculation in to_slint_with_user
    let current_user_id_for_slint = current_user_id.clone().unwrap_or_default();

    // Reverse to show oldest first in chat
    display_messages.reverse();

    // Update UI
    let channel_name = channel.name.clone();
    let channel_id_str = channel_id.to_string();

    let ui_ctx_load = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak.upgrade() {
            // Convert MessageDisplayData to MessageItemData on UI thread
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|m| m.to_slint_with_user(&current_user_id_for_slint))
                .collect();

            w.set_channel_name(channel_name.into());
            w.set_selected_channel_id(channel_id_str.into());
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);

            // Auto-register GIF animations for inline playback
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                w.as_weak(),
                ui_ctx_load.attachment_service.clone(),
            );
        }
    });
}
