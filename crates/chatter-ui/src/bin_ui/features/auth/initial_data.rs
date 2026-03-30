//! Initial data loading after login.
//!
//! Uses ChatterClient services for all operations - no direct repository access.

use std::sync::Arc;

use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel, Weak, Model};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::message::ConversationType;
use chatter_core::domain::ids::ChannelId;
use chatter_core::services::user::FriendRequestFilter;

use crate::bin_ui::converters::{
    channel_to_slint, dm_to_send_data, get_initials, message_to_display_data,
    DMListItemSendData, FriendRequestSendData,
};
use crate::{ChannelListItemData, DMListItemData, FriendRequestData, MainWindow, PrivateGroupListItemData};

/// Load initial data after login or on startup.
pub async fn load_initial_data(window_weak: Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    tracing::info!("Loading initial data...");

    // Sync channels from server first (fetches and upserts to local storage)
    match ui_ctx.channels().list_channels(100, 0, None).await {
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
    let channels = match ui_ctx.channels().get_channels_for_user() {
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
    let current_user_id = ui_ctx.auth_state().get().current_user_id;

    // Separate channels and DMs (using Send-safe types for DMs)
    let mut channel_list: Vec<ChannelListItemData> = Vec::new();
    let mut dm_list: Vec<DMListItemSendData> = Vec::new();

    for channel in &channels {
        match channel.channel_type {
            ChannelType::DirectMessage => {
                // For DMs, we need to find the OTHER user (not ourselves)
                let other_user_id = if let Some(current_id) = current_user_id {
                    // Get channel members and find the one that's not us
                    match ui_ctx.channels().list_members_local(&channel.id) {
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
                    match ui_ctx.messages().get_messages_local(&channel.id, &ConversationType::ServerDm, None, 1) {
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
        .auth_state()
        .get()
        .display_name
        .clone()
        .unwrap_or_default();
    let user_initials = get_initials(&user_display_name);

    // Load user's own avatar from settings (stored during registration)
    // Pre-decode before entering UI thread to avoid blocking
    let user_avatar_decoded: Option<(Vec<u8>, u32, u32)> =
        ui_ctx.anon_dms().get_user_avatar().and_then(|bytes| {
            image::load_from_memory(&bytes).ok().map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                tracing::info!(
                    avatar_size = bytes.len(),
                    width = w,
                    height = h,
                    "Loaded user's own avatar from settings"
                );
                (rgba.into_raw(), w, h)
            })
        });

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
        .users()
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
        .users()
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
            match ui_ctx.users().get_user_profile(&friend_id).await {
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
            avatar_decoded: None,
            sent_at: String::new(),
        });
    }

    let mut sent_ui: Vec<FriendRequestSendData> = Vec::with_capacity(sent_requests.len());
    for f in sent_requests {
        let friend_id = f.friend_id;
        let friend_id_str = friend_id.to_string();

        // Try to fetch profile from server
        let (display_name, username, avatar_url) =
            match ui_ctx.users().get_user_profile(&friend_id).await {
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
            avatar_decoded: None,
            sent_at: String::new(),
        });
    }

    // Load anonymous DMs from local storage and add to DM list
    match ui_ctx.anon_dms().list_conversations() {
        Ok(anon_dms) => {
            tracing::info!("Loaded {} anonymous DM conversations", anon_dms.len());
            for dm in anon_dms {
                // Get peer display name from conversation (revealed on accept/init)
                // Do NOT use user_service - these are strangers
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                let peer_id_str = dm.peer_id.to_string();

                // Get latest message for preview
                let (last_message, timestamp) = match ui_ctx.anon_dms().get_latest_message(&dm.id) {
                    Ok(Some(msg)) => {
                        let is_own = current_user_id.map(|id| msg.sender_id == Some(id)).unwrap_or(false);
                        let preview = msg.as_text()
                            .map(|text| {
                                let display_text = text.to_string();
                                if display_text.chars().count() > 50 {
                                    let truncated: String = display_text.chars().take(47).collect();
                                    format!("{}...", truncated)
                                } else if is_own {
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
                    avatar_decoded: None,
                    initials: get_initials(&peer_name),
                    presence_status: if dm.status == chatter_core::domain::anon_dm::AnonDmStatus::Active {
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
    match ui_ctx.anon_dms().list_message_requests() {
        Ok(requests) => {
            tracing::info!("Loaded {} pending anon DM requests", requests.len());
            for dm in requests {
                // Get peer display name from conversation (revealed after ECIES decryption)
                // Do NOT use user_service - these are strangers
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                let peer_id_str = dm.peer_id.to_string();

                // Get intro message if any
                let last_message = match ui_ctx.anon_dms().get_latest_message(&dm.id) {
                    Ok(Some(msg)) => msg.as_text().unwrap_or_default().to_string(),
                    _ => "Message request".to_string(),
                };

                // Add to DM sidebar
                let dm_item = DMListItemSendData {
                    channel_id: format!("anon_dm:{}", dm.id),
                    user_id: peer_id_str.clone(),
                    display_name: format!("\u{1F4E9} {}", peer_name), // Envelope emoji for pending
                    avatar_url: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    avatar_decoded: None,
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
                    avatar_decoded: None,
                    sent_at: last_message,
                });
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load pending anon DM requests: {}", e);
        }
    }

    // Load outgoing pending anon DM requests (requests we sent but haven't been accepted)
    match ui_ctx.anon_dms().list_outgoing_requests() {
        Ok(requests) => {
            tracing::info!("Loaded {} outgoing anon DM requests", requests.len());
            for dm in requests {
                // Get peer display name from conversation
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                let peer_id_str = dm.peer_id.to_string();

                // Get our last sent message for preview
                let (last_message, timestamp) = match ui_ctx.anon_dms().get_latest_message(&dm.id) {
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
                    avatar_decoded: None,
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
    let private_groups: Vec<PrivateGroupListItemData> = match ui_ctx.anon_groups().list_groups() {
        Ok(groups) => groups
            .into_iter()
            .map(|g| {
                let member_count = ui_ctx
                    .anon_groups()
                    .get_members(&g.id)
                    .map(|m| m.len())
                    .unwrap_or(0) as i32;

                // Get the latest message for preview
                let (last_message, timestamp) = match ui_ctx.anon_groups().get_latest_message(&g.id) {
                    Ok(Some(msg)) => {
                        let preview = msg.as_text()
                            .map(|text| {
                                if text.chars().count() > 50 {
                                    let truncated: String = text.chars().take(47).collect();
                                    format!("{}...", truncated)
                                } else {
                                    text.to_string()
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

    // Load blocked users for the Blocked tab
    let blocked_users_data: Vec<(String, String)> = match ui_ctx.anon_dms().get_blocked_users() {
        Ok(users) => {
            tracing::info!("Loaded {} blocked users", users.len());
            users.into_iter()
                .map(|user_id| {
                    let id_str = user_id.to_string();
                    let display_name = format!("User {}", &id_str[..8.min(id_str.len())]);
                    (id_str, display_name)
                })
                .collect()
        }
        Err(e) => {
            tracing::warn!("Failed to load blocked users: {}", e);
            Vec::new()
        }
    };

    // Pre-decode avatars before entering UI thread to avoid blocking
    let mut dm_list = dm_list;
    for dm in &mut dm_list {
        dm.pre_decode_avatar();
    }
    let mut private_requests_ui = private_requests_ui;
    for req in &mut private_requests_ui {
        req.pre_decode_avatar();
    }

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

            // Compute initial total unread count for mobile bottom nav badge
            crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(&w);

            w.set_user_display_name(user_display_name.into());
            w.set_user_initials(user_initials.into());
            w.set_user_presence_status("online".into());

            // Set user's own avatar if available
            if let Some((pixels, width, height)) = user_avatar_decoded {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &pixels, width, height,
                );
                w.set_user_avatar_source(Image::from_rgba8(buffer));
            }

            // Set friend requests (including private/anonymous DM requests)
            // Convert Send-safe data to Slint types inside event loop
            let received_ui_slint: Vec<FriendRequestData> = received_ui.into_iter().map(|r| r.to_slint()).collect();
            let sent_ui_slint: Vec<FriendRequestData> = sent_ui.into_iter().map(|r| r.to_slint()).collect();
            let private_requests_ui_slint: Vec<FriendRequestData> = private_requests_ui.into_iter().map(|r| r.to_slint()).collect();
            w.set_friends_received_requests(ModelRc::new(VecModel::from(received_ui_slint)));
            w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_ui_slint)));
            w.set_friends_private_requests(ModelRc::new(VecModel::from(private_requests_ui_slint)));

            // Set blocked users
            let blocked_users_slint: Vec<crate::BlockedUserData> = blocked_users_data
                .into_iter()
                .map(|(user_id, display_name)| crate::BlockedUserData {
                    user_id: user_id.into(),
                    display_name: display_name.into(),
                    username: "Private".into(),
                    avatar_url: SharedString::default(),
                    blocked_at: SharedString::default(),
                })
                .collect();
            w.set_friends_blocked_users(ModelRc::new(VecModel::from(blocked_users_slint)));

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
    ui_ctx: &Arc<ChatterClient>,
    current_user_id: Option<String>,
) {
    // Get channel info from service
    let channel = match ui_ctx.channels().get_channel_local(channel_id) {
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

    // Use the channel's own conversation_type() which respects require_encryption
    let conversation_type = channel.conversation_type();

    // Fetch recent messages (limit 50)
    let messages = match ui_ctx.messages().get_messages_local(channel_id, &conversation_type, None, 50) {
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
            ui_ctx.users().get_display_name(sender_id)
        } else {
            "Anonymous".to_string()
        };

        // Resolve attachments for this message
        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| match ui_ctx.attachments().get_metadata(att_id) {
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

    // Pre-decode attachment thumbnails before UI thread
    for msg in &mut display_messages {
        msg.decode_attachment_thumbnails();
    }

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
                ui_ctx_load.attachments().clone(),
            );
        }
    });
}

/// Refresh the DM list in the UI with latest data from storage.
pub fn refresh_dm_list(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let window_weak = window.as_weak();
    
    tokio::spawn(async move {
        // Get current user ID for DM peer detection
        let current_user_id = ui_ctx.auth_state().get().current_user_id;
        
        // Get channels from local storage
        let channels = match ui_ctx.channels().get_channels_for_user() {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to refresh DM list: {}", e);
                return;
            }
        };
        
        let mut dm_list: Vec<DMListItemSendData> = Vec::new();
        
        // Process regular DMs
        for channel in &channels {
            if channel.channel_type == ChannelType::DirectMessage {
                // Find the OTHER user (not ourselves)
                let other_user_id = if let Some(current_id) = current_user_id {
                    match ui_ctx.channels().list_members_local(&channel.id) {
                        Ok(members) => members
                            .iter()
                            .find(|m| m.user_id != current_id)
                            .map(|m| m.user_id.to_string())
                            .unwrap_or_else(|| channel.owner_id.to_string()),
                        Err(_) => channel.owner_id.to_string(),
                    }
                } else {
                    channel.owner_id.to_string()
                };
                
                let other_user_name = channel.name.clone();
                
                // Get the most recent message
                let (last_message, timestamp) = 
                    match ui_ctx.messages().get_messages_local(&channel.id, &ConversationType::ServerDm, None, 1) {
                        Ok(messages) if !messages.is_empty() => {
                            let msg = &messages[0];
                            let is_own = current_user_id.map(|id| msg.sender_id == Some(id)).unwrap_or(false);
                            
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
                                .unwrap_or_else(|| {
                                    if msg.has_attachments() {
                                        if is_own {
                                            "You: [Attachment]".to_string()
                                        } else {
                                            "[Attachment]".to_string()
                                        }
                                    } else {
                                        String::new()
                                    }
                                });
                            
                            let ts = msg.created_at
                                .with_timezone(&chrono::Local)
                                .format("%H:%M")
                                .to_string();
                            
                            (preview, ts)
                        }
                        _ => (String::new(), String::new()),
                    };
                
                // Create DM item manually since dm_to_send_data signature changed
                let dm_item = DMListItemSendData {
                    channel_id: channel.id.to_string(),
                    user_id: other_user_id.clone(),
                    display_name: other_user_name.clone(),
                    avatar_url: String::new(),
                    avatar_bytes: Some(Vec::new()),
                    avatar_decoded: None,
                    initials: get_initials(&other_user_name),
                    presence_status: "online".to_string(),
                    last_message,
                    timestamp,
                    unread_count: 0,
                    is_typing: false,
                };
                dm_list.push(dm_item);
            }
        }
        
        // Update UI on the main thread
        if let Some(w) = window_weak.upgrade() {
            // Convert Send-safe DM data to Slint types
            let dm_list_slint: Vec<DMListItemData> = dm_list.into_iter()
                .map(|d| d.to_slint())
                .collect();
            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list_slint)));
        }
    });
}

/// Refresh the anonymous groups list in the UI.
pub fn refresh_anon_groups(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let window_weak = window.as_weak();
    
    tokio::spawn(async move {
        let mut private_group_list: Vec<PrivateGroupListItemData> = Vec::new();
        
        // Load anonymous groups
        match ui_ctx.anon_groups().list_groups() {
            Ok(groups) => {
                for group in groups {
                    let group_item = PrivateGroupListItemData {
                        group_id: SharedString::from(group.id.to_string()),
                        name: SharedString::from(group.name.clone()),
                        member_count: 0, // Could query member count from storage if needed
                        unread_count: 0,
                        last_message: SharedString::from(""),
                        timestamp: SharedString::from(""),
                        is_typing: false,
                    };
                    private_group_list.push(group_item);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load anonymous groups: {}", e);
            }
        }
        
        // Update UI on the main thread
        if let Some(w) = window_weak.upgrade() {
            let private_group_model = VecModel::from(private_group_list);
            w.set_private_groups(ModelRc::new(private_group_model));
        }
    });
}

/// Refresh the anonymous DMs list in the UI.
pub fn refresh_anon_dms(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let window_weak = window.as_weak();
    
    tokio::spawn(async move {
        let mut dm_list: Vec<DMListItemSendData> = Vec::new();
        
        // Load active anon DM conversations
        match ui_ctx.anon_dms().list_conversations() {
            Ok(dms) => {
                for dm in dms {
                    if dm.status != chatter_core::domain::anon_dm::AnonDmStatus::Pending {
                        let peer_name = dm.peer_display_name.clone()
                            .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                        let peer_id_str = dm.peer_id.to_string();
                        
                        // Get latest message for preview
                        let (last_message, timestamp) = match ui_ctx.anon_dms().get_latest_message(&dm.id) {
                            Ok(Some(msg)) => {
                                let preview = msg.as_text()
                                    .map(|text| {
                                        if text.chars().count() > 50 {
                                            let truncated: String = text.chars().take(47).collect();
                                            format!("{}...", truncated)
                                        } else {
                                            text.to_string()
                                        }
                                    })
                                    .unwrap_or_else(|| "[Attachment]".to_string());
                                
                                let ts = msg.created_at
                                    .with_timezone(&chrono::Local)
                                    .format("%H:%M")
                                    .to_string();
                                
                                (preview, ts)
                            }
                            _ => (String::new(), String::new()),
                        };
                        
                        let dm_item = DMListItemSendData {
                            channel_id: format!("anon_dm:{}", dm.id),
                            user_id: peer_id_str.clone(),
                            display_name: format!("\u{1F512} {}", peer_name), // Lock emoji prefix
                            avatar_url: String::new(),
                            avatar_bytes: dm.peer_avatar.clone(),
                            avatar_decoded: None,
                            initials: get_initials(&peer_name),
                            presence_status: if dm.status == chatter_core::domain::anon_dm::AnonDmStatus::Active {
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
            }
            Err(e) => {
                tracing::warn!("Failed to refresh anonymous DMs: {}", e);
            }
        }
        
        // Also load pending anon DM requests
        match ui_ctx.anon_dms().list_message_requests() {
            Ok(requests) => {
                for dm in requests {
                    let peer_name = dm.peer_display_name.clone()
                        .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                    let peer_id_str = dm.peer_id.to_string();
                    
                    let last_message = match ui_ctx.anon_dms().get_latest_message(&dm.id) {
                        Ok(Some(msg)) => msg.as_text().unwrap_or_default().to_string(),
                        _ => "Message request".to_string(),
                    };

                    let dm_item = DMListItemSendData {
                        channel_id: format!("anon_dm:{}", dm.id),
                        user_id: peer_id_str.clone(),
                        display_name: format!("\u{1F4E9} {}", peer_name), // Envelope emoji for pending
                        avatar_url: String::new(),
                        avatar_bytes: dm.peer_avatar.clone(),
                        avatar_decoded: None,
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
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load pending anon DM requests: {}", e);
            }
        }
        
        // Get existing DM list and merge with anon DMs
        if let Some(w) = window_weak.upgrade() {
            let existing_dms = w.get_dm_conversations();
            let mut all_dms = Vec::new();
            
            // Add existing regular DMs (non-anon)
            let existing_count = existing_dms.iter().count();
            for i in 0..existing_count {
                if let Some(dm) = existing_dms.iter().nth(i) {
                    let channel_id = dm.channel_id.to_string();
                    if !channel_id.starts_with("anon_dm:") {
                        all_dms.push(dm);
                    }
                }
            }
            
            // Add the anon DMs
            for dm in dm_list {
                all_dms.push(dm.to_slint());
            }
            
            let dm_model = VecModel::from(all_dms);
            w.set_dm_conversations(ModelRc::new(dm_model));
        }
    });
}

/// Update typing indicator for a specific user in a DM.
pub fn update_dm_typing_indicator(window: &MainWindow, user_id: &str, is_typing: bool) {
    let dm_list = window.get_dm_conversations();
    
    // Find and update the specific DM
    let dm_count = dm_list.iter().count();
    for i in 0..dm_count {
        if let Some(dm) = dm_list.iter().nth(i) {
            if dm.user_id.to_string() == user_id {
                // Need to recreate the model to update
                let mut all_dms = Vec::new();
                for j in 0..dm_count {
                    if let Some(mut item) = dm_list.iter().nth(j) {
                        if j == i {
                            item.is_typing = is_typing;
                        }
                        all_dms.push(item);
                    }
                }
                window.set_dm_conversations(ModelRc::new(VecModel::from(all_dms)));
                break;
            }
        }
    }
}

/// Show incoming call modal with caller information.
pub fn show_incoming_call(
    window: &MainWindow,
    call_id: &str,
    peer_id: &str,
    caller_name: &str,
    call_type: &str,
    is_identity_verified: bool,
    identity_warning: &str,
    is_anonymous: bool,
) {
    use crate::IncomingCallData;

    // Create incoming call data structure
    let call_data = IncomingCallData {
        call_id: call_id.into(),
        caller_id: peer_id.into(),
        caller_name: caller_name.into(),
        caller_avatar: String::new().into(),
        channel_name: String::new().into(),
        is_video_call: call_type == "video",
        is_anonymous_call: is_anonymous,
        is_identity_verified,
        identity_warning: identity_warning.into(),
        encryption_key: String::new().into(),
        call_start_timestamp_ns: String::new().into(),
    };
    
    // Set call data and show modal
    window.set_incoming_call_data(call_data);
    window.set_incoming_call_visible(true);
}

/// Hide incoming call modal.
pub fn hide_incoming_call(window: &MainWindow) {
    window.set_incoming_call_visible(false);
}

/// Update friend requests lists in the UI.
pub fn update_friend_requests(
    window: &MainWindow,
    received: Vec<crate::FriendRequestData>,
    sent: Vec<crate::FriendRequestData>,
) {
    use slint::{ModelRc, VecModel};
    
    // Update received requests
    let received_model = VecModel::from(received);
    window.set_friends_received_requests(ModelRc::new(received_model));
    
    // Update sent requests
    let sent_model = VecModel::from(sent);
    window.set_friends_sent_requests(ModelRc::new(sent_model));
}
