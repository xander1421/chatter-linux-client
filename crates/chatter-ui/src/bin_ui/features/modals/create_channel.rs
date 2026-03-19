//! Create channel modal callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::{ChannelConfig, ChannelType, MessagePersistence};
use chatter_core::domain::ids::UserId;
use chatter_core::proto::chat_v1::IntegrationType;

use crate::bin_ui::converters::channel_to_slint;
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{AIModelOption, ChannelListItemData, CreateChannelConfig, MainWindow, PrivateGroupFriend};

pub fn wire_create_channel(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ctx = ui_ctx.clone();
    let channel_service = ctx.channels().clone();
    let message_service = ctx.messages().clone();
    let user_service = ctx.users().clone();
    let attachment_service = ctx.attachments().clone();
    let ai_service = ctx.ai().clone();
    let chat_state = ctx.chat_state().clone();
    let auth_state_global = ctx.auth_state().clone();

    window.on_create_channel(
        move |name: SharedString,
              description: SharedString,
              template: SharedString,
              config: CreateChannelConfig,
              ai_provider: SharedString,
              ai_model: SharedString| {
            let weak = weak_window.clone();
            let ui_ctx = ctx.clone();
            let channel_service = channel_service.clone();
            let message_service = message_service.clone();
            let user_service = user_service.clone();
            let attachment_service = attachment_service.clone();
            let ai_service = ai_service.clone();
            let chat_state = chat_state.clone();
            let name = name.to_string();
            let description = description.to_string();
            let template = template.to_string();
            let ai_provider = ai_provider.to_string();
            let ai_model = ai_model.to_string();
            let auth_state = auth_state_global.clone();

            let is_private = config.is_private;
            let is_ephemeral = config.is_ephemeral;
            let ttl_hours = config.ttl_hours;
            let max_members = config.max_members;
            let password = config.password.to_string();

            tokio::spawn(async move {
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.set_create_channel_loading(true);
                        w.set_create_channel_error("".into());
                    }
                });

                let channel_type = if is_private { ChannelType::Private } else { ChannelType::Public };
                let description_opt = if description.is_empty() { None } else { Some(description.as_str()) };
                let password_opt = if password.is_empty() { None } else { Some(password.as_str()) };

                let domain_config = ChannelConfig {
                    persistence: if is_ephemeral { MessagePersistence::Ephemeral } else { MessagePersistence::Permanent },
                    max_members: if max_members > 0 { Some(max_members as u32) } else { None },
                    default_ttl_seconds: if is_ephemeral { Some((ttl_hours as u32) * 3600) } else { None },
                    require_encryption: true,
                    ..Default::default()
                };

                match ui_ctx.create_channel(&name, channel_type, Some(domain_config), description_opt, password_opt, vec![]).await {
                    Ok(channel) => {
                        let channel_id = channel.id;
                        let channel_id_str: SharedString = channel_id.to_string().into();

                        chat_state.set_active_channel(channel_id);

                        if template == "ai-assistant" && !ai_provider.is_empty() && !ai_model.is_empty() {
                            let provider_type = match ai_provider.as_str() {
                                "claude" => IntegrationType::Claude,
                                "openai" => IntegrationType::Openai,
                                _ => IntegrationType::Custom,
                            };
                            if let Ok(integration) = ai_service.create(&format!("{} Assistant", name), provider_type, Some(chatter_core::proto::chat_v1::IntegrationConfig { model: ai_model.clone(), ..Default::default() })).await {
                                let _ = ai_service.attach_to_channel(&integration.id, &channel_id.to_string()).await;
                            }
                        }

                        let slint_channel = channel_to_slint(&channel);
                        let weak_modal = weak.clone();
                        let channel_id_for_ui = channel_id_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_modal.upgrade() {
                                w.set_create_channel_loading(false);
                                w.set_create_channel_name("".into());
                                w.set_create_channel_description("".into());
                                w.set_active_modal("".into());
                                let current_channels = w.get_channels();
                                let mut channels: Vec<ChannelListItemData> = (0..current_channels.row_count()).filter_map(|i| current_channels.row_data(i)).collect();
                                channels.push(slint_channel);
                                w.set_channels(ModelRc::new(VecModel::from(channels)));
                                w.set_selected_channel_id(channel_id_for_ui);
                            }
                        });

                        let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string());
                        load_channel_into_ui(weak, &channel_id, channel_service.clone(), message_service.clone(), user_service.clone(), attachment_service.clone(), current_user_id).await;
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
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

    let weak_window = window.as_weak();
    let ai_service = ui_ctx.ai().clone();
    window.on_create_channel_provider_selected(move |provider: SharedString| {
        let weak = weak_window.clone();
        let ai_service = ai_service.clone();
        let provider_str = provider.to_string();
        tokio::spawn(async move {
            let provider_type = match provider_str.as_str() {
                "claude" => IntegrationType::Claude,
                "openai" => IntegrationType::Openai,
                _ => return,
            };
            if let Ok(available_models) = ai_service.list_models(provider_type).await {
                let models: Vec<AIModelOption> = available_models.into_iter().map(|m| AIModelOption { id: m.id.into(), name: m.display_name.into(), provider: provider_str.clone().into() }).collect();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() { w.set_create_channel_ai_models(ModelRc::new(VecModel::from(models))); }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    window.on_toggle_friend_selection(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_create_channel_available_friends();
                let mut friends_vec: Vec<PrivateGroupFriend> = (0..friends.row_count()).filter_map(|i| friends.row_data(i)).collect();
                if let Some(friend) = friends_vec.get_mut(idx as usize) {
                    friend.selected = !friend.selected;
                    // Default to member role when selecting
                    if friend.selected && friend.role.is_empty() {
                        friend.role = "member".into();
                    }
                }
                let selected_count = friends_vec.iter().filter(|f| f.selected).count() as i32;
                w.set_create_channel_selected_friend_count(selected_count);
                w.set_create_channel_available_friends(ModelRc::new(VecModel::from(friends_vec)));
            }
        });
    });

    // Toggle friend role between member and admin
    let weak_window = window.as_weak();
    window.on_toggle_friend_role(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_create_channel_available_friends();
                let mut friends_vec: Vec<PrivateGroupFriend> = (0..friends.row_count()).filter_map(|i| friends.row_data(i)).collect();
                if let Some(friend) = friends_vec.get_mut(idx as usize) {
                    if friend.selected {
                        // Toggle between member and admin
                        friend.role = if friend.role == "admin" {
                            "member".into()
                        } else {
                            "admin".into()
                        };
                    }
                }
                w.set_create_channel_available_friends(ModelRc::new(VecModel::from(friends_vec)));
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_group = ui_ctx.clone();
    window.on_create_private_group(move |name: SharedString, friends: ModelRc<PrivateGroupFriend>| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_group.clone();
        let name = name.to_string();

        // Extract selected friends with their roles
        let selected_friends: Vec<(UserId, chatter_core::domain::anon_group::AnonGroupRole)> = (0..friends.row_count())
            .filter_map(|i| friends.row_data(i))
            .filter(|f| f.selected)
            .filter_map(|f| {
                let user_id = f.user_id.to_string().parse::<UserId>().ok()?;
                let role = if f.role == "admin" {
                    chatter_core::domain::anon_group::AnonGroupRole::Admin
                } else {
                    chatter_core::domain::anon_group::AnonGroupRole::Member
                };
                Some((user_id, role))
            })
            .collect();

        let member_count = selected_friends.len() as i32 + 1; // +1 for self

        if selected_friends.is_empty() { return; }

        let weak_loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_loading.upgrade() {
                w.set_create_channel_loading(true);
                w.set_create_channel_error("".into());
            }
        });

        tokio::spawn(async move {
            match ui_ctx.create_anon_group(name.clone(), selected_friends).await {
                Ok(group) => {
                    let weak_success = weak.clone();
                    let group_id = group.id.to_string();
                    let group_name = group.name.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_success.upgrade() {
                            w.set_create_channel_loading(false);
                            w.set_active_modal("".into());
                            crate::bin_ui::features::sidebar::add_private_group_to_sidebar(&w, &group_id, &group_name, member_count);
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_create_channel_loading(false);
                            w.set_create_channel_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    let weak_window = window.as_weak();
    let user_service = ui_ctx.users().clone();
    let anon_dm_service = ui_ctx.anon_dms().clone();
    window.on_create_channel_template_selected(move |template: SharedString| {
        if template.as_str() != "private-group" { return; }
        let weak = weak_window.clone();
        let user_service = user_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        tokio::spawn(async move {
            let mut friend_data = Vec::new();
            let mut seen_user_ids = std::collections::HashSet::new();

            // Load friends
            let friends = user_service.list_friends(100, 0).await.unwrap_or_default();
            for friendship in &friends {
                let profile = user_service.get_profile_local(&friendship.friend_id).ok().flatten();
                let display_name = profile.as_ref().map(|p| p.display_name.clone()).unwrap_or_else(|| friendship.friend_id.to_string());
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
                    // Skip if already in friends list
                    if seen_user_ids.contains(&dm.peer_id) {
                        continue;
                    }
                    // Only include ready conversations (can send via tunnel)
                    if !dm.is_ready() {
                        continue;
                    }
                    let display_name = dm.peer_display_name
                        .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
                    friend_data.push(PrivateGroupFriend {
                        user_id: dm.peer_id.to_string().into(),
                        display_name: display_name.into(),
                        username: "(anonymous)".into(), // Indicate this is an anon DM peer
                        avatar_url: "".into(),
                        selected: false,
                        role: "member".into(),
                    });
                }
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() { w.set_create_channel_available_friends(ModelRc::new(VecModel::from(friend_data))); }
            });
        });
    });
}

/// Wire the invite to private group modal callbacks.
pub fn wire_invite_to_private_group(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Toggle selection for invite modal
    let weak_window = window.as_weak();
    window.on_toggle_invite_friend_selection(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_invite_private_group_friends();
                let mut friends_vec: Vec<PrivateGroupFriend> = (0..friends.row_count())
                    .filter_map(|i| friends.row_data(i))
                    .collect();
                if let Some(friend) = friends_vec.get_mut(idx as usize) {
                    friend.selected = !friend.selected;
                    // Default to member role when selecting
                    if friend.selected && friend.role.is_empty() {
                        friend.role = "member".into();
                    }
                }
                let selected_count = friends_vec.iter().filter(|f| f.selected).count() as i32;
                w.set_invite_private_group_selected_count(selected_count);
                w.set_invite_private_group_friends(ModelRc::new(VecModel::from(friends_vec)));
            }
        });
    });

    // Toggle role for invite modal (member/admin)
    let weak_window = window.as_weak();
    window.on_toggle_invite_friend_role(move |idx: i32| {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let friends = w.get_invite_private_group_friends();
                let mut friends_vec: Vec<PrivateGroupFriend> = (0..friends.row_count())
                    .filter_map(|i| friends.row_data(i))
                    .collect();
                if let Some(friend) = friends_vec.get_mut(idx as usize) {
                    if friend.selected {
                        friend.role = if friend.role == "admin" {
                            "member".into()
                        } else {
                            "admin".into()
                        };
                    }
                }
                w.set_invite_private_group_friends(ModelRc::new(VecModel::from(friends_vec)));
            }
        });
    });

    // Submit invite
    let weak_window = window.as_weak();
    let ui_ctx_invite = ui_ctx.clone();
    window.on_invite_to_private_group(move |friends: ModelRc<PrivateGroupFriend>| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_invite.clone();

        let group_id_str = weak
            .upgrade()
            .map(|w| w.get_invite_private_group_id().to_string())
            .unwrap_or_default();

        // Extract selected friends with their roles
        let selected_friends: Vec<(UserId, chatter_core::domain::anon_group::AnonGroupRole)> = (0..friends.row_count())
            .filter_map(|i| friends.row_data(i))
            .filter(|f| f.selected)
            .filter_map(|f| {
                let user_id = f.user_id.to_string().parse::<UserId>().ok()?;
                let role = if f.role == "admin" {
                    chatter_core::domain::anon_group::AnonGroupRole::Admin
                } else {
                    chatter_core::domain::anon_group::AnonGroupRole::Member
                };
                Some((user_id, role))
            })
            .collect();

        if selected_friends.is_empty() || group_id_str.is_empty() {
            return;
        }

        // Set loading state
        let weak_loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_loading.upgrade() {
                w.set_invite_private_group_loading(true);
            }
        });

        tokio::spawn(async move {
            let group_id: chatter_core::domain::anon_group::AnonGroupId = match group_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid group ID for invite: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_invite_private_group_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.invite_to_anon_group(&group_id, selected_friends).await {
                Ok(_) => {
                    tracing::info!("Successfully invited members to group {}", group_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_invite_private_group_loading(false);
                            w.set_active_modal("none".into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to invite members: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_invite_private_group_loading(false);
                        }
                    });
                }
            }
        });
    });
}
