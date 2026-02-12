//! Profile modal callbacks.

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::UserId;
use chatter_linux::services::user_service::UserService;

use crate::MainWindow;

pub fn wire_profile_actions(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let user_repo = ctx.user_repo.clone();

    // Add friend
    let weak_window = window.as_weak();
    let grpc_pool_add = grpc_pool.clone();
    let settings_repo_add = settings_repo.clone();
    let user_repo_add = user_repo.clone();

    window.on_profile_add_friend(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_add.clone();
        let settings_repo = settings_repo_add.clone();
        let user_repo = user_repo_add.clone();

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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service
                .send_friend_request(&mut user_client, &friend_id, None)
                .await
            {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            tracing::info!("Friend request sent to {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to send friend request: {}", e);
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
    let grpc_pool_remove = grpc_pool.clone();
    let settings_repo_remove = settings_repo.clone();
    let user_repo_remove = user_repo.clone();

    window.on_profile_remove_friend(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_remove.clone();
        let settings_repo = settings_repo_remove.clone();
        let user_repo = user_repo_remove.clone();

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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.remove_friend(&mut user_client, &friend_id).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_friend(false);
                            tracing::info!("Removed friend: {}", friend_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to remove friend: {}", e);
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
    let grpc_pool_block = grpc_pool.clone();
    let settings_repo_block = settings_repo.clone();
    let user_repo_block = user_repo.clone();

    window.on_profile_block_user(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_block.clone();
        let settings_repo = settings_repo_block.clone();
        let user_repo = user_repo_block.clone();

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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.block_user(&mut user_client, &user_id).await {
                Ok(()) => {
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
    let grpc_pool_unblock = grpc_pool.clone();
    let settings_repo_unblock = settings_repo.clone();
    let user_repo_unblock = user_repo.clone();

    window.on_profile_unblock_user(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_unblock.clone();
        let settings_repo = settings_repo_unblock.clone();
        let user_repo = user_repo_unblock.clone();

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

            let user_service = UserService::new(settings_repo, user_repo);
            let mut user_client = chatter_linux::network::grpc::user_client::UserClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match user_service.unblock_user(&mut user_client, &user_id).await {
                Ok(()) => {
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
    let mailbox_manager = ctx.mailbox_manager.clone();
    let user_repo_dm = user_repo.clone();
    let chat_state = ctx.chat_state.clone();

    window.on_profile_send_message(move || {
        let weak = weak_window.clone();
        let mailbox_manager = mailbox_manager.clone();
        let user_repo = user_repo_dm.clone();
        let _chat_state = chat_state.clone(); // Reserved for future DM state tracking

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

                    // Get peer display name
                    let peer_name = user_repo
                        .get_profile(&peer_id)
                        .ok()
                        .flatten()
                        .map(|p| p.display_name)
                        .or_else(|| {
                            user_repo
                                .get(&peer_id)
                                .ok()
                                .flatten()
                                .map(|u| u.username)
                        })
                        .unwrap_or_else(|| "Unknown".to_string());

                    // Update chat state to active DM
                    // Note: For DMs, we use the peer_id as a pseudo-channel ID
                    // A proper implementation would have a DM-specific state

                    // Update UI to show the DM
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_loading_messages(false);
                            w.set_selected_dm_id(profile_id_for_ui.into());
                            w.set_selected_channel_id("".into());
                            w.set_channel_name(peer_name.into());
                            w.set_channel_type("dm".into());
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
}
