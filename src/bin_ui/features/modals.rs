//! Modal callbacks.
//!
//! Handles modal dialogs (channel creation, profile actions, friend requests,
//! settings, sessions).

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::UserId;
use chatter_linux::services::channel_service::ChannelService;
use chatter_linux::services::user_service::UserService;

use crate::{MainWindow, SessionInfo};

/// Wire modal callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_create_channel(window, ctx);
    wire_profile_actions(window, ctx);
    wire_friend_requests(window, ctx);
    wire_settings_callbacks(window, ctx);
    wire_session_callbacks(window, ctx);
}

fn wire_create_channel(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_create_channel(
        move |name: SharedString, description: SharedString, template: SharedString| {
            let weak = weak_window.clone();
            let grpc_pool = grpc_pool.clone();
            let settings_repo = settings_repo.clone();
            let channel_repo = channel_repo.clone();
            let name = name.to_string();
            let description = description.to_string();
            let template = template.to_string();

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.set_create_channel_loading(true);
                        w.set_create_channel_error("".into());
                    }
                });

                let channel_type = match template.as_str() {
                    "announcement" => ChannelType::Private,
                    "ephemeral" => ChannelType::Private,
                    _ => ChannelType::Public,
                };

                let channel_service = ChannelService::new(settings_repo, channel_repo);

                let mut channel_client =
                    chatter_linux::network::grpc::channel_client::ChannelClient::new(
                        grpc_pool.channel(),
                        grpc_pool.auth_clone(),
                    );

                let description_opt = if description.is_empty() {
                    None
                } else {
                    Some(description.as_str())
                };

                match channel_service
                    .create_channel(
                        &mut channel_client,
                        &name,
                        channel_type,
                        None,
                        description_opt,
                        None,
                        None,
                        vec![],
                    )
                    .await
                {
                    Ok(channel) => {
                        let channel_id_str = channel.id.to_string();
                        let channel_name = channel.name.clone();

                        let weak_success = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_success.upgrade() {
                                w.set_create_channel_loading(false);
                                w.set_create_channel_name("".into());
                                w.set_create_channel_description("".into());
                                w.set_create_channel_template("".into());
                                w.set_active_modal("".into());
                                w.set_selected_channel_id(channel_id_str.into());
                                w.set_channel_name(channel_name.into());
                            }
                        });

                        tracing::info!("Created channel: {}", channel.id);
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        tracing::error!("Failed to create channel: {}", error_msg);

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_create_channel_loading(false);
                                w.set_create_channel_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );
}

fn wire_profile_actions(window: &MainWindow, ctx: &AppContext) {
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

    window.on_profile_send_message(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let profile_user_id = w.get_profile_user_id().to_string();
                w.set_active_modal("".into());
                tracing::info!(
                    "Send message to user: {} (DM channel creation not yet implemented)",
                    profile_user_id
                );
            }
        });
    });
}

fn wire_friend_requests(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let user_repo = ctx.user_repo.clone();

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

    window.on_friend_request_accept(move |user_id: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_accept.clone();
        let settings_repo = settings_repo_accept.clone();
        let user_repo = user_repo_accept.clone();
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
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_requests_loading(false);
                            tracing::info!("Accepted friend request from {}", friend_id);
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

fn wire_settings_callbacks(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();

    // Change password
    let weak_window = window.as_weak();
    let grpc_pool_pw = grpc_pool.clone();
    let settings_repo_pw = settings_repo.clone();
    let auth_interceptor_pw = auth_interceptor.clone();

    window.on_settings_change_password(move |current: SharedString, new: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_pw.clone();
        let settings_repo = settings_repo_pw.clone();
        let auth_interceptor = auth_interceptor_pw.clone();
        let current = current.to_string();
        let new = new.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_password_changing(true);
                    w.set_settings_password_error("".into());
                }
            });

            use chatter_linux::services::account_service::AccountService;
            let account_service = AccountService::new(settings_repo.clone(), auth_interceptor);

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match account_service
                .change_password(&mut auth_client, &current, &new)
                .await
            {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_current_password("".into());
                            w.set_settings_new_password("".into());
                            w.set_settings_confirm_password("".into());
                        }
                    });
                    tracing::info!("Password changed successfully");
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to change password: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_password_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Read receipts toggle
    let settings_repo_read = settings_repo.clone();
    window.on_settings_read_receipts_changed(move |enabled: bool| {
        let settings_repo = settings_repo_read.clone();
        if let Err(e) = settings_repo.set(
            chatter_linux::storage::settings_keys::SEND_READ_RECEIPTS,
            &enabled,
        ) {
            tracing::error!("Failed to save read receipts setting: {}", e);
        } else {
            tracing::debug!("Read receipts setting changed to: {}", enabled);
        }
    });

    // Typing indicators toggle
    let settings_repo_typing = settings_repo.clone();
    window.on_settings_typing_indicators_changed(move |enabled: bool| {
        let settings_repo = settings_repo_typing.clone();
        if let Err(e) = settings_repo.set(
            chatter_linux::storage::settings_keys::SHOW_TYPING_INDICATORS,
            &enabled,
        ) {
            tracing::error!("Failed to save typing indicators setting: {}", e);
        } else {
            tracing::debug!("Typing indicators setting changed to: {}", enabled);
        }
    });

    // Copy fingerprint
    let key_store_fp = key_store.clone();
    window.on_settings_copy_fingerprint(move || {
        let key_store = key_store_fp.clone();

        use chatter_linux::crypto::signal::IdentityKeyManager;
        let identity_manager = IdentityKeyManager::new(key_store);

        let fingerprint = match identity_manager.get_public_key() {
            Ok(public_key) => {
                let bytes = public_key.serialize();
                bytes
                    .chunks(4)
                    .map(|chunk| chunk.iter().map(|b| format!("{:02X}", b)).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            Err(e) => {
                tracing::error!("Failed to get identity key for fingerprint: {}", e);
                return;
            }
        };

        use chatter_linux::system::Clipboard;
        match Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(e) = clipboard.set_text(&fingerprint) {
                    tracing::error!("Failed to copy fingerprint to clipboard: {}", e);
                } else {
                    tracing::info!("Fingerprint copied to clipboard");
                }
            }
            Err(e) => {
                tracing::error!("Failed to access clipboard: {}", e);
            }
        }
    });

    // Sealed sender toggle
    let settings_repo_sealed = settings_repo.clone();
    window.on_settings_sealed_sender_changed(move |unrestricted: bool| {
        let settings_repo = settings_repo_sealed.clone();
        if let Err(e) = settings_repo.set("privacy.sealed_sender_unrestricted", &unrestricted) {
            tracing::error!("Failed to save sealed sender setting: {}", e);
        } else {
            tracing::debug!(
                "Sealed sender unrestricted setting changed to: {}",
                unrestricted
            );
        }
    });

    // Refresh prekeys
    let weak_window = window.as_weak();
    let grpc_pool_keys = grpc_pool.clone();
    let key_store_refresh = key_store.clone();

    window.on_settings_refresh_keys(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_keys.clone();
        let key_store = key_store_refresh.clone();

        tokio::spawn(async move {
            use chatter_linux::crypto::signal::PreKeyManager;
            use chatter_linux::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord;
            use rand::SeedableRng;

            let prekey_manager = PreKeyManager::new(key_store);

            let mut rng = rand::rngs::StdRng::from_os_rng();

            let start_id = match prekey_manager.get_next_prekey_id() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Failed to get next prekey ID: {}", e);
                    return;
                }
            };

            let prekeys = match prekey_manager.generate_prekeys(&mut rng, start_id, 100) {
                Ok(pks) => pks,
                Err(e) => {
                    tracing::error!("Failed to generate prekeys: {}", e);
                    return;
                }
            };

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            let proto_prekeys: Vec<PreKeyRecord> = prekeys
                .iter()
                .filter_map(|pk| {
                    Some(PreKeyRecord {
                        id: u32::from(pk.id().ok()?),
                        public_key: pk.public_key().ok()?.serialize().to_vec(),
                    })
                })
                .collect();

            match auth_client.refresh_pre_keys(proto_prekeys).await {
                Ok(()) => match auth_client.get_pre_key_stats(0).await {
                    Ok(stats) => {
                        let count = stats.available_count;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_settings_prekey_count(count);
                            }
                        });
                        tracing::info!("Refreshed prekeys, {} now available", count);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to get prekey stats after refresh: {}", e);
                        tracing::info!("Prekeys refreshed successfully");
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to upload prekeys to server: {}", e);
                }
            }
        });
    });

    // Edit profile
    let weak_window = window.as_weak();
    window.on_settings_edit_profile(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_profile_is_self(true);
                w.set_active_modal("user-profile".into());
            }
        });
    });

    // Delete account (placeholder)
    let weak_window = window.as_weak();
    window.on_settings_delete_account(move || {
        let weak = weak_window.clone();
        tracing::info!("Delete account requested - showing confirmation would be needed");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Show delete confirmation modal
            }
        });
    });
}

fn wire_session_callbacks(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();

    // View sessions
    let weak_window = window.as_weak();
    let grpc_pool_sessions = grpc_pool.clone();
    let settings_repo_sessions = settings_repo.clone();

    window.on_settings_view_sessions(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_sessions.clone();
        let settings_repo = settings_repo_sessions.clone();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_sessions_loading(true);
                    w.set_active_modal("sessions".into());
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo);

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service.list_sessions(&mut auth_client).await {
                Ok(sessions) => {
                    let slint_sessions: Vec<SessionInfo> = sessions
                        .into_iter()
                        .map(|s| SessionInfo {
                            device_id: s.device_id as i32,
                            device_info: s.device_info.into(),
                            ip_address: s.ip_address.into(),
                            last_used: s.last_used.format("%Y-%m-%d %H:%M").to_string().into(),
                            created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                            is_current: s.is_current,
                        })
                        .collect();

                    let session_count = slint_sessions.len() as i32;

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                            w.set_sessions_loading(false);
                            w.set_settings_session_count(session_count);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to list sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke single session
    let weak_window = window.as_weak();
    let grpc_pool_revoke = grpc_pool.clone();
    let settings_repo_revoke = settings_repo.clone();

    window.on_revoke_session(move |device_id: i32| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_revoke.clone();
        let settings_repo = settings_repo_revoke.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo.clone());

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service
                .revoke_session(&mut auth_client, device_id as u32)
                .await
            {
                Ok(()) => {
                    match session_service.list_sessions(&mut auth_client).await {
                        Ok(sessions) => {
                            let slint_sessions: Vec<SessionInfo> = sessions
                                .into_iter()
                                .map(|s| SessionInfo {
                                    device_id: s.device_id as i32,
                                    device_info: s.device_info.into(),
                                    ip_address: s.ip_address.into(),
                                    last_used: s
                                        .last_used
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                        .into(),
                                    created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                                    is_current: s.is_current,
                                })
                                .collect();

                            let session_count = slint_sessions.len() as i32;

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                                    w.set_sessions_revoking(false);
                                    w.set_settings_session_count(session_count);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                    tracing::info!("Revoked session for device {}", device_id);
                }
                Err(e) => {
                    tracing::error!("Failed to revoke session: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke all other sessions
    let weak_window = window.as_weak();
    let grpc_pool_revoke_all = grpc_pool.clone();
    let settings_repo_revoke_all = settings_repo.clone();

    window.on_revoke_all_sessions(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_revoke_all.clone();
        let settings_repo = settings_repo_revoke_all.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            use chatter_linux::services::session_service::SessionService;
            let session_service = SessionService::new(settings_repo.clone());

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match session_service
                .revoke_all_other_sessions(&mut auth_client)
                .await
            {
                Ok(()) => {
                    match session_service.list_sessions(&mut auth_client).await {
                        Ok(sessions) => {
                            let slint_sessions: Vec<SessionInfo> = sessions
                                .into_iter()
                                .map(|s| SessionInfo {
                                    device_id: s.device_id as i32,
                                    device_info: s.device_info.into(),
                                    ip_address: s.ip_address.into(),
                                    last_used: s
                                        .last_used
                                        .format("%Y-%m-%d %H:%M")
                                        .to_string()
                                        .into(),
                                    created_at: s.created_at.format("%Y-%m-%d").to_string().into(),
                                    is_current: s.is_current,
                                })
                                .collect();

                            let session_count = slint_sessions.len() as i32;

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
                                    w.set_sessions_revoking(false);
                                    w.set_settings_session_count(session_count);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke all: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                    tracing::info!("Revoked all other sessions");
                }
                Err(e) => {
                    tracing::error!("Failed to revoke all sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });
}
