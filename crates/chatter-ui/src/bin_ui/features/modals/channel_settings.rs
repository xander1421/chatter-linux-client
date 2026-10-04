//! Channel settings modal callbacks.

use std::collections::HashSet;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::{ChatterClient, InviteCandidate};
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::ChannelType;

use crate::{BannedMemberData, ChannelMemberData, InviteUser, MainWindow, PrivateGroupFriend};

/// Wire channel settings modal callbacks.
pub fn wire_channel_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_save_settings(window, ui_ctx);
    wire_pick_icon(window, ui_ctx);
    wire_notification_settings(window, ui_ctx);
    wire_member_management(window, ui_ctx);
    wire_moderation(window, ui_ctx);
    wire_transfer_ownership(window, ui_ctx);
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

        let can_leave = ui_ctx.can_leave_channel(&channel_id);

        let created_at = channel.created_at.with_timezone(&chrono::Local).format("%b %d, %Y").to_string();

        // Core shapes the rows; the images are decoded on the UI thread below
        // (a slint::Image is not Send).
        let (members, bans) = if !is_dm {
            load_member_lists(&ui_ctx, &channel_id).await
        } else {
            (vec![], None)
        };

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
        let muted = ui_ctx
            .is_conversation_muted(chatter_core::state::ActiveConversation::Channel(channel_id))
            .unwrap_or(false);

        let weak_access = weak_window.clone();
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
                w.set_channel_settings_muted(muted);
                w.set_channel_settings_type(channel_type_str.into());
                w.set_channel_settings_archived(is_archived);
                w.set_channel_settings_created_at(created_at.into());
                w.set_channel_settings_can_edit(can_edit);
                w.set_channel_settings_can_manage_members(can_manage_members);
                w.set_channel_settings_can_archive(can_archive);
                w.set_channel_settings_can_delete(can_delete);
                w.set_channel_settings_is_owner(is_owner);
                w.set_channel_settings_can_leave(can_leave);
                apply_member_lists(&w, &members, bans.as_deref());
                crate::bin_ui::features::chat::membership::reset_access(&w);
                w.set_channel_settings_loading(false);
                crate::bin_ui::features::back::push(&w, "channel-settings");
            }
        });
        // Queued after the reset above, so its answer is not wiped by it.
        crate::bin_ui::features::chat::membership::load_access(weak_access, ui_ctx, channel_id);
    });
}

type BannedMembers = Option<Vec<chatter_core::domain::channel::BannedMember>>;

/// Fetch the members pane's two lists. Bans come after members: the member
/// fetch refreshes our local role, which decides whether core fetches bans
/// at all (`None` = our role cannot see them).
async fn load_member_lists(
    ui_ctx: &ChatterClient,
    channel_id: &ChannelId,
) -> (Vec<chatter_core::domain::channel::MemberWithProfile>, BannedMembers) {
    let members = ui_ctx
        .list_members_with_profiles(channel_id, 100, 0)
        .await
        .unwrap_or_default();
    let bans = ui_ctx.list_banned_members(channel_id).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to list channel bans");
        None
    });
    (members, bans)
}

/// UI thread only (decodes avatars into `slint::Image`s).
fn apply_member_lists(
    w: &MainWindow,
    members: &[chatter_core::domain::channel::MemberWithProfile],
    bans: Option<&[chatter_core::domain::channel::BannedMember]>,
) {
    let rows: Vec<ChannelMemberData> =
        members.iter().map(crate::bin_ui::converters::member_to_slint).collect();
    w.set_channel_settings_member_count(rows.len() as i32);
    w.set_channel_settings_members(ModelRc::new(VecModel::from(rows)));
    w.set_channel_settings_can_view_bans(bans.is_some());
    let banned: Vec<BannedMemberData> = bans
        .unwrap_or_default()
        .iter()
        .map(crate::bin_ui::converters::banned_member_to_slint)
        .collect();
    w.set_channel_settings_banned_members(ModelRc::new(VecModel::from(banned)));
}

/// Re-fetch the members pane after a moderation action, leaving the rest of
/// the modal (unsaved name/description drafts) alone.
fn refresh_member_lists(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, channel_id: ChannelId) {
    tokio::spawn(async move {
        let (members, bans) = load_member_lists(&ui_ctx, &channel_id).await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                apply_member_lists(&w, &members, bans.as_deref());
            }
        });
    });
}

/// Re-fetch the members pane (and with it the General pane's "N members")
/// when the roster of the channel it is showing changes.
///
/// The pane is a snapshot taken when the modal opened, while the chat header
/// is updated on every membership event — so with settings open (or opened
/// just before the join landed), the header read "2 members" and settings
/// still "1 member". Called from the same membership-event path that updates
/// the header — for private groups too (`anon.rs` passes the group id), whose
/// roster is re-read from the local anon-group store instead of the server.
pub fn refresh_members_if_open(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, channel_id: ChannelId) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let id = channel_id.to_string();
        let open = w.get_channel_settings_id() == id.as_str()
            && w.get_modal_stack().iter().any(|s| s == "channel-settings");
        if !open {
            return;
        }
        if w.get_channel_settings_type() != "private-group" {
            refresh_member_lists(weak.clone(), ui_ctx, channel_id);
            return;
        }
        let Ok(group_id) = id.parse::<AnonGroupId>() else { return };
        let weak = weak.clone();
        std::thread::spawn(move || {
            let Some(roster) = load_private_group_roster(&ui_ctx, group_id) else { return };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                // Still the same group's settings: the read took a moment.
                if w.get_channel_settings_id() == id.as_str() {
                    apply_private_group_roster(&w, roster);
                }
            });
        });
    });
}

/// A private group's settings roster, read from the local anon-group store.
/// Only `Send` data: avatars stay bytes until `apply_private_group_roster`
/// decodes them on the UI thread.
pub struct PrivateGroupRoster {
    pub name: String,
    is_owner: bool,
    can_manage_members: bool,
    /// (user id, display name, role, avatar bytes)
    members: Vec<(String, String, &'static str, Option<Vec<u8>>)>,
}

/// Blocking DB reads — call off the UI thread. `None` when the group is not
/// stored locally or no user is signed in.
pub fn load_private_group_roster(ui_ctx: &ChatterClient, group_id: AnonGroupId) -> Option<PrivateGroupRoster> {
    let Some(group) = ui_ctx.get_anon_group(&group_id) else {
        tracing::error!("Private group not found: {}", group_id);
        return None;
    };
    let current_user_id = ui_ctx.session_current_user_id()?;
    let channel_id: ChannelId = group_id.into();
    let members = ui_ctx
        .get_anon_group_members(&group_id)
        .into_iter()
        .map(|member| {
            let display_name = ui_ctx.display_name_in(
                &chatter_core::domain::message::ConversationType::AnonGroup,
                &channel_id,
                &member.user_id,
            );
            (member.user_id.to_string(), display_name, member.role.as_str(), member.avatar)
        })
        .collect();
    Some(PrivateGroupRoster {
        is_owner: group.is_current_user_owner(&current_user_id),
        can_manage_members: group.can_manage_members(),
        name: group.name,
        members,
    })
}

/// UI thread only (decodes avatars into `slint::Image`s). Fills the members
/// pane and the General pane's count; leaves the name field (a draft) alone.
pub fn apply_private_group_roster(w: &MainWindow, roster: PrivateGroupRoster) {
    w.set_channel_settings_member_count(roster.members.len() as i32);
    w.set_channel_settings_is_owner(roster.is_owner);
    w.set_channel_settings_can_manage_members(roster.can_manage_members);
    let rows: Vec<ChannelMemberData> = roster
        .members
        .into_iter()
        .map(|(user_id, display_name, role, avatar)| ChannelMemberData {
            user_id: user_id.into(),
            display_name: display_name.into(),
            username: "".into(),
            role: role.into(),
            joined_at: "".into(),
            avatar_source: avatar
                .as_deref()
                .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                .unwrap_or_default(),
            // Anon groups have no mute/ban RPCs.
            muted: false,
            can_ban: false,
            can_mute: false,
            can_make_owner: false,
            // Anon-group membership is owner-authoritative,
            // not the signed channel log.
            unverified: false,
        })
        .collect();
    w.set_channel_settings_members(ModelRc::new(VecModel::from(rows)));
}

fn wire_save_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_channel_settings_save(move |name, description| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let name_str = name.to_string();
        let desc_str = description.to_string();

        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        // The picked icon file (if any) so Save uploads it alongside the
        // name/description. Empty path = icon unchanged.
        let icon_path = weak
            .upgrade()
            .map(|w| w.get_channel_settings_icon_path().to_string())
            .unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

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
                        if let Some(w) = weak.upgrade() { crate::bin_ui::features::back::close(&w, "channel-settings"); }
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

    // Mute DOES persist: `mute_conversation` writes a per-channel settings key
    // (not the moderator-mute column) — the exact bug F-UI-1 documented (a
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
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        tokio::spawn(async move {
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
                // Snap the switch back so it never shows a state we did not
                // manage to persist (same as the AI toggle above).
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() { w.set_channel_settings_muted(!muted); }
                });
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

        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let user_uuid = match user_id_str.parse::<uuid::Uuid>() {
                Ok(id) => chatter_core::domain::ids::UserId::from(id),
                Err(_) => return,
            };
            let Some(new_role) = chatter_core::domain::channel::MemberRole::parse(&role_str) else { return };
            let result = ui_ctx.update_channel_member_role(&channel_uuid, &user_uuid, new_role).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                match result {
                    Ok(m) => {
                        w.set_channel_settings_error("".into());
                        let members = w.get_channel_settings_members();
                        for i in 0..members.row_count() {
                            if let Some(mut row) = members.row_data(i) {
                                if row.user_id == user_id_str.as_str() {
                                    row.role = m.role.as_str().into();
                                    members.set_row_data(i, row);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to change member role");
                        w.set_channel_settings_error(format!("Couldn't change role: {e}").into());
                    }
                }
            });
        });
    });

    let weak_window = window.as_weak();
    window.on_channel_settings_remove_member(move |user_id| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let user_id_str = user_id.to_string();

        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        tokio::spawn(async move {
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

/// A members-pane moderation action; core does the work and the server
/// authorizes it (the buttons only appear where core's `MemberModeration`
/// says the server will accept).
#[derive(Clone, Copy)]
enum ModAction {
    Ban,
    Unban,
    /// `None` = until unmuted.
    Mute(Option<std::time::Duration>),
    Unmute,
}

fn wire_moderation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let run = {
        let weak_window = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        move |user_id: slint::SharedString, action: ModAction| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
            let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
            let (Ok(channel_id), Ok(user_id)) =
                (channel_id.parse::<ChannelId>(), user_id.as_str().parse::<UserId>())
            else {
                return;
            };
            tokio::spawn(async move {
                let result = match action {
                    ModAction::Ban => ui_ctx.ban_user(&channel_id, &user_id, None).await,
                    ModAction::Unban => ui_ctx.unban_user(&channel_id, &user_id).await,
                    ModAction::Mute(duration) => ui_ctx.mute_user(&channel_id, &user_id, duration).await,
                    ModAction::Unmute => ui_ctx.unmute_user(&channel_id, &user_id).await,
                };
                let error = match &result {
                    Ok(()) => String::new(),
                    Err(e) => {
                        tracing::warn!(error = %e, "Channel moderation action failed");
                        let verb = match action {
                            ModAction::Ban => "ban",
                            ModAction::Unban => "unban",
                            ModAction::Mute(_) => "mute",
                            ModAction::Unmute => "unmute",
                        };
                        format!("Couldn't {verb}: {e}")
                    }
                };
                let weak_err = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_err.upgrade() {
                        w.set_channel_settings_error(error.into());
                    }
                });
                refresh_member_lists(weak, ui_ctx, channel_id);
            });
        }
    };

    let r = run.clone();
    window.on_channel_settings_ban_member(move |user_id| r(user_id, ModAction::Ban));
    let r = run.clone();
    window.on_channel_settings_unban_member(move |user_id| r(user_id, ModAction::Unban));
    // minutes <= 0 = until unmuted.
    window.on_channel_settings_set_member_muted(move |user_id, muted, minutes| {
        let action = if !muted {
            ModAction::Unmute
        } else {
            ModAction::Mute(u64::try_from(minutes).ok().filter(|&m| m > 0).map(|m| std::time::Duration::from_secs(m * 60)))
        };
        run(user_id, action)
    });
}

/// Members › ⋮ › "Make owner…" (confirmed in the pane). On success we are an
/// admin, so the whole modal is reloaded: the owner-only controls go and
/// Leave appears.
fn wire_transfer_ownership(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_channel_settings_transfer_ownership(move |user_id| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        let (Ok(channel_uuid), Ok(new_owner)) =
            (channel_id.parse::<ChannelId>(), user_id.as_str().parse::<UserId>())
        else {
            return;
        };
        tokio::spawn(async move {
            let result = ui_ctx.transfer_channel_ownership(&channel_uuid, &new_owner).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        w.set_channel_settings_error("".into());
                        let still_open = w.get_channel_settings_id() == channel_id.as_str()
                            && w.get_modal_stack().iter().any(|s| s == "channel-settings");
                        if still_open {
                            open_channel_settings(weak.clone(), ui_ctx, channel_uuid);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%channel_uuid, error = %e, "Ownership transfer failed");
                        w.set_channel_settings_error(format!("Couldn't transfer ownership: {e}").into());
                    }
                }
            });
        });
    });
}

fn wire_channel_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_archive = ui_ctx.clone();

    window.on_channel_settings_archive(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_archive.clone();
        let currently_archived = weak.upgrade().map(|w| w.get_channel_settings_archived()).unwrap_or(false);
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let result = ui_ctx.archive_channel(&channel_uuid, !currently_archived).await;
            if result.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() { crate::bin_ui::features::back::close(&w, "channel-settings"); }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_leave = ui_ctx.clone();
    window.on_channel_settings_leave(move || {
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak_window.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        leave_channel(weak_window.clone(), ui_ctx_leave.clone(), channel_id);
    });

    // The sidebar row's long-press menu.
    let weak_window = window.as_weak();
    let ui_ctx_leave = ui_ctx.clone();
    window.on_leave_channel(move |channel_id| {
        leave_channel(weak_window.clone(), ui_ctx_leave.clone(), channel_id.to_string());
    });

    let weak_window = window.as_weak();
    let ui_ctx_delete = ui_ctx.clone();
    window.on_channel_settings_delete(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_delete.clone();
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak.upgrade().map(|w| w.get_channel_settings_id().to_string()).unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() { return; }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            if ui_ctx.delete_channel(&channel_uuid).await.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        crate::bin_ui::features::back::close(&w, "channel-settings");
                        ui_ctx.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::None);
                        w.set_channel_name("".into());
                        crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
                    }
                });
            }
        });
    });
}

/// Leave `channel_id` (Channel Settings and the sidebar row menu). Core files
/// "You left the channel"; the row stays, read-only, like a removal.
fn leave_channel(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, channel_id: String) {
    let Ok(channel_uuid) = channel_id.parse::<uuid::Uuid>().map(ChannelId::from) else { return };
    tokio::spawn(async move {
        let result = ui_ctx.leave_channel(&channel_uuid).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            match result {
                Ok(()) => {
                    if w.get_channel_settings_id() == channel_id.as_str() {
                        crate::bin_ui::features::back::close(&w, "channel-settings");
                    }
                    // An open chat stays open, read-only: the leave's
                    // ChannelMemberLeft swaps its composer for the notice.
                    crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
                }
                Err(e) => {
                    tracing::warn!(%channel_uuid, error = %e, "Leave channel failed");
                    // A toast, not the modal's error line: the row menu has no modal.
                    w.set_toast_message(format!("Couldn't leave the channel: {e}").into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                }
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

        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let group_id_str = weak
            .upgrade()
            .map(|w| w.get_channel_settings_id().to_string())
            .unwrap_or_default();

        tokio::spawn(async move {

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

            let friend_data: Vec<PrivateGroupFriend> = ui_ctx
                .list_anon_group_invite_candidates(Some(&group_id))
                .into_iter()
                .map(|c| PrivateGroupFriend {
                    user_id: c.user_id.to_string().into(),
                    display_name: c.display_name.into(),
                    username: if c.is_anonymous { "(anonymous)".into() } else { "".into() },
                    selected: false,
                    role: "member".into(),
                })
                .collect();

            // Update UI - close settings modal and open invite modal
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Store the group ID for the invite
                    w.set_invite_private_group_id(group_id_str.into());
                    w.set_invite_private_group_friends(ModelRc::new(VecModel::from(friend_data)));
                    w.set_invite_private_group_selected_count(0);
                    crate::bin_ui::features::back::push(&w, "invite-private-group");
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
                crate::bin_ui::features::back::push(&w, "invite-member");
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
        banned: c.banned,
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
                    let count = users.iter()
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

        // Read the selection HERE, on the UI thread. It used to be read inside
        // the spawned task, where a Slint Weak never upgrades (the window is
        // thread-bound): the task returned before its first log line and
        // "Invite" did nothing at all.
        let (channel_id_str, selected_user_ids) = {
            let Some(w) = weak.upgrade() else { return };
            let channel_id = w.get_invite_member_channel_id().to_string();
            let users = w.get_invite_member_users();
            let selected_vec: Vec<String> = users.iter()
                .filter(|u| u.selected && !u.already_member && !u.banned)
                .map(|u| u.id.to_string())
                .collect();
            (channel_id, selected_vec)
        };

        tokio::spawn(async move {

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
            // Why the last failure failed, for the toast (F76: a server refusal
            // only reached the log, so the inviter could not tell why).
            let mut failure_reason = String::new();
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
                        failure_reason = e.user_message();
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
                        if attempted == 1 { format!("The invite was not sent. {failure_reason}") }
                        else { format!("None of the {attempted} invites were sent. {failure_reason}") }
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
                        format!("{success_count} of {attempted} invites sent — {failed} failed. {failure_reason}").into(),
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
                crate::bin_ui::features::back::close(&w, "invite-member");
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

    // Keystroke sequence number: a lookup only fires once typing has paused
    // on a complete handle, and a newer keystroke retires older searches.
    static SEARCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    window.on_invite_member_search_changed(move |query: slint::SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_search.clone();
        let query = query.to_string();
        let seq = SEARCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;

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
            // matches can resolve to an account. Core refuses anything that is
            // not a complete handle; the pause keeps `alice.47` → `alice.471`
            // → `alice.4711` from probing every discriminator on the way.
            if chatter_core::services::user::is_lookup_handle(trimmed) {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if SEARCH_SEQ.load(std::sync::atomic::Ordering::SeqCst) != seq {
                    return;
                }
                match ui_ctx.lookup_by_username(trimmed).await {
                    Ok(Some(user_id)) => {
                        if seen.insert(user_id) {
                            match ui_ctx.get_user_profile(&user_id).await {
                                Ok(profile) => {
                                    let username = ui_ctx
                                        .known_username(&user_id)
                                        .unwrap_or_else(|| trimmed.to_string());
                                    cands.push(InviteCandidate {
                                        user_id,
                                        display_name: profile.display_name,
                                        username,
                                        avatar_bytes: profile.avatar_bytes.unwrap_or_default(),
                                        // non-contact found by exact username; if already a
                                        // member or banned the invite RPC rejects it server-side.
                                        already_member: false,
                                        banned: false,
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
