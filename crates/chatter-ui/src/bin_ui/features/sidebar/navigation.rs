//! Navigation callbacks: saved messages, settings, profile, create/join channel, toggle sidebar, org switcher.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::ChannelId;

use crate::MainWindow;

use super::channel::load_channel_into_ui;
use super::stop_audio_playback;

pub(crate) fn wire_saved_messages(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let auth_state = ui_ctx.auth_state().clone();
    let chat_state = ui_ctx.chat_state().clone();
    let channel_service = ui_ctx.channels().clone();
    let message_service = ui_ctx.messages().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();

    window.on_saved_messages_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let chat_state = chat_state.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        tokio::spawn(async move {
            // Get current user ID - saved messages channel_id = user_id
            let user_id = match auth_state.get().current_user_id {
                Some(id) => id,
                None => {
                    tracing::warn!("Cannot open saved messages: not logged in");
                    return;
                }
            };

            // The saved messages channel ID is the same as the user ID
            let channel_id: ChannelId = user_id.into();

            // Set loading state
            let weak_loading = weak.clone();
            let channel_id_str: SharedString = channel_id.to_string().into();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_channel_id(channel_id_str);
                    w.set_selected_dm_id("".into());
                    // Clear composer reply/edit state to prevent stale context
                    w.set_composer_is_replying(false);
                    w.set_composer_reply_to_sender("".into());
                    w.set_composer_reply_to_sender_name("".into());
                    w.set_composer_reply_to_timestamp("".into());
                    w.set_composer_reply_preview("".into());
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                }
            });

            // Update chat state with active channel
            chat_state.set_active_channel(channel_id);
            chat_state.set_active_conversation(chatter_core::state::ActiveConversation::Channel(channel_id));

            // Ensure the saved messages channel exists locally before loading.
            // The server may not include it in list_channels, so fetch it on demand.
            if let Ok(None) | Err(_) = channel_service.get_channel_local(&channel_id) {
                tracing::info!("SavedMessages channel not in local repo, fetching from server");
                if let Err(e) = channel_service.get_channel(&channel_id).await {
                    tracing::error!("Failed to fetch saved messages channel from server: {}", e);
                    let weak_err = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_err.upgrade() {
                            w.set_loading_messages(false);
                        }
                    });
                    return;
                }
            }

            // Load the saved messages channel and its messages
            let current_user_id = Some(user_id.to_string());
            load_channel_into_ui(
                weak,
                &channel_id,
                channel_service,
                message_service,
                user_service,
                attachment_service,
                current_user_id,
            ).await;
        });
    });
}

pub(crate) fn wire_settings_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let auth_state = ui_ctx.auth_state().clone();
    let privacy_state = ui_ctx.privacy_state().clone();
    let signal_service = ui_ctx.signal().clone();

    let weak_window = window.as_weak();

    window.on_settings_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let privacy_state = privacy_state.clone();
        let signal_service = signal_service.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let state = auth_state.get();
                w.set_settings_display_name(state.display_name.unwrap_or_default().into());
                w.set_settings_username(
                    state
                        .current_user_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .into(),
                );

                // Load privacy settings from state
                let privacy = privacy_state.get();
                w.set_settings_read_receipts(privacy.global_read_receipts);
                w.set_settings_typing_indicators(privacy.global_typing_indicators);

                w.set_active_modal("settings".into());
            }
        });

        // Fetch identity fingerprint asynchronously and set it on the UI
        let weak_fp = weak_window.clone();
        tokio::spawn(async move {
            match signal_service.get_identity_fingerprint().await {
                Ok(fingerprint) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_fp.upgrade() {
                            w.set_settings_identity_fingerprint(fingerprint.into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to load identity fingerprint for settings: {}", e);
                }
            }
        });
    });
}

pub(crate) fn wire_profile_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let auth_state = ui_ctx.auth_state().clone();
    let user_state = ui_ctx.user_state().clone();
    let ui_ctx_profile = ui_ctx.clone();

    let weak_window = window.as_weak();

    window.on_profile_clicked(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let user_state = user_state.clone();
        let ui_ctx = ui_ctx_profile.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if let Some(user_id) = auth_state.get().current_user_id {
                    crate::bin_ui::features::modals::open_user_profile(
                        &w, &user_id, &auth_state, &user_state, Some(&ui_ctx),
                    );
                }
            }
        });
    });
}

pub(crate) fn wire_create_join_channel(window: &MainWindow) {
    // Open create channel modal (just logs for now, modal is opened via active-modal in Slint)
    window.on_open_create_channel(|| {
        tracing::debug!("Opening create channel modal");
    });

    // Open join channel modal (just logs for now, modal is opened via active-modal in Slint)
    window.on_open_join_channel(|| {
        tracing::debug!("Opening join channel modal");
    });
}

pub(crate) fn wire_toggle_sidebar(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_state = ui_ctx.ui_state().clone();
    let weak_window = window.as_weak();

    window.on_toggle_sidebar(move || {
        let ui_state = ui_state.clone();
        let weak = weak_window.clone();

        // Get the current collapsed state and persist it via UI state
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let collapsed = w.get_sidebar_collapsed();
                // Persist via UI state (which handles storage internally)
                ui_state.set_sidebar_collapsed(collapsed);
            }
        });
    });
}

pub(crate) fn load_sidebar_collapsed_state(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Load saved collapsed state from UI state on startup
    let collapsed = ui_ctx.ui_state().get().sidebar_collapsed;
    window.set_sidebar_collapsed(collapsed);
}

/// Wire message requests click handler.
///
/// Opens the friends modal with the "requests" tab and "private" sub-tab selected.
/// Refreshes both the badge count and the private requests list in the modal.
pub(crate) fn wire_message_requests_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let anon_dm_service = ui_ctx.anon_dms().clone();
    let weak_window = window.as_weak();

    window.on_message_requests_clicked(move || {
        let anon_dm_service = anon_dm_service.clone();
        let weak = weak_window.clone();

        // Refresh the pending requests list and count when clicked
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Get fresh requests from service
                match anon_dm_service.list_message_requests() {
                    Ok(requests) => {
                        let count = requests.len() as i32;

                        // Build UI data for private requests
                        let private_requests: Vec<crate::FriendRequestData> = requests
                            .iter()
                            .map(|dm| {
                                // Use stored display name, not user_service (strangers)
                                let peer_name = dm.peer_display_name.clone()
                                    .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));

                                crate::FriendRequestData {
                                    user_id: dm.id.to_string().into(),
                                    display_name: peer_name.into(),
                                    username: "Private".into(),
                                    avatar_url: "".into(),
                                    avatar_source: slint::Image::default(),
                                    has_avatar: false,
                                    sent_at: dm.created_at
                                        .with_timezone(&chrono::Local)
                                        .format("%b %d, %H:%M")
                                        .to_string()
                                        .into(),
                                }
                            })
                            .collect();

                        tracing::info!(
                            count = count,
                            "Message requests clicked - refreshed {} requests",
                            count
                        );

                        // Update both the badge count and the requests list
                        w.set_pending_message_requests(count);
                        w.set_friends_private_requests(slint::ModelRc::new(slint::VecModel::from(private_requests)));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to refresh message requests: {}", e);
                        w.set_pending_message_requests(0);
                    }
                }
            }
        });
    });
}

/// Load pending message requests count and list into the sidebar.
pub(crate) fn load_pending_message_requests_count(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    match ui_ctx.anon_dms().list_message_requests() {
        Ok(requests) => {
            let count = requests.len() as i32;

            // Build UI data for private requests
            let private_requests: Vec<crate::FriendRequestData> = requests
                .iter()
                .map(|dm| {
                    // Use stored display name, not user_service (strangers)
                    let peer_name = dm.peer_display_name.clone()
                        .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));

                    crate::FriendRequestData {
                        user_id: dm.id.to_string().into(),
                        display_name: peer_name.into(),
                        username: "Private".into(),
                        avatar_url: "".into(),
                        avatar_source: slint::Image::default(),
                        has_avatar: false,
                        sent_at: dm.created_at
                            .with_timezone(&chrono::Local)
                            .format("%b %d, %H:%M")
                            .to_string()
                            .into(),
                    }
                })
                .collect();

            tracing::info!(
                count = count,
                "Loaded {} pending message requests",
                count
            );

            window.set_pending_message_requests(count);
            window.set_friends_private_requests(slint::ModelRc::new(slint::VecModel::from(private_requests)));
        }
        Err(e) => {
            tracing::warn!("Failed to load message requests: {}", e);
            window.set_pending_message_requests(0);
        }
    }
}

/// Wire group invites click handler.
///
/// Opens the friends modal with the "requests" tab and "groups" sub-tab selected.
/// Refreshes both the badge count and the group invites list in the modal.
pub(crate) fn wire_group_invites_clicked(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window.on_group_invites_clicked(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Refresh count from the pending invites store
                let count = crate::bin_ui::features::modals::pending_group_invite_count();
                tracing::info!(count = count, "Group invites clicked - {} pending", count);
                w.set_pending_group_invites(count);
            }
        });
    });
}

/// Load pending group invites count into the sidebar.
pub(crate) fn load_pending_group_invites_count(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let count = crate::bin_ui::features::modals::pending_group_invite_count();
    tracing::info!(count = count, "Loaded {} pending group invites", count);
    window.set_pending_group_invites(count);
}

/// Load organization state into the sidebar.
///
/// Reads OrgState and populates the org switcher with the current org and list.
pub(crate) fn load_org_state(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let org_data = ui_ctx.org_state().get();

    // Set current org name
    if let Some(ref name) = org_data.current_org_name {
        window.set_current_org_name(SharedString::from(name.as_str()));
    }

    // Set org loading state
    window.set_org_loading(org_data.is_loading);

    // Build org list for the switcher
    let org_items: Vec<crate::OrgListItemData> = org_data
        .organizations
        .iter()
        .map(|org| crate::OrgListItemData {
            id: SharedString::from(org.id.as_str()),
            name: SharedString::from(org.name.as_str()),
            slug: SharedString::from(org.slug.as_str()),
            initial: SharedString::from(
                org.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
            ),
            member_count: org.member_count,
            is_current: org_data.current_org_id.as_deref() == Some(&org.id),
        })
        .collect();

    window.set_organizations(slint::ModelRc::new(slint::VecModel::from(org_items)));
}

/// Wire the org-selected callback.
///
/// When the user picks a different org from the switcher, update the org state.
pub(crate) fn wire_org_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let org_state = ui_ctx.org_state().clone();
    let weak_window = window.as_weak();

    window.on_org_selected(move |org_id| {
        let org_id_str = org_id.to_string();
        let org_state = org_state.clone();
        let weak = weak_window.clone();

        // Find org name from current data
        let org_data = org_state.get();
        let org_name = org_data
            .organizations
            .iter()
            .find(|o| o.id == org_id_str)
            .map(|o| o.name.clone())
            .unwrap_or_default();

        tracing::info!(org_id = %org_id_str, org_name = %org_name, "Organization switched");
        org_state.set_current_org(&org_id_str, &org_name);

        // Update UI immediately
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_current_org_name(SharedString::from(org_name.as_str()));

                // Update is_current flags in the list
                let model = w.get_organizations();
                let updated: Vec<crate::OrgListItemData> = (0..slint::Model::row_count(&model))
                    .filter_map(|i| slint::Model::row_data(&model, i))
                    .map(|mut item| {
                        item.is_current = item.id.as_str() == org_id_str;
                        item
                    })
                    .collect();
                w.set_organizations(slint::ModelRc::new(slint::VecModel::from(updated)));
            }
        });
    });
}

/// Wire the manage-org-clicked callback to open org management modal.
pub(crate) fn wire_manage_org_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.on_manage_org_clicked(move || {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();

        // Set current org info on the window
        let org_data = ui_ctx.org_state().get();
        if let (Some(org_id), Some(org_name)) = (&org_data.current_org_id, &org_data.current_org_name) {
            let org_id = org_id.clone();
            let org_name = org_name.clone();
            let viewer_role = org_data.organizations.iter()
                .find(|o| o.id == org_id)
                .map(|o| match o.viewer_role {
                    4 => "owner",
                    3 => "admin",
                    2 => "member",
                    1 => "guest",
                    _ => "member",
                })
                .unwrap_or("member")
                .to_string();
            let slug = org_data.organizations.iter()
                .find(|o| o.id == org_id)
                .map(|o| o.slug.clone())
                .unwrap_or_default();

            // Clone weak before moving into closures
            let weak2 = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_current_org_id(org_id.into());
                    w.set_current_org_name(org_name.into());
                    w.set_current_org_slug(slug.into());
                    w.set_current_org_viewer_role(viewer_role.into());
                    w.set_active_modal("org-management".into());
                }
            });

            // Trigger initial member load
            let ui_ctx2 = ui_ctx.clone();
            tokio::spawn(async move {
                load_org_members(&weak2, &ui_ctx2).await;
            });
        }
    });
}

/// Wire org management data-loading callbacks.
pub(crate) fn wire_org_management_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Load members
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_org_load_members(move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move { load_org_members(&weak, &ui_ctx).await; });
        });
    }

    // Load settings
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_org_load_settings(move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move { load_org_settings(&weak, &ui_ctx).await; });
        });
    }

    // Load dashboard
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_org_load_dashboard(move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move { load_org_dashboard(&weak, &ui_ctx).await; });
        });
    }

    // Invite member
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_org_invite_member(move |email: slint::SharedString, role: slint::SharedString| {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            let email = email.to_string();
            let _role = role.to_string();
            tokio::spawn(async move {
                let org_id = ui_ctx.org_state().current_org_id().unwrap_or_default();
                if org_id.is_empty() { return; }

                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.set_org_invite_loading(true);
                        w.set_org_invite_error("".into());
                        w.set_org_invite_success("".into());
                    }
                });

                if let Some(mut org_client) = ui_ctx.try_org_client() {
                    // Role 2 = MEMBER
                    match org_client.invite_member(&org_id, &email, 2).await {
                        Ok(_) => {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_org_invite_loading(false);
                                    w.set_org_invite_success(format!("Invited {}", email).into());
                                    w.set_org_invite_email("".into());
                                }
                            });
                        }
                        Err(e) => {
                            let err = e.to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_org_invite_loading(false);
                                    w.set_org_invite_error(err.into());
                                }
                            });
                        }
                    }
                }
            });
        });
    }
}

/// Load org members from server and update UI.
async fn load_org_members(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    use crate::OrgMemberData;

    let org_id = ui_ctx.org_state().current_org_id().unwrap_or_default();
    if org_id.is_empty() { return; }

    let weak_loading = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_loading.upgrade() {
            w.set_org_members_loading(true);
        }
    });

    if let Some(mut org_client) = ui_ctx.try_org_client() {
        match org_client.list_members(&org_id).await {
            Ok(response) => {
                let members: Vec<OrgMemberData> = response.members.iter().map(|m| {
                    let role_str = match m.role {
                        4 => "owner",
                        3 => "admin",
                        2 => "member",
                        1 => "guest",
                        _ => "member",
                    };
                    OrgMemberData {
                        user_id: m.user_id.clone().into(),
                        display_name: m.display_name.clone().into(),
                        username: m.username.clone().into(),
                        avatar_url: m.avatar_url.clone().into(),
                        role: role_str.into(),
                        joined_at: String::new().into(),
                    }
                }).collect();

                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let model = std::rc::Rc::new(slint::VecModel::from(members));
                        w.set_org_members(model.into());
                        w.set_org_members_loading(false);
                    }
                });
            }
            Err(e) => {
                tracing::warn!("Failed to load org members: {}", e);
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_org_members_loading(false);
                    }
                });
            }
        }
    }
}

/// Load org settings from server and update UI.
async fn load_org_settings(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let org_id = ui_ctx.org_state().current_org_id().unwrap_or_default();
    if org_id.is_empty() { return; }

    if let Some(mut org_client) = ui_ctx.try_org_client() {
        match org_client.get_org_settings(&org_id).await {
            Ok(response) => {
                if let Some(settings) = response.settings {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_org_max_members(settings.max_members.unwrap_or(0));
                            w.set_org_retention_days(settings.retention_days.unwrap_or(0));
                            w.set_org_require_2fa(settings.require_2fa);
                            w.set_org_allow_guest_access(settings.allow_guest_access);
                        }
                    });
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load org settings: {}", e);
            }
        }
    }
}

/// Load org dashboard from server and update UI.
async fn load_org_dashboard(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    use crate::OrgDashboardData;

    let org_id = ui_ctx.org_state().current_org_id().unwrap_or_default();
    if org_id.is_empty() { return; }

    if let Some(mut org_client) = ui_ctx.try_org_client() {
        match org_client.get_org_dashboard(&org_id).await {
            Ok(dashboard) => {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_org_dashboard(OrgDashboardData {
                            total_members: dashboard.total_members,
                            active_members_30d: dashboard.active_members_30d,
                            total_channels: dashboard.total_channels,
                            total_messages_30d: dashboard.total_messages_30d as i32,
                            storage_used_mb: (dashboard.storage_used_bytes / 1_048_576) as i32,
                            storage_limit_mb: (dashboard.storage_limit_bytes / 1_048_576) as i32,
                        });
                    }
                });
            }
            Err(e) => {
                tracing::warn!("Failed to load org dashboard: {}", e);
            }
        }
    }
}

/// Subscribe to OrgState changes and update the sidebar reactively.
pub(crate) fn subscribe_org_state(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut rx = ui_ctx.org_state().subscribe();
    let weak = window.as_weak();

    tokio::spawn(async move {
        loop {
            let data = match rx.recv().await {
                Ok(data) => data,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Org state receiver lagged, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Update org name
                    w.set_current_org_name(SharedString::from(
                        data.current_org_name.as_deref().unwrap_or(""),
                    ));
                    w.set_org_loading(data.is_loading);

                    // Update org list
                    let org_items: Vec<crate::OrgListItemData> = data
                        .organizations
                        .iter()
                        .map(|org| crate::OrgListItemData {
                            id: SharedString::from(org.id.as_str()),
                            name: SharedString::from(org.name.as_str()),
                            slug: SharedString::from(org.slug.as_str()),
                            initial: SharedString::from(
                                org.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
                            ),
                            member_count: org.member_count,
                            is_current: data.current_org_id.as_deref() == Some(&org.id),
                        })
                        .collect();
                    w.set_organizations(slint::ModelRc::new(slint::VecModel::from(org_items)));
                }
            });
        }
    });
}
