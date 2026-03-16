//! Channel settings modal callbacks.

use std::collections::HashSet;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::app::UIContext;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::ChannelType;
use chatter_core::services::channel::ChannelService;
use chatter_core::services::user::UserService;

use crate::{ChannelMemberData, InviteUser, MainWindow, PrivateGroupFriend};

/// Wire channel settings modal callbacks.
pub fn wire_channel_settings(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    wire_save_settings(window, ui_ctx);
    wire_toggle_setting(window, ui_ctx);
    wire_notification_settings(window, ui_ctx);
    wire_member_management(window, ui_ctx);
    wire_channel_actions(window, ui_ctx);
    wire_invite_members(window, ui_ctx);
    wire_invite_channel_members(window, ui_ctx);
}

/// Open the channel settings modal with channel data.
pub fn open_channel_settings(
    weak_window: slint::Weak<MainWindow>,
    channel_service: Arc<ChannelService>,
    user_service: Arc<UserService>,
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
        let channel = match channel_service.get_channel_local(&channel_id) {
            Ok(Some(c)) => c,
            _ => {
                let weak = weak_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_loading(false);
                        w.set_channel_settings_error("Channel not found".into());
                    }
                });
                return;
            }
        };

        let current_user_id = channel_service.current_user_id().ok();
        let is_owner = current_user_id.as_ref() == Some(&channel.owner_id);
        let is_dm = channel.channel_type == ChannelType::DirectMessage;

        let can_edit = is_owner && !is_dm;
        let can_manage_members = is_owner && !is_dm;
        let can_archive = is_owner && !is_dm;
        let can_delete = is_owner && !is_dm;

        let created_at = channel.created_at.with_timezone(&chrono::Local).format("%b %d, %Y").to_string();

        let members: Vec<ChannelMemberData> = if !is_dm {
            match channel_service.get_channel_members(&channel_id, 100, 0).await {
                Ok(memberships) => {
                    let mut data = Vec::new();
                    for m in memberships {
                        let display = user_service.get_profile_local(&m.user_id).ok().flatten().map(|p| p.display_name).unwrap_or_else(|| "Unknown".to_string());
                        let uname = user_service.get_user_local(&m.user_id).ok().flatten().map(|u| u.username).unwrap_or_else(|| "unknown".to_string());
                        let role = match m.role {
                            chatter_core::domain::channel::MemberRole::Member => "member",
                            chatter_core::domain::channel::MemberRole::Moderator => "moderator",
                            chatter_core::domain::channel::MemberRole::Admin => "admin",
                            chatter_core::domain::channel::MemberRole::Owner => "owner",
                        };
                        data.push(ChannelMemberData {
                            user_id: m.user_id.to_string().into(),
                            display_name: display.into(),
                            username: uname.into(),
                            role: role.into(),
                            joined_at: "".into(),
                        });
                    }
                    data
                }
                Err(_) => vec![]
            }
        } else {
            vec![]
        };

        let member_count = members.len() as i32;
        let channel_type_str = match channel.channel_type {
            ChannelType::Public => "public",
            ChannelType::Private => "private",
            ChannelType::DirectMessage => "dm",
            ChannelType::SavedMessages => "saved",
        };

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
            }
        });
    });
}

fn wire_save_settings(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();

    window.on_channel_settings_save(move |name, description| {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        let name_str = name.to_string();
        let desc_str = description.to_string();

        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            let desc_opt = if desc_str.is_empty() { None } else { Some(desc_str.as_str()) };
            match channel_service.update_channel(&channel_uuid, Some(&name_str), desc_opt, None, None).await {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() { w.set_active_modal("none".into()); }
                    });
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() { w.set_channel_settings_error(err_msg.into()); }
                    });
                }
            }
        });
    });
}

fn wire_toggle_setting(window: &MainWindow, _ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    window.on_channel_settings_toggle_setting(move |setting, value| {
        let weak = weak_window.clone();
        let setting_str = setting.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                match setting_str.as_str() {
                    "reactions" => w.set_channel_settings_allow_reactions(value),
                    "threads" => w.set_channel_settings_allow_threads(value),
                    "attachments" => w.set_channel_settings_allow_attachments(value),
                    _ => {}
                }
            }
        });
    });
}

fn wire_notification_settings(window: &MainWindow, _ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    window.on_channel_settings_change_notification(move |preference| {
        let weak = weak_window.clone();
        let pref_str = preference.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() { w.set_channel_settings_notification_pref(pref_str.into()); }
        });
    });

    let weak_window = window.as_weak();
    window.on_channel_settings_toggle_mute(move |muted| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() { w.set_channel_settings_muted(muted); }
        });
    });
}

fn wire_member_management(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();

    window.on_channel_settings_change_member_role(move |user_id, role| {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        let user_id_str = user_id.to_string();
        let role_str = role.to_string();

        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let user_uuid = match user_id_str.parse::<uuid::Uuid>() {
                Ok(id) => chatter_core::domain::ids::UserId::from(id),
                Err(_) => return,
            };
            let new_role = match role_str.as_str() {
                "member" => chatter_core::domain::channel::MemberRole::Member,
                "moderator" => chatter_core::domain::channel::MemberRole::Moderator,
                "admin" => chatter_core::domain::channel::MemberRole::Admin,
                "owner" => chatter_core::domain::channel::MemberRole::Owner,
                _ => return,
            };
            let _ = channel_service.update_member_role(&channel_uuid, &user_uuid, new_role).await;
        });
    });

    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();
    let message_service = ui_ctx.message_service.clone();

    window.on_channel_settings_remove_member(move |user_id| {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_id_str = user_id.to_string();
        
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let user_uuid = match user_id_str.parse::<uuid::Uuid>() {
                Ok(id) => chatter_core::domain::ids::UserId::from(id),
                Err(_) => return,
            };

            if let Ok(skdm_opt) = channel_service.kick_user(&channel_uuid, &user_uuid, None).await {
                if let Some(skdm) = skdm_opt {
                    let _ = message_service.distribute_skdm(&channel_uuid, &skdm).await;
                }
            }
        });
    });
}

fn wire_channel_actions(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();

    window.on_channel_settings_archive(move || {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let currently_archived = weak.upgrade().map(|w| w.get_channel_settings_archived()).unwrap_or(false);
            let result = if currently_archived { channel_service.unarchive_channel(&channel_uuid).await } else { channel_service.archive_channel(&channel_uuid).await };
            if result.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() { w.set_active_modal("none".into()); }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();
    window.on_channel_settings_leave(move || {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            if channel_service.leave_channel(&channel_uuid).await.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_active_modal("none".into());
                        w.set_selected_channel_id("".into());
                    }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();
    window.on_channel_settings_delete(move || {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            if channel_service.delete_channel(&channel_uuid).await.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_active_modal("none".into());
                        w.set_selected_channel_id("".into());
                    }
                });
            }
        });
    });
}

fn wire_invite_members(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak_window = window.as_weak();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();

    window.on_channel_settings_invite_members(move || {
        let weak = weak_window.clone();
        let anon_group_service = anon_group_service.clone();
        let user_service = user_service.clone();
        let anon_dm_service = anon_dm_service.clone();

        tokio::spawn(async move {
            // Get the private group ID from channel settings
            let group_id_str = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if group_id_str.is_empty() {
                return;
            }

            let group_id: AnonGroupId = match group_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid private group ID: {}", e);
                    return;
                }
            };

            // Get existing members to filter them out
            let existing_members: HashSet<UserId> = anon_group_service
                .get_members(&group_id)
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.user_id)
                .collect();

            // Load available friends and anon DM peers
            let mut friend_data: Vec<PrivateGroupFriend> = Vec::new();
            let mut seen_user_ids: HashSet<UserId> = HashSet::new();

            // Load friends
            let friends = user_service.list_friends(100, 0).await.unwrap_or_default();
            for friendship in &friends {
                // Skip existing members
                if existing_members.contains(&friendship.friend_id) {
                    continue;
                }
                if seen_user_ids.contains(&friendship.friend_id) {
                    continue;
                }

                let profile = user_service.get_profile_local(&friendship.friend_id).ok().flatten();
                let display_name = profile
                    .as_ref()
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| friendship.friend_id.to_string());

                seen_user_ids.insert(friendship.friend_id);
                friend_data.push(PrivateGroupFriend {
                    user_id: friendship.friend_id.to_string().into(),
                    display_name: display_name.into(),
                    username: "".into(),
                    avatar_url: "".into(),
                    selected: false,
                    role: "member".into(),
                });
            }

            // Load anon DM peers (active conversations only)
            if let Ok(conversations) = anon_dm_service.list_conversations() {
                for dm in conversations {
                    // Skip existing members
                    if existing_members.contains(&dm.peer_id) {
                        continue;
                    }
                    if seen_user_ids.contains(&dm.peer_id) {
                        continue;
                    }
                    // Only include active conversations
                    if !dm.is_ready() {
                        continue;
                    }

                    let display_name = dm
                        .peer_display_name
                        .clone()
                        .unwrap_or_else(|| dm.peer_id.to_string());

                    seen_user_ids.insert(dm.peer_id);
                    friend_data.push(PrivateGroupFriend {
                        user_id: dm.peer_id.to_string().into(),
                        display_name: display_name.into(),
                        username: "".into(),
                        avatar_url: "".into(),
                        selected: false,
                        role: "member".into(),
                    });
                }
            }

            // Update UI - close settings modal and open invite modal
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Store the group ID for the invite
                    w.set_invite_private_group_id(group_id_str.into());
                    w.set_invite_private_group_friends(ModelRc::new(VecModel::from(friend_data)));
                    w.set_invite_private_group_selected_count(0);
                    w.set_active_modal("invite-private-group".into());
                }
            });
        });
    });
}

fn wire_invite_channel_members(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    // Open invite modal for regular channels
    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();
    let user_service = ui_ctx.user_service.clone();

    window.on_channel_settings_invite_channel_members(move || {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        let user_service = user_service.clone();

        tokio::spawn(async move {
            // Get the channel ID from channel settings
            let channel_id_str = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();

            if channel_id_str.is_empty() {
                return;
            }

            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid channel ID: {}", e);
                    return;
                }
            };

            // Get channel name
            let channel_name = channel_service
                .get_channel_local(&channel_id)
                .ok()
                .flatten()
                .map(|c| c.name.clone())
                .unwrap_or_default();

            // Get existing members to filter them out
            let existing_members: HashSet<UserId> = channel_service
                .get_channel_members(&channel_id, 100, 0)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.user_id)
                .collect();

            // Load available friends
            let mut user_data: Vec<InviteUser> = Vec::new();
            let mut seen_user_ids: HashSet<UserId> = HashSet::new();

            // Load friends
            let friends = user_service.list_friends(100, 0).await.unwrap_or_default();
            for friendship in &friends {
                if seen_user_ids.contains(&friendship.friend_id) {
                    continue;
                }

                let is_member = existing_members.contains(&friendship.friend_id);
                let profile = user_service.get_profile_local(&friendship.friend_id).ok().flatten();
                let user = user_service.get_user_local(&friendship.friend_id).ok().flatten();

                let display_name = profile
                    .as_ref()
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| friendship.friend_id.to_string());
                let username = user
                    .as_ref()
                    .map(|u| u.username.clone())
                    .unwrap_or_default();

                seen_user_ids.insert(friendship.friend_id);
                user_data.push(InviteUser {
                    id: friendship.friend_id.to_string().into(),
                    display_name: display_name.into(),
                    username: username.into(),
                    avatar_url: "".into(),
                    already_member: is_member,
                    selected: false,
                });
            }

            // Update UI - open invite modal
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_invite_member_channel_id(channel_id_str.into());
                    w.set_invite_member_channel_name(channel_name.into());
                    w.set_invite_member_users(ModelRc::new(VecModel::from(user_data)));
                    w.set_invite_member_selected_count(0);
                    w.set_active_modal("invite-member".into());
                }
            });
        });
    });

    // Toggle user selection
    let weak_window = window.as_weak();
    window.on_invite_member_toggle_user(move |idx| {
        let weak = weak_window.clone();
        let index = idx as usize;

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let users = w.get_invite_member_users();
                if let Some(mut user) = users.row_data(index) {
                    // Toggle selected state
                    user.selected = !user.selected;
                    users.set_row_data(index, user);

                    // Recalculate selected count
                    let count = (0..users.row_count())
                        .filter_map(|i| users.row_data(i))
                        .filter(|u| u.selected)
                        .count() as i32;
                    w.set_invite_member_selected_count(count);
                }
            }
        });
    });

    // Confirm invite
    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channel_service.clone();

    window.on_invite_member_confirm(move || {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();

        tokio::spawn(async move {
            // Get channel ID and selected users
            let (channel_id_str, selected_user_ids) = {
                let Some(w) = weak.upgrade() else { return };
                let channel_id = w.get_invite_member_channel_id().to_string();
                let users = w.get_invite_member_users();
                let selected_vec: Vec<String> = (0..users.row_count())
                    .filter_map(|i| users.row_data(i))
                    .filter(|u| u.selected && !u.already_member)
                    .map(|u| u.id.to_string())
                    .collect();
                (channel_id, selected_vec)
            };

            if channel_id_str.is_empty() || selected_user_ids.is_empty() {
                return;
            }

            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid channel ID: {}", e);
                    return;
                }
            };

            // Set loading state
            {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_invite_member_loading(true);
                    }
                });
            }

            // Invite each user
            let mut success_count = 0;
            for user_id_str in &selected_user_ids {
                let user_id: UserId = match user_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => continue,
                };

                match channel_service.invite_user(&channel_id, &user_id, None).await {
                    Ok(_) => {
                        success_count += 1;
                        tracing::info!(
                            channel_id = %channel_id,
                            user_id = %user_id,
                            "Invited user to channel"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            channel_id = %channel_id,
                            user_id = %user_id,
                            error = %e,
                            "Failed to invite user to channel"
                        );
                    }
                }
            }

            // Close modal
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_invite_member_loading(false);
                    if success_count > 0 {
                        w.set_active_modal("none".into());
                    }
                }
            });
        });
    });
}
