//! Channel settings modal callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::{ChatterClient, InviteCandidate};
use chatter_core::domain::ChannelType;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::ConversationType;
use chatter_core::state::ActiveConversation;

use crate::{BannedMemberData, ChannelMemberData, InviteUser, MainWindow, PrivateGroupFriend};

// UI-thread owned staging. Native picker cache files are never uploaded until Save.
// Model identity retires a picker after any modal navigation, including closing
// and reopening the same conversation; the sequence retires competing pickers.
#[derive(Clone)]
struct IconRequest {
    generation: u64,
    modal_stack: ModelRc<slint::SharedString>,
    conversation: ActiveConversation,
    session_revision: u64,
    settings_id: String,
}
#[derive(Default)]
struct IconState {
    generation: u64,
    pending: Option<IconRequest>,
    staged: Option<(IconRequest, Vec<u8>)>,
}
thread_local! {
    static ICON_STATE: std::cell::RefCell<IconState> = Default::default();
    static SETTINGS_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn icon_draft_current(w: &MainWindow, ctx: &ChatterClient, request: &IconRequest) -> bool {
    w.get_active_modal() == "channel-settings"
        && w.get_channel_settings_can_edit()
        && w.get_channel_settings_id().as_str() == request.settings_id
        && ctx.active_conversation() == request.conversation
        && ctx.composer_session_is_current(request.session_revision)
}

fn icon_request_current(w: &MainWindow, ctx: &ChatterClient, request: &IconRequest) -> bool {
    icon_draft_current(w, ctx, request) && w.get_modal_stack() == request.modal_stack
}

fn begin_icon_request(w: &MainWindow, ctx: &ChatterClient) -> Option<u64> {
    if w.get_active_modal() != "channel-settings" || !w.get_channel_settings_can_edit() {
        return None;
    }
    if !matches!(ctx.active_conversation(), ActiveConversation::Channel(id)
        if id.to_string() == w.get_channel_settings_id().as_str())
    {
        return None;
    }
    Some(ICON_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.pending = Some(IconRequest {
            generation,
            modal_stack: w.get_modal_stack(),
            conversation: ctx.active_conversation(),
            session_revision: ctx.auth_state().get().session_revision,
            settings_id: w.get_channel_settings_id().to_string(),
        });
        generation
    }))
}

fn finish_icon_request(
    w: &MainWindow,
    ctx: &ChatterClient,
    generation: u64,
    result: Result<Option<(std::path::PathBuf, Vec<u8>)>, String>,
) {
    ICON_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(request) = state.pending.as_ref() else {
            return;
        };
        if request.generation != generation || !icon_request_current(w, ctx, request) {
            return;
        }
        let request = state.pending.take().unwrap();
        match result {
            Ok(Some((path, data))) => {
                match crate::bin_ui::slint_conversions::memory_to_slint_image(&data) {
                    Ok(image) => {
                        w.set_channel_settings_icon_path(path.to_string_lossy().as_ref().into());
                        w.set_channel_settings_icon_preview(image);
                        w.set_channel_settings_has_icon_preview(true);
                        w.set_channel_settings_error("".into());
                        state.staged = Some((request, data));
                    }
                    Err(error) => w.set_channel_settings_error(
                        format!("Couldn't open channel icon: {error}").into(),
                    ),
                }
            }
            Ok(None) => {
                // The native adapter reports both cancellation and launch failure
                // as no selection. Give truthful feedback without inventing a cause.
                w.set_toast_message("No icon selected. Your draft is kept.".into());
                w.set_toast_variant("info".into());
                w.set_toast_visible(true);
            }
            Err(error) => {
                w.set_channel_settings_error(format!("Couldn't open channel icon: {error}").into())
            }
        }
    });
}

/// Wire channel settings modal callbacks.
pub fn wire_channel_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_save_settings(window, ui_ctx);
    wire_pick_icon(window, ui_ctx);
    wire_notification_settings(window, ui_ctx);
    wire_member_management(window, ui_ctx);
    wire_moderation(window, ui_ctx);
    wire_transfer_ownership(window, ui_ctx);
    wire_group_blocking(window, ui_ctx);
    wire_channel_actions(window, ui_ctx);
    wire_invite_members(window, ui_ctx);
    wire_invite_channel_members(window, ui_ctx);
    wire_invite_member_search(window, ui_ctx);
}

/// Paint conversation settings only for the channel or private group the
/// click was for. A later sidebar switch must not open or fill that screen.
pub(crate) fn settings_open_allowed(
    active: ActiveConversation,
    opened: ActiveConversation,
) -> bool {
    active == opened
        && matches!(
            opened,
            ActiveConversation::Channel(_) | ActiveConversation::AnonGroup(_)
        )
}

/// A roster or error paint may update the screen already open for `opened_id`.
/// It must not reopen settings, and it must not write a conversation the user left.
pub(crate) fn settings_refresh_allowed(
    active: ActiveConversation,
    opened: ActiveConversation,
    settings_id: &str,
    opened_id: &str,
    settings_on_stack: bool,
) -> bool {
    settings_open_allowed(active, opened) && settings_on_stack && settings_id == opened_id
}

fn settings_on_stack(w: &MainWindow) -> bool {
    w.get_modal_stack()
        .iter()
        .any(|s| s.as_str() == "channel-settings")
}

/// Show the settings screen before any local or network read. Destructive
/// actions stay off until the channel row says the user may take them.
pub(crate) fn show_channel_settings_shell(w: &MainWindow, id: &str, kind: &str) {
    SETTINGS_EPOCH.with(|epoch| epoch.set(epoch.get().wrapping_add(1)));
    ICON_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.pending = None;
        state.staged = None;
    });
    w.set_channel_settings_id(id.into());
    w.set_channel_settings_type(kind.into());
    w.set_channel_settings_name("".into());
    w.set_channel_settings_description("".into());
    w.set_channel_settings_name_draft("".into());
    w.set_channel_settings_description_draft("".into());
    w.set_channel_settings_created_at("".into());
    w.set_channel_settings_archived(false);
    w.set_channel_settings_ai_enabled(false);
    w.set_channel_settings_muted(false);
    w.set_channel_settings_icon_path("".into());
    w.set_channel_settings_has_icon_preview(false);
    w.set_channel_settings_can_edit(false);
    w.set_channel_settings_can_manage_members(false);
    w.set_channel_settings_can_archive(false);
    w.set_channel_settings_can_delete(false);
    w.set_channel_settings_can_leave(false);
    w.set_channel_settings_is_owner(false);
    w.set_channel_settings_member_count(0);
    w.set_channel_settings_members(ModelRc::default());
    w.set_channel_settings_can_view_bans(false);
    w.set_channel_settings_banned_members(ModelRc::default());
    w.set_channel_settings_loading(true);
    w.set_channel_settings_error("".into());
    crate::bin_ui::features::chat::membership::reset_access(w);
    crate::bin_ui::features::back::push(w, "channel-settings");
}

/// Put `message` on the settings screen. The error banner lives on that
/// screen, so a failure that only sets the property is invisible.
pub(crate) fn present_settings_error(w: &MainWindow, id: &str, kind: &str, message: &str) {
    if w.get_channel_settings_id().as_str() != id || !settings_on_stack(w) {
        show_channel_settings_shell(w, id, kind);
    }
    w.set_channel_settings_loading(false);
    w.set_channel_settings_error(message.into());
}

struct LocalChannelSettings {
    id: String,
    name: String,
    description: String,
    channel_type: &'static str,
    archived: bool,
    created_at: String,
    ai_enabled: bool,
    muted: bool,
    can_edit: bool,
    can_manage_members: bool,
    can_archive: bool,
    can_delete: bool,
    is_owner: bool,
    can_leave: bool,
    member_count: i32,
    roster_pending: bool,
}

fn present_local_channel_settings(w: &MainWindow, settings: &LocalChannelSettings) {
    w.set_channel_settings_id(settings.id.as_str().into());
    w.set_channel_settings_name(settings.name.as_str().into());
    w.set_channel_settings_description(settings.description.as_str().into());
    // Reset the editable drafts to the stored values so reopening a
    // different channel doesn't carry the previous edits.
    w.set_channel_settings_name_draft(settings.name.as_str().into());
    w.set_channel_settings_description_draft(settings.description.as_str().into());
    // Clear any icon picked in a previous open.
    w.set_channel_settings_icon_path("".into());
    w.set_channel_settings_has_icon_preview(false);
    w.set_channel_settings_ai_enabled(settings.ai_enabled);
    w.set_channel_settings_muted(settings.muted);
    w.set_channel_settings_type(settings.channel_type.into());
    w.set_channel_settings_archived(settings.archived);
    w.set_channel_settings_created_at(settings.created_at.as_str().into());
    w.set_channel_settings_can_edit(settings.can_edit);
    w.set_channel_settings_can_manage_members(settings.can_manage_members);
    w.set_channel_settings_can_archive(settings.can_archive);
    w.set_channel_settings_can_delete(settings.can_delete);
    w.set_channel_settings_is_owner(settings.is_owner);
    w.set_channel_settings_can_leave(settings.can_leave);
    w.set_channel_settings_member_count(settings.member_count);
    // Drop the previous channel's rows. The roster RPC fills this pane
    // after the screen is already up.
    w.set_channel_settings_members(ModelRc::default());
    w.set_channel_settings_can_view_bans(false);
    w.set_channel_settings_banned_members(ModelRc::default());
    w.set_channel_settings_loading(settings.roster_pending);
    w.set_channel_settings_error("".into());
}

fn paint_if_current(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    opened: ActiveConversation,
    opened_id: String,
    paint: impl FnOnce(&MainWindow) + Send + 'static,
) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let current_id = w.get_channel_settings_id();
        if !settings_refresh_allowed(
            ui_ctx.chat_state().active_conversation(),
            opened,
            current_id.as_str(),
            &opened_id,
            settings_on_stack(&w),
        ) {
            return;
        }
        paint(&w);
    });
}

/// Open the channel settings modal with channel data.
///
/// The screen is pushed before the member and ban RPCs. Those calls use the
/// unary deadline (up to 75s), and the menu has already closed, so waiting
/// to push left the chat with no settings and no error.
pub fn open_channel_settings(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
) {
    let opened = ActiveConversation::Channel(channel_id);
    let channel_id_str = channel_id.to_string();
    if settings_open_allowed(ui_ctx.chat_state().active_conversation(), opened) {
        if let Some(w) = weak_window.upgrade() {
            let already =
                w.get_channel_settings_id().as_str() == channel_id_str && settings_on_stack(&w);
            if !already {
                // Type is filled from the local row. Empty hides the access
                // pane until that row says public or private.
                show_channel_settings_shell(&w, &channel_id_str, "");
            }
        }
    }

    tokio::spawn(async move {
        if !settings_open_allowed(ui_ctx.chat_state().active_conversation(), opened) {
            return;
        }

        let channel = match ui_ctx.get_channel_local(&channel_id) {
            Some(c) => c,
            None => {
                paint_if_current(weak_window, ui_ctx, opened, channel_id_str, |w| {
                    w.set_channel_settings_loading(false);
                    w.set_channel_settings_error("Channel not found".into());
                    w.set_channel_settings_can_edit(false);
                    w.set_channel_settings_can_manage_members(false);
                    w.set_channel_settings_can_archive(false);
                    w.set_channel_settings_can_delete(false);
                    w.set_channel_settings_can_leave(false);
                });
                return;
            }
        };

        let current_user_id = ui_ctx.session_current_user_id();
        let is_dm = channel.is_dm();
        let (is_owner, can_edit, can_manage_members, can_archive, can_delete) =
            match current_user_id.as_ref() {
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
        let created_at = channel
            .created_at
            .with_timezone(&chrono::Local)
            .format("%b %d, %Y")
            .to_string();
        let channel_type_str = match channel.channel_type {
            ChannelType::Public => "public",
            ChannelType::Private => "private",
            ChannelType::DirectMessage => "dm",
            ChannelType::SavedMessages => "saved",
        };
        let local = LocalChannelSettings {
            id: channel_id_str.clone(),
            name: channel.name.clone(),
            description: channel.description.clone().unwrap_or_default(),
            channel_type: channel_type_str,
            archived: channel.archived,
            created_at,
            // Read the stored per-channel AI switch so the toggle opens showing
            // what is actually persisted, not a default.
            ai_enabled: ui_ctx.ai_enabled_for(&channel_id_str),
            muted: ui_ctx
                .is_conversation_muted(ActiveConversation::Channel(channel_id))
                .unwrap_or(false),
            can_edit,
            can_manage_members,
            can_archive,
            can_delete,
            is_owner,
            can_leave,
            member_count: i32::try_from(channel.member_count).unwrap_or(i32::MAX),
            roster_pending: !is_dm,
        };

        let weak_access = weak_window.clone();
        let ui_for_present = ui_ctx.clone();
        let id_for_present = channel_id_str.clone();
        paint_if_current(
            weak_window.clone(),
            ui_ctx.clone(),
            opened,
            id_for_present,
            move |w| {
                present_local_channel_settings(w, &local);
                // Queued after the reset in the shell, so its answer is not wiped.
                crate::bin_ui::features::chat::membership::reset_access(w);
                crate::bin_ui::features::chat::membership::load_access(
                    weak_access,
                    ui_for_present,
                    channel_id,
                );
            },
        );

        if is_dm {
            return;
        }

        // Core shapes the rows; the images are decoded on the UI thread below
        // (a slint::Image is not Send). This await must stay after the local
        // present above: the roster RPC is what used to hide the screen.
        let (members, bans, member_error) = load_member_lists(&ui_ctx, &channel_id).await;
        paint_if_current(weak_window, ui_ctx, opened, channel_id_str, move |w| {
            if let Some(message) = member_error {
                if !members.is_empty() {
                    apply_member_lists(w, &members, bans.as_deref());
                }
                w.set_channel_settings_loading(false);
                w.set_channel_settings_error(message.into());
                return;
            }
            apply_member_lists(w, &members, bans.as_deref());
            w.set_channel_settings_loading(false);
            w.set_channel_settings_error("".into());
        });
    });
}

type BannedMembers = Option<Vec<chatter_core::domain::channel::BannedMember>>;

/// Fetch the members pane's two lists. Bans come after members: the member
/// fetch refreshes our local role, which decides whether core fetches bans
/// at all (`None` = our role cannot see them).
async fn load_member_lists(
    ui_ctx: &ChatterClient,
    channel_id: &ChannelId,
) -> (
    Vec<chatter_core::domain::channel::MemberWithProfile>,
    BannedMembers,
    Option<String>,
) {
    let members = match ui_ctx.list_members_with_profiles(channel_id, 100, 0).await {
        Ok(members) => members,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to list channel members");
            return (vec![], None, Some(format!("Couldn't load members: {e}")));
        }
    };
    let bans = match ui_ctx.list_banned_members(channel_id).await {
        Ok(bans) => bans,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to list channel bans");
            return (
                members,
                None,
                Some(format!("Couldn't load banned members: {e}")),
            );
        }
    };
    (members, bans, None)
}

/// UI thread only (decodes avatars into `slint::Image`s).
fn apply_member_lists(
    w: &MainWindow,
    members: &[chatter_core::domain::channel::MemberWithProfile],
    bans: Option<&[chatter_core::domain::channel::BannedMember]>,
) {
    let rows: Vec<ChannelMemberData> = members
        .iter()
        .map(crate::bin_ui::converters::member_to_slint)
        .collect();
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
fn refresh_member_lists(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
) {
    tokio::spawn(async move {
        let (members, bans, error) = load_member_lists(&ui_ctx, &channel_id).await;
        paint_if_current(
            weak,
            ui_ctx,
            ActiveConversation::Channel(channel_id),
            channel_id.to_string(),
            move |w| {
                if let Some(error) = error {
                    w.set_channel_settings_error(error.into());
                } else {
                    apply_member_lists(w, &members, bans.as_deref());
                }
            },
        );
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
pub fn refresh_members_if_open(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
) {
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
        let Ok(group_id) = id.parse::<AnonGroupId>() else {
            return;
        };
        let weak = weak.clone();
        std::thread::spawn(move || {
            let Some(roster) = load_private_group_roster(&ui_ctx, group_id) else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let current_id = w.get_channel_settings_id();
                if settings_refresh_allowed(
                    ui_ctx.chat_state().active_conversation(),
                    ActiveConversation::AnonGroup(group_id),
                    current_id.as_str(),
                    &id,
                    settings_on_stack(&w),
                ) {
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
    can_leave: bool,
    blocked: bool,
    /// (user id, display name, role, avatar bytes, is us, blocked in group)
    members: Vec<(String, String, &'static str, Option<Vec<u8>>, bool, bool)>,
}

/// Blocking DB reads — call off the UI thread. `None` when the group is not
/// stored locally or no user is signed in.
pub fn load_private_group_roster(
    ui_ctx: &ChatterClient,
    group_id: AnonGroupId,
) -> Option<PrivateGroupRoster> {
    let Some(group) = ui_ctx.get_anon_group(&group_id) else {
        tracing::error!("Private group not found: {}", group_id);
        return None;
    };
    let current_user_id = ui_ctx.session_current_user_id()?;
    let channel_id: ChannelId = group_id.into();
    let blocked_members = ui_ctx.anon_group_blocked_members(&group_id);
    let members = ui_ctx
        .get_anon_group_members(&group_id)
        .into_iter()
        .map(|member| {
            let display_name = ui_ctx.display_name_in(
                &chatter_core::domain::message::ConversationType::AnonGroup,
                &channel_id,
                &member.user_id,
            );
            (
                member.user_id.to_string(),
                display_name,
                member.role.as_str(),
                member.avatar,
                member.user_id == current_user_id,
                blocked_members.contains(&member.user_id),
            )
        })
        .collect();
    Some(PrivateGroupRoster {
        is_owner: group.is_current_user_owner(&current_user_id),
        can_manage_members: group.can_manage_members(),
        can_leave: ui_ctx.can_leave_anon_group(&group_id),
        blocked: group.is_blocked(),
        name: group.name,
        members,
    })
}

/// UI thread only (decodes avatars into `slint::Image`s). Fills the members
/// pane and the General pane's count; leaves the name field (a draft) alone.
pub fn apply_private_group_roster(w: &MainWindow, roster: PrivateGroupRoster) {
    // A group has no server-side edit/archive/delete; Leave is core's call
    // (not the owner while others remain). Set every time: these properties
    // are shared with channel settings and would otherwise keep a channel's
    // values, offering server RPCs keyed by the group id.
    w.set_channel_settings_can_edit(false);
    w.set_channel_settings_can_archive(false);
    w.set_channel_settings_can_delete(false);
    w.set_channel_settings_can_leave(roster.can_leave);
    w.set_channel_settings_group_blocked(roster.blocked);
    w.set_channel_settings_member_count(roster.members.len() as i32);
    w.set_channel_settings_is_owner(roster.is_owner);
    w.set_channel_settings_can_manage_members(roster.can_manage_members);
    let is_owner = roster.is_owner;
    let rows: Vec<ChannelMemberData> = roster
        .members
        .into_iter()
        .map(
            |(user_id, display_name, role, avatar, is_me, blocked)| ChannelMemberData {
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
                can_make_owner: is_owner && !is_me,
                // Anon-group membership is owner-authoritative,
                // not the signed channel log.
                unverified: false,
                can_block: !is_me,
                blocked,
            },
        )
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
        let channel_id = weak
            .upgrade()
            .map(|w| w.get_channel_settings_id().to_string())
            .unwrap_or_default();
        // The picked icon file (if any) so Save uploads it alongside the
        // name/description. Empty path = icon unchanged.
        let icon_path = weak
            .upgrade()
            .map(|w| w.get_channel_settings_icon_path().to_string())
            .unwrap_or_default();
        let epoch = SETTINGS_EPOCH.with(|epoch| epoch.get());
        let staged_icon = weak.upgrade().and_then(|w| {
            ICON_STATE.with(|state| {
                state.borrow().staged.as_ref().and_then(|(request, bytes)| {
                    icon_draft_current(&w, &ui_ctx, request).then(|| bytes.clone())
                })
            })
        });
        let opened = ui_ctx.active_conversation();
        let session_revision = ui_ctx.auth_state().get().session_revision;
        tokio::spawn(async move {
            if channel_id.is_empty() {
                return;
            }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };

            let icon_bytes: Option<Vec<u8>> = if staged_icon.is_some() {
                staged_icon
            } else if icon_path.is_empty() {
                None
            } else {
                match std::fs::read(&icon_path) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        paint_if_current(weak, ui_ctx, opened, channel_id, move |w| {
                            if SETTINGS_EPOCH.with(|current| current.get() == epoch) {
                                w.set_channel_settings_error(
                                    format!("Couldn't read channel icon: {e}").into(),
                                );
                            }
                        });
                        return;
                    }
                }
            };

            let desc_opt = if desc_str.is_empty() {
                None
            } else {
                Some(desc_str.as_str())
            };
            let result = ui_ctx
                .update_channel(
                    &channel_uuid,
                    Some(&name_str),
                    desc_opt,
                    icon_bytes.as_deref(),
                )
                .await;
            if !ui_ctx.composer_session_is_current(session_revision) {
                return;
            }
            paint_if_current(weak, ui_ctx, opened, channel_id, move |w| {
                if !SETTINGS_EPOCH.with(|current| current.get() == epoch)
                    || w.get_active_modal() != "channel-settings"
                {
                    return;
                }
                match result {
                    Ok(_) => {
                        ICON_STATE.with(|state| {
                            let mut state = state.borrow_mut();
                            state.pending = None;
                            state.staged = None;
                        });
                        crate::bin_ui::features::back::close(w, "channel-settings");
                    }
                    Err(e) => w.set_channel_settings_error(e.to_string().into()),
                }
            });
        });
    });
}

// Reuse the native avatar/photo picker adapter, with modal-owned staging.
fn wire_pick_icon(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ctx = ui_ctx.clone();
    window.on_channel_settings_pick_icon(move || {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let Some(generation) = begin_icon_request(&w, &ctx) else {
            return;
        };
        let weak = w.as_weak();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let picked = {
                let config = chatter_core::media::FilePickerConfig::new()
                    .with_title("Select Channel Icon")
                    .with_filter(chatter_core::media::FileFilter::images());
                chatter_core::media::FilePicker::pick_file(config).await
            };
            #[cfg(target_os = "android")]
            let picked = crate::platform::android::file_picker::pick_files(&["image/*"])
                .await
                .into_iter()
                .next();
            #[cfg(target_os = "ios")]
            let picked: Option<std::path::PathBuf> = None;
            let result = match picked {
                Some(path) => std::fs::read(&path)
                    .map(|data| Some((path, data)))
                    .map_err(|e| e.to_string()),
                None => Ok(None),
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    finish_icon_request(&w, &ctx, generation, result);
                }
            });
        });
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
            if let Some(w) = weak.upgrade() {
                w.set_channel_settings_notification_pref(pref_str.into());
            }
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
            if let Some(w) = weak_ui.upgrade() {
                w.set_channel_settings_muted(muted);
            }
        });
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak
            .upgrade()
            .map(|w| w.get_channel_settings_id().to_string())
            .unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() {
                return;
            }
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
                    if let Some(w) = weak.upgrade() {
                        w.set_channel_settings_muted(!muted);
                    }
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
        let channel_id = weak
            .upgrade()
            .map(|w| w.get_channel_settings_id().to_string())
            .unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() {
                return;
            }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let user_uuid = match user_id_str.parse::<uuid::Uuid>() {
                Ok(id) => chatter_core::domain::ids::UserId::from(id),
                Err(_) => return,
            };
            let Some(new_role) = chatter_core::domain::channel::MemberRole::parse(&role_str) else {
                return;
            };
            let result = ui_ctx
                .update_channel_member_role(&channel_uuid, &user_uuid, new_role)
                .await;
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
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let Some(conv) = weak
            .upgrade()
            .and_then(|w| settings_conversation(&w, &ui_ctx))
        else {
            return;
        };
        let Ok(user) = user_id.as_str().parse::<UserId>() else {
            return;
        };
        tokio::spawn(async move {
            let result = ui_ctx.remove_conversation_member(conv, &user).await;
            match result {
                // A group kick emits no membership event here; re-read the roster.
                Ok(()) => {
                    if let Some(id) = conv.channel_id() {
                        refresh_members_if_open(weak, ui_ctx, id);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to remove member");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_channel_settings_error(
                                format!("Couldn't remove member: {e}").into(),
                            );
                        }
                    });
                }
            }
        });
    });
}

/// The conversation the settings modal is showing: a private group when it
/// was opened for one (`type == "private-group"`), otherwise a channel.
fn settings_conversation(w: &MainWindow, ui_ctx: &ChatterClient) -> Option<ActiveConversation> {
    let kind = if w.get_channel_settings_type() == "private-group" {
        ConversationType::AnonGroup
    } else {
        ConversationType::Channel
    };
    ui_ctx.resolve_row_conversation(w.get_channel_settings_id().as_str(), &kind)
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
            let channel_id = weak
                .upgrade()
                .map(|w| w.get_channel_settings_id().to_string())
                .unwrap_or_default();
            let (Ok(channel_id), Ok(user_id)) = (
                channel_id.parse::<ChannelId>(),
                user_id.as_str().parse::<UserId>(),
            ) else {
                return;
            };
            tokio::spawn(async move {
                let result = match action {
                    ModAction::Ban => ui_ctx.ban_user(&channel_id, &user_id, None).await,
                    ModAction::Unban => ui_ctx.unban_user(&channel_id, &user_id).await,
                    ModAction::Mute(duration) => {
                        ui_ctx.mute_user(&channel_id, &user_id, duration).await
                    }
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
            ModAction::Mute(
                u64::try_from(minutes)
                    .ok()
                    .filter(|&m| m > 0)
                    .map(|m| std::time::Duration::from_secs(m * 60)),
            )
        };
        run(user_id, action)
    });
}

/// Members › ⋮ › "Make owner…" (confirmed in the pane), for a channel or a
/// private group. On success we are an admin, so the modal is reloaded: the
/// owner-only controls go and Leave appears.
fn wire_transfer_ownership(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_channel_settings_transfer_ownership(move |user_id| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let Some(w) = weak.upgrade() else { return };
        let channel_id = w.get_channel_settings_id().to_string();
        let (Some(conv), Ok(channel_uuid), Ok(new_owner)) = (
            settings_conversation(&w, &ui_ctx),
            channel_id.parse::<ChannelId>(),
            user_id.as_str().parse::<UserId>(),
        ) else {
            return;
        };
        tokio::spawn(async move {
            let result = ui_ctx
                .transfer_conversation_ownership(conv, &new_owner)
                .await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                match result {
                    Ok(()) => {
                        w.set_channel_settings_error("".into());
                        let still_open = w.get_channel_settings_id() == channel_id.as_str()
                            && w.get_modal_stack().iter().any(|s| s == "channel-settings");
                        if still_open && matches!(conv, ActiveConversation::AnonGroup(_)) {
                            refresh_members_if_open(weak.clone(), ui_ctx, channel_uuid);
                        } else if still_open {
                            open_channel_settings(weak.clone(), ui_ctx, channel_uuid);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%channel_uuid, error = %e, "Ownership transfer failed");
                        w.set_channel_settings_error(
                            format!("Couldn't transfer ownership: {e}").into(),
                        );
                    }
                }
            });
        });
    });
}

/// Private group › Danger Zone › Block group, and Members › ⋮ › Block in
/// group (and their Unblock). Core stores the block and enforces it on
/// receive; this only forwards the choice and re-reads the roster.
fn wire_group_blocking(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    window.on_channel_settings_block_group(move |block| {
        change_group_block(&weak, &ctx, move |ctx, conv| {
            ctx.set_conversation_blocked(conv, block)
        });
    });
    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    window.on_channel_settings_block_member(move |user_id, block| {
        let Ok(user) = user_id.as_str().parse::<UserId>() else {
            return;
        };
        change_group_block(&weak, &ctx, move |ctx, conv| {
            ctx.set_conversation_member_blocked(conv, &user, block)
        });
    });
}

/// Apply a block change to the private group the settings modal shows (off
/// the event loop: it writes the store), then re-read its roster, or show
/// why it failed.
fn change_group_block(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    change: impl FnOnce(
        &Arc<ChatterClient>,
        ActiveConversation,
    ) -> Result<(), chatter_core::services::conversation::ConversationError>
    + Send
    + 'static,
) {
    // Read on the UI thread: a Slint Weak never upgrades off it.
    let Some(w) = weak.upgrade() else { return };
    let Some(conv @ ActiveConversation::AnonGroup(group_id)) = settings_conversation(&w, ui_ctx)
    else {
        return;
    };
    let (weak, ui_ctx) = (weak.clone(), ui_ctx.clone());
    std::thread::spawn(move || match change(&ui_ctx, conv) {
        Ok(()) => refresh_members_if_open(weak, ui_ctx, group_id.into()),
        Err(e) => {
            tracing::warn!(error = %e, "Group block change failed");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_channel_settings_error(format!("Couldn't change the block: {e}").into());
                }
            });
        }
    });
}

fn wire_channel_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_archive = ui_ctx.clone();

    window.on_channel_settings_archive(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_archive.clone();
        let currently_archived = weak
            .upgrade()
            .map(|w| w.get_channel_settings_archived())
            .unwrap_or(false);
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak
            .upgrade()
            .map(|w| w.get_channel_settings_id().to_string())
            .unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() {
                return;
            }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let result = ui_ctx
                .archive_channel(&channel_uuid, !currently_archived)
                .await;
            if result.is_ok() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        crate::bin_ui::features::back::close(&w, "channel-settings");
                    }
                });
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_leave = ui_ctx.clone();
    window.on_channel_settings_leave(move || {
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let Some(conv) = weak_window
            .upgrade()
            .and_then(|w| settings_conversation(&w, &ui_ctx_leave))
        else {
            return;
        };
        leave_conversation(weak_window.clone(), ui_ctx_leave.clone(), conv);
    });

    // The sidebar row's long-press menu.
    let weak_window = window.as_weak();
    let ui_ctx_leave = ui_ctx.clone();
    window.on_leave_conversation(move |row_id, kind| {
        let kind = crate::bin_ui::converters::conversation_type(kind);
        let Some(conv) = ui_ctx_leave.resolve_row_conversation(row_id.as_str(), &kind) else {
            return;
        };
        leave_conversation(weak_window.clone(), ui_ctx_leave.clone(), conv);
    });

    let weak_window = window.as_weak();
    let ui_ctx_delete = ui_ctx.clone();
    window.on_channel_settings_delete(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_delete.clone();
        // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
        let channel_id = weak
            .upgrade()
            .map(|w| w.get_channel_settings_id().to_string())
            .unwrap_or_default();
        tokio::spawn(async move {
            if channel_id.is_empty() {
                return;
            }
            let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                Ok(id) => ChannelId::from(id),
                Err(_) => return,
            };
            let result = ui_ctx.delete_channel(&channel_uuid).await;
            finish_channel_delete(weak, ui_ctx, channel_uuid, result);
        });
    });
}

/// Apply the awaited delete result on the same path used by Settings.
fn finish_channel_delete(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
    result: Result<(), chatter_core::services::channel::ChannelError>,
) {
    if result.is_ok() {
        crate::bin_ui::features::sidebar::conversation_removed(
            &weak,
            &ui_ctx,
            ActiveConversation::Channel(channel_id),
        );
    }
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        match result {
            Ok(()) => {
                // A late completion must not close a different settings screen.
                if w.get_channel_settings_id() == channel_id.to_string().as_str() {
                    crate::bin_ui::features::back::close(&w, "channel-settings");
                }
                crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
            }
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "Channel deletion failed");
                let message = format!("Couldn't delete channel: {}", e.user_message());
                if w.get_channel_settings_id() == channel_id.to_string().as_str() {
                    w.set_channel_settings_error(message.clone().into());
                }
                w.set_toast_message(message.into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
            }
        }
    });
}

/// Leave a channel or private group (Settings and the sidebar row menu).
/// A channel's row stays, read-only, like a removal (core files "You left
/// the channel"); a group is gone from this device, so its chat closes.
fn leave_conversation(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    conv: ActiveConversation,
) {
    let id = conv.channel_id().map(|c| c.to_string()).unwrap_or_default();
    tokio::spawn(async move {
        let result = ui_ctx.leave_conversation(conv).await;
        if result.is_ok() && matches!(conv, ActiveConversation::AnonGroup(_)) {
            crate::bin_ui::features::sidebar::conversation_removed(&weak, &ui_ctx, conv);
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            match result {
                Ok(()) => {
                    if w.get_channel_settings_id() == id.as_str() {
                        crate::bin_ui::features::back::close(&w, "channel-settings");
                    }
                    // An open chat stays open, read-only: the leave's
                    // ChannelMemberLeft swaps its composer for the notice.
                    crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
                }
                Err(e) => {
                    tracing::warn!(%id, error = %e, "Leave failed");
                    // A toast, not the modal's error line: the row menu has no modal.
                    w.set_toast_message(format!("Couldn't leave: {e}").into());
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
                    username: if c.is_anonymous {
                        "(anonymous)".into()
                    } else {
                        "".into()
                    },
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
pub fn open_invite_member_modal(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
) {
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
                let user_data: Vec<InviteUser> = candidates
                    .into_iter()
                    .map(candidate_to_invite_user)
                    .collect();
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
                    let count = users.iter().filter(|u| u.selected).count() as i32;
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

    // Keystroke sequence number: a newer keystroke retires older searches
    // (core asks after its pre-lookup pause).
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

            // Core filters and looks up; the rows (non-Send images) are
            // built inside the event loop below.
            let superseded = || SEARCH_SEQ.load(std::sync::atomic::Ordering::SeqCst) != seq;
            let Some(cands) = ui_ctx
                .search_channel_invite_candidates(&channel_id, &query, superseded)
                .await
            else {
                return;
            };

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

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use crate::bin_ui::converters::ChatRowExt;
    use chatter_core::services::channel::ChannelError;
    use chatter_core::storage::repositories::{
        channel_repo::ChannelRepository,
        settings_repo::{SettingsRepository, keys},
    };

    const CHILD_MARKER: &str = "CHATTER_UI_CHANNEL_DELETE_TEST_CHILD";
    const FIXTURE_COMPLETED: &str = "channel deletion fixture completed";

    fn run_isolated_delete_test(test_name: &str) {
        use std::io::{Read, Seek, SeekFrom};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        // Slint's integration backend may only be initialized once per process.
        // Re-run this exact test alone, preserving the inherited no-keyring env.
        // Files avoid pipe-buffer deadlocks even if the fixture logs heavily.
        let mut stdout = tempfile::tempfile().expect("create child stdout capture");
        let mut stderr = tempfile::tempfile().expect("create child stderr capture");
        let mut child = Command::new(std::env::current_exe().expect("locate UI test executable"))
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD_MARKER, test_name)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().unwrap()))
            .stderr(Stdio::from(stderr.try_clone().unwrap()))
            .spawn()
            .expect("spawn isolated channel deletion test");
        let deadline = Instant::now() + Duration::from_secs(45);
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => break Err("child exceeded 45-second timeout".to_owned()),
                Err(error) => break Err(format!("polling child failed: {error}")),
            }
        };
        // Never signal another test or service; reap only the child we spawned.
        let cleanup = if result.is_err() {
            let killed = child.kill();
            let reaped = child.wait();
            format!("; child kill: {killed:?}; child wait: {reaped:?}")
        } else {
            String::new()
        };
        let mut stdout_text = String::new();
        let mut stderr_text = String::new();
        stdout.seek(SeekFrom::Start(0)).unwrap();
        stderr.seek(SeekFrom::Start(0)).unwrap();
        stdout.read_to_string(&mut stdout_text).unwrap();
        stderr.read_to_string(&mut stderr_text).unwrap();
        assert!(
            matches!(&result, Ok(status) if status.success())
                && stderr_text.lines().any(|line| line == FIXTURE_COMPLETED),
            "isolated test {test_name}: {result:?}{cleanup}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}"
        );
    }

    #[test]
    fn icon_picker_stages_bytes_preserves_drafts_and_rejects_obsolete_completions() {
        let test_name = format!(
            "{}::icon_picker_stages_bytes_preserves_drafts_and_rejects_obsolete_completions",
            module_path!().split_once("::").unwrap().1
        );
        if std::env::var(CHILD_MARKER).ok().as_deref() != Some(test_name.as_str()) {
            run_isolated_delete_test(&test_name);
            return;
        }
        unsafe {
            std::env::set_var("CHATTER_NO_KEYRING", "1");
            std::env::set_var("CHATTER_DB_PASSPHRASE", "icon-picker-fixture");
            std::env::set_var("CHATTER_HEADLESS", "1");
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let fixture = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .data_dir(fixture.path().to_string_lossy().into_owned())
            .build()
            .unwrap();
        let w = MainWindow::new().unwrap();
        let id = ChannelId::new();
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(id));
        show_channel_settings_shell(&w, &id.to_string(), "private");
        w.set_channel_settings_can_edit(true);
        w.set_channel_settings_name_draft("Unsaved name".into());
        w.set_channel_settings_description_draft("Unsaved description".into());
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let picked = || {
            Ok(Some((
                std::path::PathBuf::from("staged-icon.png"),
                bytes.clone(),
            )))
        };
        let first = begin_icon_request(&w, &client).unwrap();
        let latest = begin_icon_request(&w, &client).unwrap();
        finish_icon_request(&w, &client, first, picked());
        assert!(!w.get_channel_settings_has_icon_preview());
        finish_icon_request(&w, &client, latest, picked());
        assert!(w.get_channel_settings_has_icon_preview());
        ICON_STATE.with(|s| assert_eq!(s.borrow().staged.as_ref().unwrap().1, bytes));
        assert_eq!(w.get_channel_settings_name_draft().as_str(), "Unsaved name");
        assert_eq!(
            w.get_channel_settings_description_draft().as_str(),
            "Unsaved description"
        );

        // The path intentionally does not exist: Save must use the frozen bytes,
        // then show core's offline failure and preserve the editable draft.
        wire_save_settings(&w, &client);
        w.invoke_channel_settings_save("Unsaved name".into(), "Unsaved description".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while w.get_channel_settings_error().is_empty() && std::time::Instant::now() < deadline {
            slint::Timer::single_shot(std::time::Duration::from_millis(25), || {
                slint::quit_event_loop().unwrap();
            });
            w.run().unwrap();
        }
        assert!(
            !w.get_channel_settings_error().is_empty(),
            "offline Save must report a failure"
        );
        assert!(
            !w.get_channel_settings_error()
                .contains("Couldn't read channel icon"),
            "Save must use staged bytes, not re-read the native cache file"
        );
        assert_eq!(w.get_active_modal().as_str(), "channel-settings");
        assert_eq!(w.get_channel_settings_name_draft().as_str(), "Unsaved name");
        assert_eq!(
            w.get_channel_settings_description_draft().as_str(),
            "Unsaved description"
        );

        let cancelled = begin_icon_request(&w, &client).unwrap();
        finish_icon_request(&w, &client, cancelled, Ok(None));
        assert!(w.get_channel_settings_has_icon_preview());
        let invalid = begin_icon_request(&w, &client).unwrap();
        finish_icon_request(
            &w,
            &client,
            invalid,
            Ok(Some(("bad.png".into(), vec![1, 2, 3]))),
        );
        assert!(
            w.get_channel_settings_error()
                .contains("Couldn't open channel icon")
        );
        assert_eq!(w.get_channel_settings_name_draft().as_str(), "Unsaved name");
        ICON_STATE.with(|s| assert_eq!(s.borrow().staged.as_ref().unwrap().1, bytes));

        let dismissed = begin_icon_request(&w, &client).unwrap();
        crate::bin_ui::features::back::close(&w, "channel-settings");
        show_channel_settings_shell(&w, &id.to_string(), "private");
        w.set_channel_settings_can_edit(true);
        finish_icon_request(&w, &client, dismissed, picked());
        assert!(!w.get_channel_settings_has_icon_preview());

        // Even direct navigation that bypasses the shell changes stack identity.
        let reopened = begin_icon_request(&w, &client).unwrap();
        crate::bin_ui::features::back::close(&w, "channel-settings");
        crate::bin_ui::features::back::push(&w, "channel-settings");
        finish_icon_request(&w, &client, reopened, picked());
        assert!(!w.get_channel_settings_has_icon_preview());
        let switched = begin_icon_request(&w, &client).unwrap();
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(ChannelId::new()));
        finish_icon_request(&w, &client, switched, picked());
        assert!(!w.get_channel_settings_has_icon_preview());
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(id));
        let read_error = begin_icon_request(&w, &client).unwrap();
        finish_icon_request(&w, &client, read_error, Err("fixture read failed".into()));
        assert!(
            w.get_channel_settings_error()
                .contains("fixture read failed")
        );
        let old_session = begin_icon_request(&w, &client).unwrap();
        client.clear_composer_session();
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(id));
        finish_icon_request(&w, &client, old_session, picked());
        assert!(!w.get_channel_settings_has_icon_preview());
        eprintln!("{FIXTURE_COMPLETED}");
    }

    #[test]
    fn delete_result_refreshes_rows_reports_errors_and_preserves_a_new_selection() {
        let test_name = format!(
            "{}::delete_result_refreshes_rows_reports_errors_and_preserves_a_new_selection",
            module_path!().split_once("::").unwrap().1
        );
        if std::env::var(CHILD_MARKER).ok().as_deref() != Some(test_name.as_str()) {
            run_isolated_delete_test(&test_name);
            return;
        }
        unsafe {
            std::env::set_var("CHATTER_NO_KEYRING", "1");
            std::env::set_var("CHATTER_DB_PASSPHRASE", "channel-delete-fixture");
            std::env::set_var("CHATTER_HEADLESS", "1");
        }
        i_slint_backend_testing::init_integration_test_with_system_time();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let fixture = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .data_dir(fixture.path().to_string_lossy().into_owned())
            .build()
            .unwrap();
        let path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
            .unwrap()
            .database_path();
        let db = Arc::new(
            chatter_core::storage::Database::open(&path, "channel-delete-fixture").unwrap(),
        );
        let me = UserId::new();
        SettingsRepository::new(db.clone())
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        let repo = ChannelRepository::new(db);
        let deleted = ChannelId::new();
        let other = ChannelId::new();
        for (id, name) in [(deleted, "Delete fixture"), (other, "Keep fixture")] {
            repo.insert(&chatter_core::domain::channel::Channel::new(
                id,
                name.into(),
                ChannelType::Private,
                me,
            ))
            .unwrap();
            repo.mark_former(&id, false).unwrap();
        }
        let w = MainWindow::new().unwrap();
        w.set_current_view("main".into());
        w.set_channel_settings_id(deleted.to_string().into());
        crate::bin_ui::features::back::push(&w, "channel-settings");
        w.set_channel_name("Delete fixture".into());
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(deleted));
        w.set_chat_rows(ModelRc::new(VecModel::from(
            client
                .sidebar_rows()
                .into_iter()
                .map(|r| r.to_slint())
                .collect::<Vec<_>>(),
        )));
        assert_eq!(w.get_chat_rows().row_count(), 2);
        let pump = || {
            for _ in 0..4 {
                slint::Timer::single_shot(std::time::Duration::from_millis(25), || {
                    slint::quit_event_loop().unwrap();
                });
                w.run().unwrap();
            }
        };
        finish_channel_delete(
            w.as_weak(),
            client.clone(),
            deleted,
            Err(ChannelError::NotAuthorized),
        );
        pump();
        assert_eq!(w.get_chat_rows().row_count(), 2);
        assert_eq!(
            client.active_conversation(),
            ActiveConversation::Channel(deleted)
        );
        assert_eq!(w.get_active_modal().as_str(), "channel-settings");
        assert!(
            w.get_channel_settings_error()
                .contains("Couldn't delete channel")
        );
        assert!(w.get_toast_visible());

        // Simulate the successful awaited service's local row removal. This
        // exercises the production result handler, without a server or device.
        repo.remove_confirmed_deleted_channel(&deleted).unwrap();
        finish_channel_delete(w.as_weak(), client.clone(), deleted, Ok(()));
        pump();
        assert_eq!(w.get_chat_rows().row_count(), 1);
        assert_eq!(
            w.get_chat_rows()
                .row_data(0)
                .unwrap()
                .conversation_id
                .as_str(),
            other.to_string()
        );
        assert_eq!(client.active_conversation(), ActiveConversation::None);
        assert!(w.get_channel_name().is_empty());
        assert_eq!(w.get_messages().row_count(), 0);
        assert_ne!(w.get_active_modal().as_str(), "channel-settings");
        assert!(repo.get(&other).unwrap().is_some());

        // A late result for the deleted channel must preserve a newly opened chat.
        client
            .chat_state()
            .set_active_conversation(ActiveConversation::Channel(other));
        w.set_channel_name("Keep fixture".into());
        w.set_channel_settings_id(other.to_string().into());
        crate::bin_ui::features::back::push(&w, "channel-settings");
        finish_channel_delete(w.as_weak(), client.clone(), deleted, Ok(()));
        pump();
        assert_eq!(
            client.active_conversation(),
            ActiveConversation::Channel(other)
        );
        assert_eq!(w.get_channel_name().as_str(), "Keep fixture");
        assert_eq!(w.get_active_modal().as_str(), "channel-settings");
        eprintln!("{FIXTURE_COMPLETED}");
    }
}
