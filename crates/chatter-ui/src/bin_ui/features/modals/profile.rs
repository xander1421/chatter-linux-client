//! Profile modal callbacks.
//!
//! Uses UserService for all profile operations - no direct network calls.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use crate::bin_ui::converters::{get_initials, DMListItemExt, DMListItemSendData};
use crate::{DMListItemData, MainWindow};

thread_local! {
    static PENDING_AVATAR_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Open the user profile modal with all properties populated correctly.
///
/// Handles both "self" (own profile) and "other user" paths, ensuring
/// consistent data regardless of which entry point triggered the modal.
pub fn open_user_profile(
    w: &MainWindow,
    user_id: &UserId,
    auth_state: &chatter_core::state::AuthState,
    user_state: &chatter_core::state::UserState,
    ui_ctx: Option<&Arc<ChatterClient>>,
) {
    let is_self = auth_state.get().current_user_id.as_ref() == Some(user_id);

    w.set_profile_user_id(user_id.to_string().into());
    // Reset safety number state for fresh modal
    w.set_profile_safety_number("".into());
    w.set_profile_show_safety_number(false);

    if is_self {
        let state = auth_state.get();
        let display_name = state.display_name.clone().unwrap_or_default();
        let username = user_state
            .get_user(user_id)
            .map(|u| u.username.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| user_id.to_string());
        let initials = get_initials(&display_name);

        w.set_profile_display_name(display_name.into());
        w.set_profile_username(username.into());
        w.set_profile_initials(initials.into());
        w.set_profile_avatar_source(w.get_user_avatar_source());
        w.set_profile_presence(w.get_user_presence_status());

        if let Some(profile) = user_state.get_profile(user_id) {
            if let Some(msg) = profile.status_message {
                w.set_profile_bio(msg.text.into());
            } else {
                w.set_profile_bio("".into());
            }
        } else {
            w.set_profile_bio("".into());
        }

        w.set_profile_is_self(true);
        w.set_profile_is_friend(false);
        w.set_profile_is_blocked(false);
        w.set_profile_is_verified(false);
    } else {
        // Other-user path
        let profile = user_state.get_profile(user_id);
        let user = user_state.get_user(user_id);

        let display_name = profile
            .as_ref()
            .map(|p| &p.display_name)
            .filter(|n| !n.is_empty())
            .cloned()
            .or_else(|| user.as_ref().map(|u| u.username.clone()).filter(|u| !u.is_empty()))
            .unwrap_or_else(|| user_id.to_string()[..8.min(user_id.to_string().len())].to_string());

        let username = user
            .as_ref()
            .map(|u| u.username.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| user_id.to_string());

        let initials = get_initials(&display_name);

        let presence = match profile.as_ref().map(|p| p.presence) {
            Some(chatter_core::domain::user::PresenceStatus::Online) => "online",
            Some(chatter_core::domain::user::PresenceStatus::Away) => "away",
            Some(chatter_core::domain::user::PresenceStatus::DoNotDisturb) => "dnd",
            _ => "offline",
        };

        let bio = profile
            .as_ref()
            .and_then(|p| p.status_message.as_ref())
            .map(|m| m.text.clone())
            .unwrap_or_default();

        let user_data = user_state.get();
        let is_friend = user_data.friends.contains(user_id);
        let is_blocked = ui_ctx.map(|ctx| ctx.is_profile_user_blocked(user_id)).unwrap_or(false);

        w.set_profile_display_name(display_name.into());
        w.set_profile_username(username.into());
        w.set_profile_initials(initials.into());
        w.set_profile_avatar_source(slint::Image::default());
        w.set_profile_presence(presence.into());
        w.set_profile_bio(bio.into());
        // Check verification state
        let is_verified = ui_ctx
            .and_then(|ctx| ctx.is_peer_verified(user_id).ok())
            .unwrap_or(false);

        w.set_profile_is_self(false);
        w.set_profile_is_friend(is_friend);
        w.set_profile_is_blocked(is_blocked);
        w.set_profile_is_verified(is_verified);
    }

    w.set_active_modal("user-profile".into());
}

pub fn wire_profile_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {

    // Add friend
    let weak_window = window.as_weak();
    let ui_ctx_add = ui_ctx.clone();

    window.on_profile_add_friend(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_add.clone();

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
                ui_ctx.send_friend_request(&friend_id, None),
            ).await {
                Ok(Ok(_)) => {
                    tracing::info!("Friend request sent to {}", friend_id);
                    // Core emits `FriendRequestsUpdated` after send — the
                    // domain-handler dispatcher refreshes the list. No
                    // manual list_friend_requests + reshape here.
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to send friend request: {}", e);
                }
                Err(_) => {
                    tracing::error!("Friend request timed out");
                }
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_profile_loading(false);
                }
            });
        });
    });

    // Remove friend
    let weak_window = window.as_weak();
    let ui_ctx_remove = ui_ctx.clone();

    window.on_profile_remove_friend(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_remove.clone();

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
                ui_ctx.remove_friend(&friend_id),
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
    let ui_ctx_block = ui_ctx.clone();

    window.on_profile_block_user(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_block.clone();

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

            match ui_ctx.block_profile_user(&user_id).await {
                Ok(()) => {
                    crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                        weak.clone(), ui_ctx.clone(),
                    );
                    crate::bin_ui::features::modals::friends::refresh_blocked_users(
                        weak.clone(), &ui_ctx,
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(true);
                            w.set_profile_is_friend(false);
                            tracing::info!("Blocked user: {}", user_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to block user: {}", e);
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
    let ui_ctx_unblock = ui_ctx.clone();

    window.on_profile_unblock_user(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_unblock.clone();

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

            match ui_ctx.unblock_profile_user(&user_id).await {
                Ok(()) => {
                    crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                        weak.clone(), ui_ctx.clone(),
                    );
                    crate::bin_ui::features::modals::friends::refresh_blocked_users(
                        weak.clone(), &ui_ctx,
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(false);
                            tracing::info!("Unblocked user: {}", user_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to unblock user: {}", e);
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
    let ui_ctx_dm = ui_ctx.clone();

    window.on_profile_send_message(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_dm.clone();

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

            // Ensure mailbox manager initialized and token pool populated.
            if let Err(e) = ui_ctx.ensure_mailbox_ready(5).await {
                tracing::error!("Failed to prepare mailbox: {}", e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_loading_messages(false);
                    }
                });
                return;
            }

            // Get or create mailbox for this DM conversation (TTL: 7 days).
            match ui_ctx.get_or_create_mailbox(&conversation_id, 604800).await {
                Ok(mailbox) => {
                    tracing::info!(
                        "DM mailbox ready for conversation with {}: mailbox_id={}",
                        peer_id,
                        mailbox.mailbox_id
                    );

                    // Get peer display name via façade
                    let peer_name = match ui_ctx.get_user_profile(&peer_id).await {
                        Ok(profile) => {
                            if profile.display_name.is_empty() {
                                peer_id.to_string()
                            } else {
                                profile.display_name
                            }
                        }
                        Err(_) => peer_id.to_string(),
                    };

                    // Find the DM channel for this peer
                    let dm_channel = ui_ctx.get_dm_channel_for_peer(&peer_id);

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
                        avatar_bytes: None,
                        avatar_decoded: None,
                        presence_status: "offline".to_string(),
                        last_message: String::new(),
                        timestamp: String::new(),
                        unread_count: unread as u32,
                        is_typing: false,
                        is_anon: false,
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
                            // Trigger DM selection via the canonical handler so
                            // chat_state + highlighting stay in sync.
                            w.invoke_dm_selected(channel_id_for_ui.into());
                            w.set_channel_name(peer_name.into());
                            w.set_channel_type("dm".into());
                            w.set_is_dm(true);
                            w.set_header_presence_status(w.get_profile_presence());
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
    let ui_ctx_save = ui_ctx.clone();
    window.on_edit_profile_save(move |display_name: slint::SharedString, bio: slint::SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_save.clone();
        let display_name = display_name.to_string();
        let bio = bio.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_edit_profile_loading(true);
                    w.set_edit_profile_error("".into());
                }
            });

            // Map bio to status message
            let status_msg = if !bio.is_empty() {
                Some(chatter_core::domain::user::StatusMessage::new(bio))
            } else {
                None
            };

            // Update profile
            tracing::info!(
                "Updating user profile - display_name: {}, bio: {:?}",
                display_name, status_msg.as_ref().map(|m| &m.text)
            );

            match ui_ctx
                .update_user_profile(Some(display_name.clone()), status_msg.clone())
                .await
            {
                Ok(profile) => {
                    tracing::info!(
                        "Profile updated - display_name: {}, bio: {:?}",
                        profile.display_name, profile.status_message.as_ref().map(|m| &m.text)
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
                let config = chatter_core::media::FilePickerConfig::new()
                    .with_title("Select Profile Picture")
                    .with_filter(chatter_core::media::FileFilter::images());

                if let Some(path) = chatter_core::media::FilePicker::pick_file(config).await {
                    match std::fs::read(&path) {
                        Ok(data) => {
                            // Store the path for later use
                            PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));

                            // Create the image on the UI thread
                            let _ = slint::invoke_from_event_loop(move || {
                                // Process image on UI thread where it's safe
                                match crate::bin_ui::slint_conversions::memory_to_slint_image(&data) {
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

    // View safety number — generates and displays the 60-digit fingerprint
    let weak_window = window.as_weak();
    let ui_ctx_safety = ui_ctx.clone();
    window.on_profile_view_safety_number(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_safety.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("No profile user ID for safety number");
                return;
            }
        };

        tokio::spawn(async move {
            let peer_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID for safety number: {}", e);
                    return;
                }
            };

            match ui_ctx.get_safety_number(&peer_id).await {
                Ok(safety_number) => {
                    // Format as 12 groups of 5 digits
                    let formatted: String = safety_number
                        .chars()
                        .collect::<Vec<_>>()
                        .chunks(5)
                        .map(|chunk| chunk.iter().collect::<String>())
                        .collect::<Vec<_>>()
                        .join(" ");

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_safety_number(formatted.into());
                            w.set_profile_show_safety_number(true);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to generate safety number: {}", e);
                }
            }
        });
    });

    // Toggle verified state
    let weak_window = window.as_weak();
    let ui_ctx_verify = ui_ctx.clone();
    window.on_profile_toggle_verified(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_verify.clone();

        let (profile_user_id, currently_verified) = match weak.upgrade() {
            Some(w) => (w.get_profile_user_id().to_string(), w.get_profile_is_verified()),
            None => return,
        };

        if profile_user_id.is_empty() {
            return;
        }

        let peer_id: UserId = match profile_user_id.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid user ID for verification toggle: {}", e);
                return;
            }
        };

        if currently_verified {
            // Unverify — sync operation
            match ui_ctx.mark_peer_unverified(&peer_id) {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_is_verified(false);
                        }
                    });
                }
                Err(e) => tracing::error!("Failed to unverify peer: {}", e),
            }
        } else {
            // Verify — async (needs to generate safety number first)
            tokio::spawn(async move {
                match ui_ctx.mark_peer_verified(&peer_id).await {
                    Ok(()) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_profile_is_verified(true);
                            }
                        });
                    }
                    Err(e) => tracing::error!("Failed to verify peer: {}", e),
                }
            });
        }
    });
}

/// Broadcast profile update to all anon DM conversations and anon groups
async fn broadcast_profile_update(ui_ctx: &Arc<ChatterClient>, profile: &chatter_core::domain::user::Profile) {
    let display_name = Some(profile.display_name.clone());
    // The server profile no longer carries a plaintext avatar URL; the avatar
    // travels as encrypted bytes inside send_anon_dm_profile_update itself.
    let avatar_url: Option<String> = None;
    let bio = profile.status_message.as_ref().map(|m| m.text.clone());

    tracing::info!(
        "Broadcasting profile update - display_name: {:?}, bio: {:?}",
        display_name, bio
    );
    
    // Broadcast to all anon DM conversations
    let conversations = ui_ctx.list_anon_dm_conversations();

    for conv in &conversations {
        if !conv.is_ready() {
            continue;
        }

        let conv_id = conv.id;
        let ui_ctx_profile = ui_ctx.clone();
        let display_name = display_name.clone();
        let avatar_url = avatar_url.clone();
        let bio = bio.clone();

        tokio::spawn(async move {
            if let Err(e) = ui_ctx_profile.send_anon_dm_profile_update(&conv_id, display_name, avatar_url, bio).await {
                tracing::debug!("Failed to send profile update to anon DM: {}", e);
            }
        });
    }
    
    // Broadcast to anon groups where user is owner
    let ui_ctx_bcast = ui_ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = ui_ctx_bcast.broadcast_profile_to_owned_groups().await {
            tracing::warn!("Failed to broadcast profile to owned groups: {}", e);
        }
    });
    
    tracing::info!("Broadcasted profile update to {} anon DM conversations", conversations.len());
}
