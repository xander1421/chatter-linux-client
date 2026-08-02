//! Channel settings modal callbacks.

use std::collections::HashSet;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::{ChatterClient, InviteCandidate};
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::ChannelType;

use crate::{ChannelMemberData, InviteUser, MainWindow, PrivateGroupFriend};

/// Wire channel settings modal callbacks.
pub fn wire_channel_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_save_settings(window, ui_ctx);
    wire_pick_icon(window, ui_ctx);
    wire_notification_settings(window, ui_ctx);
    wire_member_management(window, ui_ctx);
    wire_channel_actions(window, ui_ctx);
    wire_invite_members(window, ui_ctx);
    wire_invite_channel_members(window, ui_ctx);
    wire_invite_member_search(window, ui_ctx);
}

/// Open the channel settings modal with channel data.
pub fn open_channel_settings(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
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
        let channel = match ui_ctx.get_channel_local(&channel_id) {
            Some(c) => c,
            None => {
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

        let current_user_id = ui_ctx.session_current_user_id();
        let is_dm = channel.is_dm();
        let (is_owner, can_edit, can_manage_members, can_archive, can_delete) = match current_user_id.as_ref() {
            Some(uid) => (
                channel.is_owner(uid),
                channel.can_user_edit(uid),
                channel.can_user_manage_members(uid),
                channel.can_user_archive(uid),
                channel.can_user_delete(uid),
            ),
            None => (false, false, false, false, false),
        };

        let created_at = channel.created_at.with_timezone(&chrono::Local).format("%b %d, %Y").to_string();

        let members: Vec<ChannelMemberData> = if !is_dm {
            match ui_ctx.get_channel_members(&channel_id, 100, 0).await {
                Ok(memberships) => {
                    let mut data = Vec::new();
                    for m in memberships {
                        let display = ui_ctx.display_name(&m.user_id);
                        let uname = ui_ctx.get_user_local(&m.user_id).map(|u| u.username).unwrap_or_else(|| "unknown".to_string());
                        let role = m.role.as_str();
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
        // Read the stored per-channel AI switch so the toggle opens showing
        // what is actually persisted, not a default.
        let ai_enabled = ui_ctx.ai_enabled_for(&channel_id_str);

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                w.set_channel_settings_id(channel_id_str.into());
                w.set_channel_settings_name(channel_name.clone().into());
                w.set_channel_settings_description(channel_description.clone().into());
                // Reset the editable drafts to the stored values so reopening a
                // different channel doesn't carry the previous edits.
                w.set_channel_settings_name_draft(channel_name.into());
                w.set_channel_settings_description_draft(channel_description.into());
                // Clear any icon picked in a previous open.
                w.set_channel_settings_icon_path("".into());
                w.set_channel_settings_has_icon_preview(false);
                w.set_channel_settings_ai_enabled(ai_enabled);
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

fn wire_save_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_channel_settings_save(move |name, description| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let name_str = name.to_string();
        let desc_str = description.to_string();

        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            // Read the picked icon file (if any) so Save uploads it alongside
            // the name/description. Empty path = icon unchanged.
            let icon_path = weak
                .upgrade()
                .map(|w| w.get_channel_settings_icon_path().to_string())
                .unwrap_or_default();
            let icon_bytes: Option<Vec<u8>> = if icon_path.is_empty() {
                None
            } else {
                match std::fs::read(&icon_path) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read picked channel icon");
                        None
                    }
                }
            };

            let desc_opt = if desc_str.is_empty() { None } else { Some(desc_str.as_str()) };
            match ui_ctx.update_channel(&channel_uuid, Some(&name_str), desc_opt, icon_bytes.as_deref()).await {
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

// Native file picker for the channel icon. Mirrors create_channel's picker:
// stash the chosen path on `channel-settings-icon-path` (Save reads it to
// upload) and decode a preview for the modal. File dialogs can't run headless,
// so this wire is proven statically (ui_wiring) + the upload round-trip by V30.
fn wire_pick_icon(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    window.on_channel_settings_pick_icon(move || {
        #[cfg(not(target_os = "android"))]
        {
            let weak = weak_window.clone();
            tokio::spawn(async move {
                let config = chatter_core::media::FilePickerConfig::new()
                    .with_title("Select Channel Icon")
                    .with_filter(chatter_core::media::FileFilter::images());
                let Some(path) = chatter_core::media::FilePicker::pick_file(config).await else {
                    return;
                };
                let data = match std::fs::read(&path) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to read picked channel icon");
                        return;
                    }
                };
                let path_string = path.to_string_lossy().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_icon_path(path_string.into());
                        if let Ok(img) = crate::bin_ui::slint_conversions::memory_to_slint_image(&data) {
                            w.set_channel_settings_icon_preview(img);
                            w.set_channel_settings_has_icon_preview(true);
                        }
                    }
                });
            });
        }
        #[cfg(target_os = "android")]
        tracing::warn!("File picker not supported on Android");
    });
}

fn wire_notification_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // NOTE: `change_notification` (per-channel notification PREFERENCE — all /
    // mentions / none) is intentionally still LOCAL-ONLY: chatter-core has no
    // notification-preference persistence method and the proto channel Config
    // carries no such field, so there is nothing to persist it to. Wiring it
    // needs a server-side change (owner-scoped, tracked in VALIDATED-UI F-UI-1).
    let weak_window = window.as_weak();
    window.on_channel_settings_change_notification(move |preference| {
        let weak = weak_window.clone();
        let pref_str = preference.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() { w.set_channel_settings_notification_pref(pref_str.into()); }
        });
    });

    // Per-channel AI switch. Off by default; read at ask time by
    // ChatterClient::jarvis_ask, so flipping it here actually changes whether
    // the AI will answer in this channel.
    let weak_ai = window.as_weak();
    let ui_ctx_ai = ui_ctx.clone();
    window.on_channel_settings_toggle_ai(move |enabled| {
        let Some(w) = weak_ai.upgrade() else { return };
        let channel_id = w.get_channel_settings_id().to_string();
        if channel_id.is_empty() {
            return;
        }
        match ui_ctx_ai.set_ai_enabled_for(&channel_id, enabled) {
            Ok(()) => w.set_channel_settings_ai_enabled(enabled),
            Err(e) => {
                tracing::error!("Failed to save per-channel AI setting: {}", e);
                // Snap the toggle back so it never shows a state we did not
                // manage to persist.
                w.set_channel_settings_ai_enabled(ui_ctx_ai.ai_enabled_for(&channel_id));
            }
        }
    });

    // Mute DOES persist: `mute_conversation` flips the local membership's
    // `is_muted` flag (channel-scoped) — the exact bug F-UI-1 documented (a
    // toggle that only flipped a Slint bool). Build the ActiveConversation from
    // the open channel-settings id, mirroring `change_member_role`.
    let weak_window = window.as_weak();
    let ui_ctx_mute = ui_ctx.clone();
    window.on_channel_settings_toggle_mute(move |muted| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_mute.clone();
        // Reflect the toggle immediately, then persist off the UI thread.
        let weak_ui = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_ui.upgrade() { w.set_channel_settings_muted(muted); }
        });
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            if let Err(e) = ui_ctx.mute_conversation(
                chatter_core::state::ActiveConversation::Channel(channel_uuid),
                muted,
            ) {
                tracing::error!(error = %e, "Failed to persist channel mute");
            }
        });
    });
}

fn wire_member_management(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    let ui_ctx_role = ui_ctx.clone();

    window.on_channel_settings_change_member_role(move |user_id, role| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_role.clone();
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
            let _ = ui_ctx.update_channel_member_role(&channel_uuid, &user_uuid, new_role).await;
        });
    });

    let weak_window = window.as_weak();
    window.on_channel_settings_remove_member(move |user_id| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
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

            if let Err(e) = ui_ctx.kick_user(&channel_uuid, &user_uuid, None).await {
                tracing::error!(error = %e, "Failed to kick user");
            }
        });
    });
}

fn wire_channel_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_archive = ui_ctx.clone();

    window.on_channel_settings_archive(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_archive.clone();
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let currently_archived = weak.upgrade().map(|w| w.get_channel_settings_archived()).unwrap_or(false);
            let result = ui_ctx.archive_channel(&channel_uuid, !currently_archived).await;
            if result.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() { w.set_active_modal("none".into()); }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_leave = ui_ctx.clone();
    window.on_channel_settings_leave(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_leave.clone();
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            if ui_ctx.leave_channel(&channel_uuid).await.is_ok() {
                let ui_ctx_refresh = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_active_modal("none".into());
                        ui_ctx_refresh.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::None);
                        crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx_refresh);
                    }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_delete = ui_ctx.clone();
    window.on_channel_settings_delete(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_delete.clone();
        tokio::spawn(async move {
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            if ui_ctx.delete_channel(&channel_uuid).await.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_active_modal("none".into());
                        ui_ctx.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::None);
                        crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
                    }
                });
            }
        });
    });
}

fn wire_invite_members(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_channel_settings_invite_members(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();

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
            let existing_members: HashSet<UserId> = ui_ctx
                .get_anon_group_members(&group_id)
                .into_iter()
                .map(|m| m.user_id)
                .collect();

            // Load available friends and anon DM peers
            let mut friend_data: Vec<PrivateGroupFriend> = Vec::new();
            let mut seen_user_ids: HashSet<UserId> = HashSet::new();

            // Load friends
            let friends = ui_ctx.list_friends(100, 0).await.unwrap_or_default();
            for friendship in &friends {
                // Skip existing members
                if existing_members.contains(&friendship.addressee_id) {
                    continue;
                }
                if seen_user_ids.contains(&friendship.addressee_id) {
                    continue;
                }

                let profile = ui_ctx.get_profile_local(&friendship.addressee_id);
                let display_name = profile
                    .as_ref()
                    .map(|p| p.display_name.clone())
                    .unwrap_or_else(|| friendship.addressee_id.to_string());

                seen_user_ids.insert(friendship.addressee_id);
                friend_data.push(PrivateGroupFriend {
                    user_id: friendship.addressee_id.to_string().into(),
                    display_name: display_name.into(),
                    username: "".into(),
                    selected: false,
                    role: "member".into(),
                });
            }

            // Load anon DM peers (active conversations only)
            {
                let conversations = ui_ctx.list_anon_dm_conversations();
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

/// Load invite-member data (friends + channel metadata) and open the invite modal.
///
/// Callable from any entry point that opens the channel invite modal (header
/// button, channel settings). Populates friends list so users see candidates
/// immediately; the search input can then hit the server for non-friend users.
pub fn open_invite_member_modal(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, channel_id: ChannelId) {
    tokio::spawn(async move {
        let channel_id_str = channel_id.to_string();

        let channel_name = ui_ctx
            .get_channel_local(&channel_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();

        // Carry Send `InviteCandidate`s across the thread boundary; build the
        // UI rows (which hold non-Send `image`s) inside the event loop.
        let candidates = ui_ctx
            .list_channel_invite_candidates(&channel_id)
            .await
            .unwrap_or_default();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let user_data: Vec<InviteUser> =
                    candidates.into_iter().map(candidate_to_invite_user).collect();
                w.set_invite_member_channel_id(channel_id_str.into());
                w.set_invite_member_channel_name(channel_name.into());
                w.set_invite_member_users(ModelRc::new(VecModel::from(user_data)));
                w.set_invite_member_selected_count(0);
                w.set_active_modal("invite-member".into());
            }
        });
    });
}

/// Thin presentation mapping: core candidate → UI row.
fn candidate_to_invite_user(c: InviteCandidate) -> InviteUser {
    let avatar = crate::bin_ui::converters::avatar_bytes_to_image(&c.avatar_bytes);
    InviteUser {
        id: c.user_id.to_string().into(),
        display_name: c.display_name.into(),
        username: c.username.into(),
        has_avatar: avatar.is_some(),
        avatar_source: avatar.unwrap_or_default(),
        already_member: c.already_member,
        selected: false,
    }
}

fn wire_invite_channel_members(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_open = ui_ctx.clone();

    window.on_channel_settings_invite_channel_members(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_open.clone();

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

        open_invite_member_modal(weak, ui_ctx, channel_id);
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
    let ui_ctx_invite = ui_ctx.clone();

    window.on_invite_member_confirm(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_invite.clone();

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

            tracing::info!(
                channel_id = %channel_id_str,
                selected = ?selected_user_ids,
                "invite confirm: resolving selection"
            );
            if channel_id_str.is_empty() || selected_user_ids.is_empty() {
                tracing::warn!("invite confirm: nothing to invite (empty channel or selection)");
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

                match ui_ctx.invite_user_to_channel(&channel_id, &user_id, None).await {
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

            // Report, then close.
            //
            // Before this, a run where every invite failed set loading back to
            // false and left the modal exactly as the user had filled it in —
            // same names ticked, same button — with the failures only in the log.
            // The obvious read is that the button did nothing, so people pressed
            // it again. A partial run was worse: it closed, which reads as "all
            // done", while some of the people picked were never invited.
            let attempted = selected_user_ids.len();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                w.set_invite_member_loading(false);

                if success_count == 0 {
                    // Stay open: the selection is still there and retrying is
                    // one tap, which is the right thing to offer here.
                    w.set_toast_message(
                        if attempted == 1 { "The invite was not sent.".to_string() }
                        else { format!("None of the {attempted} invites were sent.") }
                        .into(),
                    );
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                    w.global::<crate::Haptics>().invoke_error();
                    return;
                }

                if success_count < attempted {
                    let failed = attempted - success_count;
                    w.set_toast_message(
                        format!("{success_count} of {attempted} invites sent — {failed} failed.").into(),
                    );
                    w.set_toast_variant("warning".into());
                } else {
                    // The modal closing is the only other evidence, and it looks
                    // identical to dismissing it.
                    w.set_toast_message(
                        if attempted == 1 { "Invite sent.".to_string() }
                        else { format!("{attempted} invites sent.") }
                        .into(),
                    );
                    w.set_toast_variant("success".into());
                }
                w.set_toast_visible(true);
                w.set_active_modal("none".into());
            });
        });
    });
}

/// On every search keystroke, rebuild the candidate list: DM contacts from core
/// (substring-filtered when a query is present), then an exact-username server
/// lookup for anyone else matching. Dedupe by user id.
fn wire_invite_member_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_search = ui_ctx.clone();

    window.on_invite_member_search_changed(move |query: slint::SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_search.clone();
        let query = query.to_string();

        let channel_id_str = weak
            .upgrade()
            .map(|w| w.get_invite_member_channel_id().to_string())
            .unwrap_or_default();

        tokio::spawn(async move {
            if channel_id_str.is_empty() {
                return;
            }
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };

            // Work on Send `InviteCandidate`s; convert to UI rows (non-Send
            // images) inside the event loop below.
            let mut cands: Vec<InviteCandidate> = ui_ctx
                .list_channel_invite_candidates(&channel_id)
                .await
                .unwrap_or_default();
            let trimmed = query.trim();

            if !trimmed.is_empty() {
                cands.retain(|c| c.matches(trimmed));
            }

            let mut seen: HashSet<UserId> = cands.iter().map(|c| c.user_id).collect();

            // Signal-style exact-username lookup: display names are
            // profile-key-encrypted on the server, so only exact username
            // matches can resolve to an account.
            if !trimmed.is_empty() {
                match ui_ctx.lookup_by_username(trimmed).await {
                    Ok(Some(user_id)) => {
                        if seen.insert(user_id) {
                            match ui_ctx.get_user_profile(&user_id).await {
                                Ok(profile) => {
                                    let username = ui_ctx
                                        .get_user_local(&user_id)
                                        .map(|u| u.username)
                                        .unwrap_or_else(|| trimmed.to_string());
                                    cands.push(InviteCandidate {
                                        user_id,
                                        display_name: profile.display_name,
                                        username,
                                        avatar_bytes: profile.avatar_bytes.unwrap_or_default(),
                                        // ponytail: non-contact found by exact username; if already a
                                        // member the invite RPC rejects it server-side.
                                        already_member: false,
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, %user_id, "lookup hit but profile fetch failed");
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, query = %trimmed, "username lookup failed");
                    }
                }
            }

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let friends: Vec<InviteUser> =
                        cands.into_iter().map(candidate_to_invite_user).collect();
                    w.set_invite_member_users(ModelRc::new(VecModel::from(friends)));
                    w.set_invite_member_selected_count(0);
                }
            });
        });
    });
}
