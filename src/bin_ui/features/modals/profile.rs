//! Profile modal callbacks.
//!
//! Uses UserService for all profile operations - no direct network calls.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::UserId;
use chatter_linux::services::user::FriendRequestFilter;

use crate::bin_ui::converters::{get_initials, DMListItemSendData, FriendRequestSendData};
use crate::{DMListItemData, FriendRequestData, MainWindow};

thread_local! {
    static PENDING_AVATAR_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub fn wire_profile_actions(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let user_service = ui_ctx.user_service.clone();
    let channel_service = ui_ctx.channel_service.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();

    // Add friend
    let weak_window = window.as_weak();
    let user_service_add = user_service.clone();

    window.on_profile_add_friend(move || {
        let weak = weak_window.clone();
        let user_service = user_service_add.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let friend_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                user_service.send_friend_request(&friend_id, None),
            ).await {
                Ok(Ok(_)) => {
                    tracing::info!("Friend request sent to {}", friend_id);

                    // Refresh sent requests list via UserService
                    match user_service.list_friend_requests(FriendRequestFilter::Sent, 50, 0).await {
                        Ok(friendships) => {
                            // We need to get the full profile info for each friend
                            // For now, just use friend_id as display info
                            // Use Send-safe type to pass across thread boundary
                            let sent_requests: Vec<FriendRequestSendData> = friendships
                                .into_iter()
                                .map(|f| FriendRequestSendData {
                                    user_id: f.friend_id.to_string(),
                                    display_name: f.friend_id.to_string(),
                                    username: String::new(),
                                    avatar_url: String::new(),
                                    avatar_bytes: None,
                                    avatar_decoded: None,
                                    sent_at: String::new(),
                                })
                                .collect();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_profile_loading(false);
                                    // Convert Send-safe data to Slint types inside event loop
                                    let sent_slint: Vec<FriendRequestData> = sent_requests.into_iter().map(|r| r.to_slint()).collect();
                                    w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_slint)));
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!("Failed to refresh sent requests: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_profile_loading(false);
                                }
                            });
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to send friend request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Friend request timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Remove friend
    let weak_window = window.as_weak();
    let user_service_remove = user_service.clone();

    window.on_profile_remove_friend(move || {
        let weak = weak_window.clone();
        let user_service = user_service_remove.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let friend_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                user_service.remove_friend(&friend_id),
            ).await {
                Ok(Ok(())) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_friend(false);
                            tracing::info!("Removed friend: {}", friend_id);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to remove friend: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Remove friend timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Block user
    let weak_window = window.as_weak();
    let user_service_block = user_service.clone();

    window.on_profile_block_user(move || {
        let weak = weak_window.clone();
        let user_service = user_service_block.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let user_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                user_service.block_user(&user_id),
            ).await {
                Ok(Ok(())) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(true);
                            w.set_profile_is_friend(false);
                            tracing::info!("Blocked user: {}", user_id);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to block user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Block user timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Unblock user
    let weak_window = window.as_weak();
    let user_service_unblock = user_service.clone();

    window.on_profile_unblock_user(move || {
        let weak = weak_window.clone();
        let user_service = user_service_unblock.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let user_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                user_service.unblock_user(&user_id),
            ).await {
                Ok(Ok(())) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(false);
                            tracing::info!("Unblocked user: {}", user_id);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to unblock user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Unblock user timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Send message (open DM)
    let weak_window = window.as_weak();
    let mailbox_manager = mailbox_manager.clone();
    let channel_service = channel_service.clone();
    let user_service_dm = user_service.clone();

    window.on_profile_send_message(move || {
        let weak = weak_window.clone();
        let mailbox_manager = mailbox_manager.clone();
        let channel_service = channel_service.clone();
        let user_service = user_service_dm.clone();

        // Get profile user ID synchronously before spawning
        let profile_user_id = weak
            .upgrade()
            .map(|w| w.get_profile_user_id().to_string())
            .unwrap_or_default();

        if profile_user_id.is_empty() {
            tracing::error!("No profile user ID for DM");
            return;
        }

        tokio::spawn(async move {
            // Show loading state and close modal
            let weak_loading = weak.clone();
            let profile_id_for_ui = profile_user_id.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_active_modal("".into());
                    w.set_loading_messages(true);
                }
            });

            // Parse user ID
            let peer_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID for DM: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                    return;
                }
            };

            // Use peer_id as conversation_id for 1:1 DMs
            let conversation_id = peer_id.to_string();

            // Initialize mailbox manager if needed
            if let Err(e) = mailbox_manager.initialize().await {
                tracing::warn!("Failed to initialize mailbox manager: {} (may already be initialized)", e);
            }

            // Ensure we have tokens available
            if mailbox_manager.token_count().await == 0 {
                if let Err(e) = mailbox_manager.acquire_tokens(5).await {
                    tracing::error!("Failed to acquire mailbox tokens: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                    return;
                }
            }

            // Get or create mailbox for this DM conversation
            // TTL: 7 days (604800 seconds)
            match mailbox_manager.get_or_create_mailbox(&conversation_id, 604800).await {
                Ok(mailbox) => {
                    tracing::info!(
                        "DM mailbox ready for conversation with {}: mailbox_id={}",
                        peer_id,
                        mailbox.mailbox_id
                    );

                    // Get peer display name via UserService
                    let peer_name = match user_service.get_user_profile(&peer_id).await {
                        Ok(profile) => {
                            if profile.display_name.is_empty() {
                                peer_id.to_string()
                            } else {
                                profile.display_name
                            }
                        }
                        Err(_) => peer_id.to_string(),
                    };

                    // Find the DM channel for this peer using channel service
                    let dm_channel = channel_service
                        .get_dm_channel_for_peer(&peer_id)
                        .ok()
                        .flatten();

                    let channel_id_for_ui = dm_channel
                        .as_ref()
                        .map(|c| c.id.to_string())
                        .unwrap_or_else(|| profile_id_for_ui.clone());

                    // Create Send-safe DM sidebar item data
                    let initials = get_initials(&peer_name);
                    let unread = dm_channel.as_ref().map(|c| c.unread_count as i32).unwrap_or(0);
                    let dm_send_data = DMListItemSendData {
                        channel_id: channel_id_for_ui.clone(),
                        user_id: profile_id_for_ui.clone(),
                        display_name: peer_name.clone(),
                        initials,
                        avatar_url: String::new(),
                        avatar_bytes: None, // Regular DMs fetch avatar from server
                        avatar_decoded: None,
                        presence_status: "offline".to_string(),
                        last_message: String::new(),
                        timestamp: String::new(),
                        unread_count: unread,
                        is_typing: false,
                    };

                    // Update UI to show the DM and add to sidebar
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            // Add to sidebar if not already present
                            let current_dms = w.get_dm_conversations();
                            let mut dms: Vec<DMListItemData> = (0..current_dms.row_count())
                                .filter_map(|i| current_dms.row_data(i))
                                .collect();

                            let exists = dms.iter().any(|dm| dm.user_id.as_str() == profile_id_for_ui);
                            if !exists {
                                // Convert Send-safe data to Slint type inside event loop
                                let dm_item = dm_send_data.to_slint();
                                dms.push(dm_item);
                                w.set_dm_conversations(ModelRc::new(VecModel::from(dms)));
                                tracing::info!("Added DM to sidebar from profile modal");
                            }

                            w.set_loading_messages(false);
                            // Use channel_id for selection (matching updated dm_list.slint)
                            w.set_selected_dm_id(channel_id_for_ui.into());
                            w.set_selected_channel_id("".into());
                            w.set_channel_name(peer_name.into());
                            w.set_channel_type("dm".into());
                            w.set_is_dm(true);
                            w.set_member_count(2);
                            w.set_topic("".into());
                            // Clear messages for new conversation
                            let empty: Vec<crate::MessageItemData> = vec![];
                            w.set_messages(ModelRc::new(VecModel::from(empty)));
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to create DM mailbox: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                }
            }
        });
    });

    // Edit profile actions
    let weak_window = window.as_weak();
    window.on_profile_edit_profile(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_edit_profile_display_name(w.get_profile_display_name());
                w.set_edit_profile_bio(w.get_profile_bio());
                w.set_edit_profile_avatar(w.get_user_avatar_source());
                
                // Clear any pending avatar
                PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = None);
                
                w.set_active_modal("edit-profile".into());
            }
        });
    });

    let weak_window = window.as_weak();
    let user_service_save = user_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let ui_ctx_save = ui_ctx.clone();
    window.on_edit_profile_save(move |display_name: slint::SharedString, bio: slint::SharedString| {
        let weak = weak_window.clone();
        let user_service = user_service_save.clone();
        let attachment_service = attachment_service.clone();
        let ui_ctx = ui_ctx_save.clone();
        let display_name = display_name.to_string();
        let bio = bio.to_string();

        // Check for pending avatar path
        let pending_avatar = PENDING_AVATAR_PATH.with(|p| p.borrow().clone());

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_edit_profile_loading(true);
                    w.set_edit_profile_error("".into());
                }
            });

            // 1. Upload avatar if needed
            let mut avatar_url = None;
            if let Some(path) = pending_avatar {
                match attachment_service.upload_to_cdn_encrypted(&path).await {
                    Ok(result) => {
                        avatar_url = Some(result.cdn_key);
                        tracing::info!("Uploaded new avatar to CDN: {}", avatar_url.as_ref().unwrap());
                    }
                    Err(e) => {
                        let error_msg = format!("Failed to upload avatar: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_edit_profile_loading(false);
                                w.set_edit_profile_error(error_msg.into());
                            }
                        });
                        return;
                    }
                }
            } else {
                // No new avatar, but we should get the existing avatar_url if any
                // Get current profile to preserve avatar_url
                if let Ok(current_user_id) = user_service.current_user_id() {
                    if let Ok(Some(current_profile)) = user_service.get_profile_local(&current_user_id) {
                        avatar_url = current_profile.avatar_url;
                        if avatar_url.is_some() {
                            tracing::debug!("Using existing avatar_url: {:?}", avatar_url);
                        }
                    }
                }
            }

            // 2. Map bio to status message
            let status_msg = if !bio.is_empty() {
                Some(chatter_linux::domain::user::StatusMessage::new(bio))
            } else {
                None
            };

            // 3. Update profile
            tracing::info!(
                "Updating user profile - display_name: {}, avatar_url: {:?}, bio: {:?}",
                display_name, avatar_url, status_msg.as_ref().map(|m| &m.text)
            );
            
            match user_service
                .update_user_profile(Some(&display_name), avatar_url.as_deref(), status_msg.as_ref())
                .await
            {
                Ok(profile) => {
                    tracing::info!(
                        "Profile updated - display_name: {}, avatar_url: {:?}, bio: {:?}",
                        profile.display_name, profile.avatar_url, profile.status_message.as_ref().map(|m| &m.text)
                    );
                    
                    // Broadcast profile update to all anon DM conversations
                    broadcast_profile_update(&ui_ctx, &profile).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_edit_profile_loading(false);
                            w.set_active_modal("none".into());

                            // Update UI with new profile info
                            w.set_profile_display_name(profile.display_name.clone().into());
                            if let Some(msg) = &profile.status_message {
                                w.set_profile_bio(msg.text.clone().into());
                            } else {
                                w.set_profile_bio("".into());
                            }

                            // Update sidebar name too
                            w.set_user_display_name(profile.display_name.into());
                            
                            // Clear pending avatar
                            PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = None);
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_edit_profile_loading(false);
                            w.set_edit_profile_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Change avatar with file picker
    let weak_window = window.as_weak();
    window.on_edit_profile_change_avatar(move || {
        #[cfg(not(target_os = "android"))]
        {
            let weak = weak_window.clone();

            tokio::spawn(async move {
                let config = chatter_linux::media::FilePickerConfig::new()
                    .with_title("Select Profile Picture")
                    .with_filter(chatter_linux::media::FileFilter::images());

                if let Some(path) = chatter_linux::media::FilePicker::pick_file(config).await {
                    match std::fs::read(&path) {
                        Ok(data) => {
                            // Store the path for later use
                            PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));

                            // Create the image on the UI thread
                            let _ = slint::invoke_from_event_loop(move || {
                                // Process image on UI thread where it's safe
                                match chatter_linux::media::ImageProcessor::memory_to_slint_image(&data) {
                                    Ok(slint_img) => {
                                        if let Some(w) = weak.upgrade() {
                                            w.set_edit_profile_avatar(slint_img);
                                        }
                                    }
                                    Err(e) => tracing::error!("Failed to decode avatar image: {}", e),
                                }
                            });
                        }
                        Err(e) => tracing::error!("Failed to read avatar file: {}", e),
                    }
                }
            });
        }
        #[cfg(target_os = "android")]
        tracing::warn!("File picker not supported on Android");
    });
}

/// Broadcast profile update to all anon DM conversations and anon groups
async fn broadcast_profile_update(ui_ctx: &Arc<UIContext>, profile: &chatter_linux::domain::user::Profile) {
    let display_name = Some(profile.display_name.clone());
    let avatar_url = profile.avatar_url.clone();
    let bio = profile.status_message.as_ref().map(|m| m.text.clone());
    
    tracing::info!(
        "Broadcasting profile update - display_name: {:?}, avatar_url: {:?}, bio: {:?}",
        display_name, avatar_url, bio
    );
    
    // Broadcast to all anon DM conversations
    let conversations = match ui_ctx.anon_dm_service.list_conversations() {
        Ok(convs) => convs,
        Err(e) => {
            tracing::warn!("Failed to list anon DM conversations for profile broadcast: {}", e);
            return;
        }
    };
    
    for conv in &conversations {
        if !conv.is_ready() {
            continue;
        }
        
        let conv_id = conv.id;
        let service = ui_ctx.anon_dm_service.clone();
        let display_name = display_name.clone();
        let avatar_url = avatar_url.clone();
        let bio = bio.clone();
        
        tokio::spawn(async move {
            if let Err(e) = service.send_profile_update(&conv_id, display_name, avatar_url, bio).await {
                tracing::debug!("Failed to send profile update to anon DM: {}", e);
            }
        });
    }
    
    // Broadcast to anon groups where user is owner
    let anon_group_service = ui_ctx.anon_group_service.clone();
    tokio::spawn(async move {
        if let Err(e) = anon_group_service.broadcast_profile_to_owned_groups().await {
            tracing::warn!("Failed to broadcast profile to owned groups: {}", e);
        }
    });
    
    tracing::info!("Broadcasted profile update to {} anon DM conversations", conversations.len());
}
