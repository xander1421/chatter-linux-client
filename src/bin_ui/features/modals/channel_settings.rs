//! Channel settings modal callbacks.
//!
//! Handles channel settings including:
//! - Editing channel name/description
//! - Toggling channel features (reactions, threads, attachments)
//! - Notification preferences
//! - Member management
//! - Archive/leave/delete channel

use std::sync::Arc;

use futures::StreamExt;
use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::ChannelType;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::proto::chat_v1::Channel as ChannelProto;
use chatter_linux::state::AuthState;
use chatter_linux::storage::{ChannelRepository, UserRepository};

use crate::{ChannelMemberData, MainWindow};

/// Wire channel settings modal callbacks.
pub fn wire_channel_settings(window: &MainWindow, ctx: &AppContext) {
    wire_save_settings(window, ctx);
    wire_toggle_setting(window, ctx);
    wire_notification_settings(window, ctx);
    wire_member_management(window, ctx);
    wire_channel_actions(window, ctx);
}

/// Open the channel settings modal with channel data.
pub fn open_channel_settings(
    weak_window: slint::Weak<MainWindow>,
    channel_repo: Arc<ChannelRepository>,
    user_repo: Arc<UserRepository>,
    auth_state: AuthState,
    grpc_pool: Arc<GrpcClientPool>,
    channel_id: ChannelId,
) {
    tokio::spawn(async move {
        // Set loading state
        {
            let weak = weak_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_channel_settings_loading(true);
                    w.set_channel_settings_error("".into());
                }
            });
        }

        // Get channel from local storage
        let channel = match channel_repo.get(&channel_id) {
            Ok(Some(c)) => c,
            Ok(None) => {
                let weak = weak_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_loading(false);
                        w.set_channel_settings_error("Channel not found".into());
                    }
                });
                return;
            }
            Err(e) => {
                tracing::error!("Failed to get channel: {}", e);
                let weak = weak_window.clone();
                let err_msg = e.to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_loading(false);
                        w.set_channel_settings_error(err_msg.into());
                    }
                });
                return;
            }
        };

        // Determine current user permissions
        let current_user_id = auth_state.get().current_user_id;
        let is_owner = current_user_id.as_ref() == Some(&channel.owner_id);
        let is_dm = channel.channel_type == ChannelType::DirectMessage;

        // For DMs, we don't have admin capabilities
        let can_edit = is_owner && !is_dm;
        let can_manage_members = is_owner && !is_dm;
        let can_archive = is_owner && !is_dm;
        let can_delete = is_owner && !is_dm;

        // Format created_at timestamp
        let created_at = channel
            .created_at
            .with_timezone(&chrono::Local)
            .format("%b %d, %Y")
            .to_string();

        // Get member count and member list
        let members: Vec<ChannelMemberData> = if !is_dm {
            // Fetch members from server for group channels
            let mut client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            match client.get_channel_members(&channel_id.to_string(), 100, 0).await {
                Ok(response) => {
                    let member_list = response.members;
                    let mut member_data = Vec::with_capacity(member_list.len());

                    for m in member_list {
                        // Get user profile for display name
                        let (display_name, username) = if let Ok(user_id) =
                            m.user_id.parse::<uuid::Uuid>()
                        {
                            let user_id = chatter_linux::domain::ids::UserId::from(user_id);
                            let display = user_repo
                                .get_profile(&user_id)
                                .ok()
                                .flatten()
                                .map(|p| p.display_name)
                                .unwrap_or_else(|| "Unknown".to_string());

                            let uname = user_repo
                                .get(&user_id)
                                .ok()
                                .flatten()
                                .map(|u| u.username)
                                .unwrap_or_else(|| "unknown".to_string());

                            (display, uname)
                        } else {
                            ("Unknown".to_string(), "unknown".to_string())
                        };

                        let role = match m.role {
                            0 => "member",
                            1 => "moderator",
                            2 => "admin",
                            3 => "owner",
                            _ => "member",
                        };

                        member_data.push(ChannelMemberData {
                            user_id: m.user_id.into(),
                            display_name: display_name.into(),
                            username: username.into(),
                            role: role.into(),
                            joined_at: "".into(), // Not available from proto
                        });
                    }

                    member_data
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch members: {}", e);
                    vec![]
                }
            }
        } else {
            vec![]
        };

        let member_count = members.len() as i32;

        // Channel type string
        let channel_type_str = match channel.channel_type {
            ChannelType::Public => "public",
            ChannelType::Private => "private",
            ChannelType::DirectMessage => "dm",
            ChannelType::SavedMessages => "saved",
        };

        // Set all the properties
        let channel_name = channel.name.clone();
        let channel_description = channel.description.clone().unwrap_or_default();
        let channel_id_str = channel_id.to_string();
        let is_archived = channel.archived;

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                w.set_channel_settings_id(channel_id_str.into());
                w.set_channel_settings_name(channel_name.into());
                w.set_channel_settings_description(channel_description.into());
                w.set_channel_settings_type(channel_type_str.into());
                w.set_channel_settings_archived(is_archived);
                w.set_channel_settings_created_at(created_at.into());
                w.set_channel_settings_member_count(member_count);
                w.set_channel_settings_can_edit(can_edit);
                w.set_channel_settings_can_manage_members(can_manage_members);
                w.set_channel_settings_can_archive(can_archive);
                w.set_channel_settings_can_delete(can_delete);
                w.set_channel_settings_is_owner(is_owner);
                w.set_channel_settings_members(ModelRc::new(VecModel::from(members)));
                w.set_channel_settings_loading(false);
                w.set_active_modal("channel-settings".into());

                tracing::info!("Channel settings modal opened for channel {}", channel_id);
            }
        });
    });
}

fn wire_save_settings(window: &MainWindow, ctx: &AppContext) {
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_channel_settings_save(move |name: slint::SharedString, description: slint::SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let channel_repo = channel_repo.clone();
        let name_str = name.to_string();
        let description_str = description.to_string();

        tokio::spawn(async move {
            // Get current channel ID
            let channel_id = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if channel_id.is_empty() {
                return;
            }

            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            // Get current channel to preserve existing fields
            let local_channel = match channel_repo.get(&channel_uuid) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    tracing::error!("Channel not found in local storage");
                    return;
                }
                Err(e) => {
                    tracing::error!("Failed to get channel: {}", e);
                    return;
                }
            };

            // Build the Channel proto with updated fields
            let channel_proto = ChannelProto {
                id: channel_id.clone(),
                name: name_str.clone(),
                description: description_str.clone(),
                r#type: match local_channel.channel_type {
                    ChannelType::Public => 0,
                    ChannelType::Private => 1,
                    ChannelType::DirectMessage => 2,
                    ChannelType::SavedMessages => 3,
                },
                owner_id: local_channel.owner_id.to_string(),
                img_url: local_channel.image_url.clone().unwrap_or_default(),
                archived: local_channel.archived,
                created_at: local_channel.created_at.timestamp_nanos_opt().unwrap_or(0) as u64,
                config: None, // Keep existing config
                members: vec![], // Don't update members
            };

            // Create field mask to only update name and description
            let update_mask = Some(prost_types::FieldMask {
                paths: vec!["name".to_string(), "description".to_string()],
            });

            // Send update to server
            let mut client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            match client.update_channel(&channel_id, channel_proto, update_mask).await {
                Ok(mut stream) => {
                    // Process the stream to completion
                    while let Some(result) = stream.next().await {
                        match result {
                            Ok(event) => {
                                tracing::debug!("Channel update event: {:?}", event);
                            }
                            Err(e) => {
                                tracing::error!("Channel update stream error: {}", e);
                                let err_msg = e.to_string();
                                let weak_err = weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak_err.upgrade() {
                                        w.set_channel_settings_error(err_msg.into());
                                    }
                                });
                                return;
                            }
                        }
                    }

                    // Update local storage after server confirms
                    if let Ok(Some(mut channel)) = channel_repo.get(&channel_uuid) {
                        channel.name = name_str.clone();
                        channel.description = if description_str.is_empty() {
                            None
                        } else {
                            Some(description_str.clone())
                        };
                        if let Err(e) = channel_repo.update(&channel) {
                            tracing::error!("Failed to update channel in local storage: {}", e);
                        }
                    }

                    tracing::info!("Channel settings saved (name: {}, desc: {})", name_str, description_str);

                    // Close modal
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_active_modal("none".into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to update channel: {}", e);
                    let err_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_channel_settings_error(err_msg.into());
                        }
                    });
                }
            }
        });
    });
}

fn wire_toggle_setting(window: &MainWindow, _ctx: &AppContext) {
    let weak_window = window.as_weak();

    window.on_channel_settings_toggle_setting(move |setting: slint::SharedString, value: bool| {
        let weak = weak_window.clone();
        let setting_str = setting.to_string();

        // Update local state immediately
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                match setting_str.as_str() {
                    "reactions" => w.set_channel_settings_allow_reactions(value),
                    "threads" => w.set_channel_settings_allow_threads(value),
                    "attachments" => w.set_channel_settings_allow_attachments(value),
                    _ => tracing::warn!("Unknown setting: {}", setting_str),
                }
            }
        });

        // TODO: Persist to server when API supports it
        tracing::info!("Channel setting toggled: {} = {}", setting, value);
    });
}

fn wire_notification_settings(window: &MainWindow, _ctx: &AppContext) {
    // Change notification preference
    let weak_window = window.as_weak();

    window.on_channel_settings_change_notification(move |preference: slint::SharedString| {
        let weak = weak_window.clone();
        let pref_str = preference.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_channel_settings_notification_pref(pref_str.into());
            }
        });

        // TODO: Persist notification preference
        tracing::info!("Notification preference changed to: {}", preference);
    });

    // Toggle mute
    let weak_window = window.as_weak();

    window.on_channel_settings_toggle_mute(move |muted: bool| {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_channel_settings_muted(muted);
            }
        });

        // TODO: Persist mute setting
        tracing::info!("Channel muted: {}", muted);
    });
}

fn wire_member_management(window: &MainWindow, ctx: &AppContext) {
    // Change member role
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_channel_settings_change_member_role(move |user_id: slint::SharedString, role: slint::SharedString| {
        let _weak = weak_window.clone();
        let _grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();
        let role_str = role.to_string();

        // TODO: Implement role change via gRPC when API supports it
        tracing::info!("Change member {} role to: {}", user_id_str, role_str);
    });

    // Remove member
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_channel_settings_remove_member(move |user_id: slint::SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let user_id_str = user_id.to_string();

        tokio::spawn(async move {
            let channel_id = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if channel_id.is_empty() {
                return;
            }

            let mut client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            match client.kick_user(&channel_id, &user_id_str, None).await {
                Ok(_) => {
                    tracing::info!("Member {} removed from channel", user_id_str);
                    // Refresh member list would go here
                }
                Err(e) => {
                    tracing::error!("Failed to remove member: {}", e);
                    let err_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_channel_settings_error(err_msg.into());
                        }
                    });
                }
            }
        });
    });
}

fn wire_channel_actions(window: &MainWindow, ctx: &AppContext) {
    // Archive channel
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_channel_settings_archive(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let channel_repo = channel_repo.clone();

        tokio::spawn(async move {
            let channel_id = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if channel_id.is_empty() {
                return;
            }

            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            // Toggle archive status
            let currently_archived = weak
                .upgrade()
                .map(|w| w.get_channel_settings_archived())
                .unwrap_or(false);

            let mut client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            let success = if currently_archived {
                match client.unarchive_channel(&channel_id).await {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::error!("Failed to unarchive channel: {}", e);
                        let err_msg = e.to_string();
                        let weak_err = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_err.upgrade() {
                                w.set_channel_settings_error(err_msg.into());
                            }
                        });
                        false
                    }
                }
            } else {
                match client.archive_channel(&channel_id).await {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::error!("Failed to archive channel: {}", e);
                        let err_msg = e.to_string();
                        let weak_err = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_err.upgrade() {
                                w.set_channel_settings_error(err_msg.into());
                            }
                        });
                        false
                    }
                }
            };

            if success {
                tracing::info!(
                    "Channel {} {}",
                    channel_id,
                    if !currently_archived { "archived" } else { "unarchived" }
                );

                // Update local storage
                if let Ok(Some(mut channel)) = channel_repo.get(&channel_uuid) {
                    channel.archived = !currently_archived;
                    let _ = channel_repo.update(&channel);
                }

                // Update UI and close modal
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_active_modal("none".into());
                    }
                });
            }
        });
    });

    // Leave channel
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_channel_settings_leave(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let channel_repo = channel_repo.clone();

        tokio::spawn(async move {
            let channel_id = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if channel_id.is_empty() {
                return;
            }

            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            let mut client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            match client.leave_channel(&channel_id).await {
                Ok(_) => {
                    tracing::info!("Left channel {}", channel_id);

                    // Remove from local storage
                    let _ = channel_repo.delete(&channel_uuid);

                    // Close modal and clear selection
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_active_modal("none".into());
                            w.set_selected_channel_id("".into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to leave channel: {}", e);
                    let err_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_channel_settings_error(err_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Delete channel
    let weak_window = window.as_weak();
    let grpc_pool = ctx.grpc_pool.clone();
    let channel_repo = ctx.channel_repo.clone();

    window.on_channel_settings_delete(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool.clone();
        let channel_repo = channel_repo.clone();

        tokio::spawn(async move {
            let channel_id = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if channel_id.is_empty() {
                return;
            }

            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            let mut client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            match client.delete_channel(&channel_id).await {
                Ok(_) => {
                    tracing::info!("Deleted channel {}", channel_id);

                    // Remove from local storage
                    let _ = channel_repo.delete(&channel_uuid);

                    // Close modal and clear selection
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_active_modal("none".into());
                            w.set_selected_channel_id("".into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to delete channel: {}", e);
                    let err_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_channel_settings_error(err_msg.into());
                        }
                    });
                }
            }
        });
    });
}
