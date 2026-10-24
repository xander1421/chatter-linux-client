//! Channel settings modal callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::{ChatterClient, InviteCandidate};
use chatter_core::domain::ChannelType;
use chatter_core::domain::channel::NotificationPreference;
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
    static SAVE_REQUEST: std::cell::RefCell<Option<IconRequest>> = const { std::cell::RefCell::new(None) };
    static SETTINGS_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn icon_draft_current(w: &MainWindow, ctx: &ChatterClient, request: &IconRequest) -> bool {
    w.get_active_modal() == "channel-settings"
        && w.global::<crate::ModalHostState>()
            .get_channel_settings_can_edit()
        && w.global::<crate::ModalHostState>()
            .get_channel_settings_id()
            .as_str()
            == request.settings_id
        && ctx.active_conversation() == request.conversation
        && ctx.composer_session_is_current(request.session_revision)
}

fn icon_request_current(w: &MainWindow, ctx: &ChatterClient, request: &IconRequest) -> bool {
    icon_draft_current(w, ctx, request)
        && w.global::<crate::ModalHostState>().get_modal_stack() == request.modal_stack
}

fn begin_icon_request(w: &MainWindow, ctx: &ChatterClient) -> Option<u64> {
    if w.get_active_modal() != "channel-settings"
        || !w
            .global::<crate::ModalHostState>()
            .get_channel_settings_can_edit()
    {
        return None;
    }
    if !matches!(ctx.active_conversation(), ActiveConversation::Channel(id)
        if id.to_string() == w.global::<crate::ModalHostState>().get_channel_settings_id().as_str())
    {
        return None;
    }
    Some(ICON_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.pending = Some(IconRequest {
            generation,
            modal_stack: w.global::<crate::ModalHostState>().get_modal_stack(),
            conversation: ctx.active_conversation(),
            session_revision: ctx.auth_snapshot().session_revision,
            settings_id: w
                .global::<crate::ModalHostState>()
                .get_channel_settings_id()
                .to_string(),
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
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_icon_path(path.to_string_lossy().as_ref().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_icon_preview(image);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_has_icon_preview(true);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_error("".into());
                        state.staged = Some((request, data));
                    }
                    Err(error) => w
                        .global::<crate::ModalHostState>()
                        .set_channel_settings_error(
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
            Err(error) => w
                .global::<crate::ModalHostState>()
                .set_channel_settings_error(format!("Couldn't open channel icon: {error}").into()),
        }
    });
}

/// Wire channel settings modal callbacks.
pub fn wire_channel_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_clear_error(move || {
            if let Some(w) = weak_window.upgrade() {
                w.global::<crate::ModalHostState>()
                    .set_channel_settings_error("".into());
            }
        });
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
    w.global::<crate::ModalHostState>()
        .get_modal_stack()
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
    w.global::<crate::ModalHostState>()
        .set_channel_settings_id(id.into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_type(kind.into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_name("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_description("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_name_draft("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_description_draft("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_saving(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_created_at("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_archived(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_ai_enabled(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_muted(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_notification_pref("all".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_icon_path("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_has_icon_preview(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_edit(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_manage_members(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_invite(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_archive(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_delete(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_leave(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_is_owner(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_member_count(0);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_invited_count(0);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_members(ModelRc::default());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_view_bans(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_banned_members(ModelRc::default());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_loading(true);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_error("".into());
    crate::bin_ui::features::chat::membership::reset_access(w);
    crate::bin_ui::features::back::push(w, "channel-settings");
}

/// Put `message` on the settings screen. The error banner lives on that
/// screen, so a failure that only sets the property is invisible.
pub(crate) fn present_settings_error(w: &MainWindow, id: &str, kind: &str, message: &str) {
    if w.global::<crate::ModalHostState>()
        .get_channel_settings_id()
        .as_str()
        != id
        || !settings_on_stack(w)
    {
        show_channel_settings_shell(w, id, kind);
    }
    w.global::<crate::ModalHostState>()
        .set_channel_settings_loading(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_error(message.into());
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
    notification_preference: NotificationPreference,
    can_edit: bool,
    can_manage_members: bool,
    can_invite: bool,
    can_archive: bool,
    can_delete: bool,
    is_owner: bool,
    can_leave: bool,
    member_count: i32,
}

pub(crate) fn preference_label(preference: NotificationPreference) -> &'static str {
    match preference {
        NotificationPreference::All => "all",
        NotificationPreference::Mentions => "mentions",
        NotificationPreference::None => "none",
    }
}

fn present_local_channel_settings(w: &MainWindow, settings: &LocalChannelSettings) {
    w.global::<crate::ModalHostState>()
        .set_channel_settings_id(settings.id.as_str().into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_name(settings.name.as_str().into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_description(settings.description.as_str().into());
    // Reset the editable drafts to the stored values so reopening a
    // different channel doesn't carry the previous edits.
    w.global::<crate::ModalHostState>()
        .set_channel_settings_name_draft(settings.name.as_str().into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_description_draft(settings.description.as_str().into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_saving(false);
    // Clear any icon picked in a previous open.
    w.global::<crate::ModalHostState>()
        .set_channel_settings_icon_path("".into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_has_icon_preview(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_ai_enabled(settings.ai_enabled);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_muted(settings.muted);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_notification_pref(preference_label(settings.notification_preference).into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_type(settings.channel_type.into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_archived(settings.archived);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_created_at(settings.created_at.as_str().into());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_edit(settings.can_edit);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_manage_members(settings.can_manage_members);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_invite(settings.can_invite);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_archive(settings.can_archive);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_delete(settings.can_delete);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_is_owner(settings.is_owner);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_leave(settings.can_leave);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_member_count(settings.member_count);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_invited_count(0);
    // Drop the previous channel's rows. The roster RPC fills this pane
    // after the screen is already up.
    w.global::<crate::ModalHostState>()
        .set_channel_settings_members(ModelRc::default());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_view_bans(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_banned_members(ModelRc::default());
    w.global::<crate::ModalHostState>()
        .set_channel_settings_loading(true);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_error("".into());
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
        let current_id = w
            .global::<crate::ModalHostState>()
            .get_channel_settings_id();
        if !settings_refresh_allowed(
            ui_ctx.active_conversation(),
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
    if settings_open_allowed(ui_ctx.active_conversation(), opened) {
        if let Some(w) = weak_window.upgrade() {
            let already = w
                .global::<crate::ModalHostState>()
                .get_channel_settings_id()
                .as_str()
                == channel_id_str
                && settings_on_stack(&w);
            if !already {
                // Type is filled from the local row. Empty hides the access
                // pane until that row says public or private.
                show_channel_settings_shell(&w, &channel_id_str, "");
            }
        }
    }

    tokio::spawn(async move {
        if !settings_open_allowed(ui_ctx.active_conversation(), opened) {
            return;
        }

        let channel = match ui_ctx.get_channel_local(&channel_id) {
            Some(c) => c,
            None => {
                paint_if_current(weak_window, ui_ctx, opened, channel_id_str, |w| {
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_loading(false);
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_error("Channel not found".into());
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_can_edit(false);
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_can_manage_members(false);
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_can_archive(false);
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_can_delete(false);
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_can_leave(false);
                });
                return;
            }
        };

        let current_user_id = ui_ctx.session_current_user_id();
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
            notification_preference: ui_ctx
                .conversation_notification_preference(ActiveConversation::Channel(channel_id))
                .unwrap_or_default(),
            can_edit,
            can_manage_members,
            can_invite: ui_ctx.can_invite_to_channel(&channel_id),
            can_archive,
            can_delete,
            is_owner,
            can_leave,
            member_count: i32::try_from(channel.member_count).unwrap_or(i32::MAX),
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

        // Core shapes the rows; the images are decoded on the UI thread below
        // (a slint::Image is not Send). This await must stay after the local
        // present above: the roster RPC is what used to hide the screen.
        let lists = ui_ctx.channel_member_lists(&channel_id).await;
        let members = lists.members;
        let bans = lists.bans;
        let complete = lists.complete;
        let member_error = lists.failure_stage.map(|stage| match stage {
            chatter_core::client::channels::ChannelMemberFailure::Members(error) => {
                format!("Couldn't load members: {error}")
            }
            chatter_core::client::channels::ChannelMemberFailure::MembersPartial(error) => format!(
                "Couldn't load all members ({} shown): {error}",
                members.len()
            ),
            chatter_core::client::channels::ChannelMemberFailure::MembersLimit => {
                "Only the first 10000 members are shown.".to_string()
            }
            chatter_core::client::channels::ChannelMemberFailure::Bans(error) => {
                format!("Couldn't load banned members: {error}")
            }
        });
        paint_if_current(weak_window, ui_ctx, opened, channel_id_str, move |w| {
            if let Some(message) = member_error {
                if !members.is_empty() {
                    apply_member_lists(w, &members, bans.as_deref(), complete);
                }
                w.global::<crate::ModalHostState>()
                    .set_channel_settings_loading(false);
                w.global::<crate::ModalHostState>()
                    .set_channel_settings_error(message.into());
                return;
            }
            apply_member_lists(w, &members, bans.as_deref(), complete);
            w.global::<crate::ModalHostState>()
                .set_channel_settings_loading(false);
            w.global::<crate::ModalHostState>()
                .set_channel_settings_error("".into());
        });
    });
}

/// UI thread only (decodes avatars into `slint::Image`s). A partial roster
/// leaves the member count from channel settings rather than calling the
/// rows that loaded the total.
fn apply_member_lists(
    w: &MainWindow,
    members: &[chatter_core::domain::channel::MemberWithProfile],
    bans: Option<&[chatter_core::domain::channel::BannedMember]>,
    complete: bool,
) {
    let rows: Vec<ChannelMemberData> = members
        .iter()
        .map(crate::bin_ui::converters::member_to_slint)
        .collect();
    if complete {
        w.global::<crate::ModalHostState>()
            .set_channel_settings_member_count(rows.len() as i32);
    }
    w.global::<crate::ModalHostState>()
        .set_channel_settings_members(ModelRc::new(VecModel::from(rows)));
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_view_bans(bans.is_some());
    let banned: Vec<BannedMemberData> = bans
        .unwrap_or_default()
        .iter()
        .map(crate::bin_ui::converters::banned_member_to_slint)
        .collect();
    w.global::<crate::ModalHostState>()
        .set_channel_settings_banned_members(ModelRc::new(VecModel::from(banned)));
}

/// Re-fetch the members pane after a moderation action, leaving the rest of
/// the modal (unsaved name/description drafts) alone.
fn refresh_member_lists(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
) {
    tokio::spawn(async move {
        let lists = ui_ctx.channel_member_lists(&channel_id).await;
        let members = lists.members;
        let bans = lists.bans;
        let complete = lists.complete;
        let error = lists.failure_stage.map(|stage| match stage {
            chatter_core::client::channels::ChannelMemberFailure::Members(error) => {
                format!("Couldn't load members: {error}")
            }
            chatter_core::client::channels::ChannelMemberFailure::MembersPartial(error) => format!(
                "Couldn't load all members ({} shown): {error}",
                members.len()
            ),
            chatter_core::client::channels::ChannelMemberFailure::MembersLimit => {
                "Only the first 10000 members are shown.".to_string()
            }
            chatter_core::client::channels::ChannelMemberFailure::Bans(error) => {
                format!("Couldn't load banned members: {error}")
            }
        });
        paint_if_current(
            weak,
            ui_ctx,
            ActiveConversation::Channel(channel_id),
            channel_id.to_string(),
            move |w| {
                if !members.is_empty() || error.is_none() {
                    apply_member_lists(w, &members, bans.as_deref(), complete);
                }
                if let Some(error) = error {
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_error(error.into());
                } else {
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_error("".into());
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
        let open = w
            .global::<crate::ModalHostState>()
            .get_channel_settings_id()
            == id.as_str()
            && w.global::<crate::ModalHostState>()
                .get_modal_stack()
                .iter()
                .any(|s| s == "channel-settings");
        if !open {
            return;
        }
        if w.global::<crate::ModalHostState>()
            .get_channel_settings_type()
            != "private-group"
        {
            refresh_member_lists(weak.clone(), ui_ctx, channel_id);
            return;
        }
        let Ok(group_id) = id.parse::<AnonGroupId>() else {
            return;
        };
        let weak = weak.clone();
        std::thread::spawn(move || {
            let Some(roster) = ui_ctx.private_group_settings_roster(group_id) else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                let current_id = w
                    .global::<crate::ModalHostState>()
                    .get_channel_settings_id();
                if settings_refresh_allowed(
                    ui_ctx.active_conversation(),
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

/// UI thread only (decodes avatars into `slint::Image`s). Fills the members
/// pane and the General pane's count; leaves the name field (a draft) alone.
pub fn apply_private_group_roster(
    w: &MainWindow,
    roster: chatter_core::client::PrivateGroupRoster,
) {
    // A group has no server-side edit/archive/delete; Leave is core's call
    // (not the owner while others remain). Set every time: these properties
    // are shared with channel settings and would otherwise keep a channel's
    // values, offering server RPCs keyed by the group id.
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_edit(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_archive(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_delete(false);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_leave(roster.can_leave);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_group_blocked(roster.blocked);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_member_count(roster.member_count);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_invited_count(roster.invited_count);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_is_owner(roster.is_owner);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_manage_members(roster.can_manage_members);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_can_invite(roster.can_invite);
    let rows: Vec<ChannelMemberData> = roster
        .members
        .into_iter()
        .map(|member| ChannelMemberData {
            user_id: member.user_id.into(),
            display_name: member.display_name.into(),
            role: member.role.into(),
            avatar_source: member
                .avatar
                .as_deref()
                .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                .unwrap_or_default(),
            can_make_owner: member.can_make_owner,
            invited: member.invited,
            can_block: member.can_block,
            blocked: member.blocked,
            username: "".into(),
            joined_at: "".into(),
            // Anon groups have no mute/ban RPCs.
            muted: false,
            can_ban: false,
            can_mute: false,
            // Anon-group membership is owner-authoritative,
            // not the signed channel log.
            unverified: false,
        })
        .collect();
    w.global::<crate::ModalHostState>()
        .set_channel_settings_members(ModelRc::new(VecModel::from(rows)));
}

fn begin_settings_save(w: &MainWindow, ctx: &ChatterClient) -> Option<u64> {
    let opened = ctx.active_conversation();
    if !matches!(opened, ActiveConversation::Channel(id)
        if id.to_string() == w.global::<crate::ModalHostState>().get_channel_settings_id().as_str())
        || w.get_active_modal() != "channel-settings"
        || !w
            .global::<crate::ModalHostState>()
            .get_channel_settings_can_edit()
    {
        return None;
    }
    // Freeze exact modal identity and session; competing Saves retire old ones.
    let epoch = SETTINGS_EPOCH.with(|epoch| {
        epoch.set(epoch.get().wrapping_add(1));
        epoch.get()
    });
    SAVE_REQUEST.with(|request| {
        *request.borrow_mut() = Some(IconRequest {
            generation: epoch,
            modal_stack: w.global::<crate::ModalHostState>().get_modal_stack(),
            conversation: opened,
            session_revision: ctx.auth_snapshot().session_revision,
            settings_id: w
                .global::<crate::ModalHostState>()
                .get_channel_settings_id()
                .to_string(),
        });
    });
    w.global::<crate::ModalHostState>()
        .set_channel_settings_saving(true);
    w.global::<crate::ModalHostState>()
        .set_channel_settings_error("".into());
    Some(epoch)
}

fn save_request_current(w: &MainWindow, ctx: &ChatterClient, epoch: u64) -> bool {
    SETTINGS_EPOCH.with(|current| current.get() == epoch)
        && SAVE_REQUEST.with(|request| {
            request.borrow().as_ref().is_some_and(|request| {
                request.generation == epoch && icon_request_current(w, ctx, request)
            })
        })
}

/// Production Save-success paint; the service has already persisted metadata.
/// One guarded UI turn replaces both snapshots before closing the owned modal.
fn apply_saved_channel(
    w: &MainWindow,
    ctx: &Arc<ChatterClient>,
    epoch: u64,
    metadata: crate::bin_ui::features::sidebar::channel::ChannelMetadata,
    rows: Vec<chatter_core::ui::sidebar_data::ChatRowSendData>,
) {
    if !save_request_current(w, ctx, epoch) {
        return;
    }
    metadata.apply(w);
    crate::bin_ui::features::sidebar::apply_chat_list(w, ctx, rows);
    ICON_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.pending = None;
        state.staged = None;
    });
    SAVE_REQUEST.with(|request| *request.borrow_mut() = None);
    crate::bin_ui::features::back::close(w, "channel-settings");
}

fn wire_save_settings(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_save(move |name, description| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            let name_str = name.to_string();
            let desc_str = description.to_string();

            // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
            let channel_id = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::ModalHostState>()
                        .get_channel_settings_id()
                        .to_string()
                })
                .unwrap_or_default();
            // The picked icon file (if any) so Save uploads it alongside the
            // name/description. Empty path = icon unchanged.
            let icon_path = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::ModalHostState>()
                        .get_channel_settings_icon_path()
                        .to_string()
                })
                .unwrap_or_default();
            let Some(w) = weak.upgrade() else { return };
            let opened = ui_ctx.active_conversation();
            let Some(epoch) = begin_settings_save(&w, &ui_ctx) else {
                return;
            };
            let session_revision = ui_ctx.auth_snapshot().session_revision;
            let staged_icon = weak.upgrade().and_then(|w| {
                ICON_STATE.with(|state| {
                    state.borrow().staged.as_ref().and_then(|(request, bytes)| {
                        icon_draft_current(&w, &ui_ctx, request).then(|| bytes.clone())
                    })
                })
            });
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
                            let ctx_for_error = ui_ctx.clone();
                            paint_if_current(weak, ui_ctx, opened, channel_id, move |w| {
                                if save_request_current(w, &ctx_for_error, epoch) {
                                    w.global::<crate::ModalHostState>()
                                        .set_channel_settings_saving(false);
                                    w.global::<crate::ModalHostState>()
                                        .set_channel_settings_error(
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
                let result = result.map(|channel| {
                    let metadata =
                        crate::bin_ui::features::sidebar::channel::ChannelMetadata::from_channel(
                            &channel,
                        );
                    let rows = ui_ctx.sidebar_rows();
                    (metadata, rows)
                });
                let ctx_for_paint = ui_ctx.clone();
                paint_if_current(weak, ui_ctx, opened, channel_id, move |w| {
                    if !save_request_current(w, &ctx_for_paint, epoch) {
                        return;
                    }
                    match result {
                        Ok((metadata, rows)) => {
                            apply_saved_channel(w, &ctx_for_paint, epoch, metadata, rows)
                        }
                        Err(e) => {
                            w.global::<crate::ModalHostState>()
                                .set_channel_settings_saving(false);
                            w.global::<crate::ModalHostState>()
                                .set_channel_settings_error(match &e {
                                    chatter_core::services::channel::ChannelError::InvalidInput(message) => message.clone().into(),
                                    _ => e.user_message().into(),
                                });
                        }
                    }
                });
            });
        });
}

// Reuse the native avatar/photo picker adapter, with modal-owned staging.
fn wire_pick_icon(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ctx = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_pick_icon(move || {
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
    let weak_window = window.as_weak();
    let ui_ctx_preference = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_change_notification(move |preference| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_preference.clone();
            let Some(w) = weak.upgrade() else { return };
            let Some(conv) = settings_conversation(&w, &ui_ctx) else { return };
            let selected = match preference.as_str() {
                "all" => NotificationPreference::All,
                "mentions" => NotificationPreference::Mentions,
                "none" => NotificationPreference::None,
                _ => return,
            };
            let settings_id = w.global::<crate::ModalHostState>().get_channel_settings_id().to_string();
            tokio::spawn(async move {
                let result = ui_ctx.set_conversation_notification_preference(conv, selected);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    if w.global::<crate::ModalHostState>().get_channel_settings_id() != settings_id.as_str()
                        || !settings_on_stack(&w) { return; }
                    match result {
                        Ok(()) => w.global::<crate::ModalHostState>()
                            .set_channel_settings_notification_pref(preference_label(selected).into()),
                        Err(error) => {
                            tracing::warn!(%error, "Could not save conversation notification preference");
                            w.global::<crate::ModalHostState>()
                                .set_channel_settings_error("Couldn't save notification preference".into());
                        }
                    }
                });
            });
        });

    // Per-channel AI switch. Off by default; read at ask time by
    // ChatterClient::jarvis_ask, so flipping it here actually changes whether
    // the AI will answer in this channel.
    let weak_ai = window.as_weak();
    let ui_ctx_ai = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_toggle_ai(move |enabled| {
            let Some(w) = weak_ai.upgrade() else { return };
            let channel_id = w
                .global::<crate::ModalHostState>()
                .get_channel_settings_id()
                .to_string();
            if channel_id.is_empty() {
                return;
            }
            let weak = weak_ai.clone();
            let ui_ctx = ui_ctx_ai.clone();
            tokio::spawn(async move {
                let result = ui_ctx.set_ai_enabled_for(&channel_id, enabled);
                let stored = ui_ctx.ai_enabled_for(&channel_id);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    if w.global::<crate::ModalHostState>().get_channel_settings_id() != channel_id.as_str()
                        || !settings_on_stack(&w) { return; }
                    w.global::<crate::ModalHostState>().set_channel_settings_ai_enabled(stored);
                    if let Err(error) = result {
                        tracing::warn!(%error, "Could not save conversation AI setting");
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_error("Couldn't save AI setting".into());
                    }
                });
            });
        });

    // Mute DOES persist: `mute_conversation` writes a per-channel settings key
    // (not the moderator-mute column) — the exact bug F-UI-1 documented (a
    // toggle that only flipped a Slint bool). Build the ActiveConversation from
    // the open channel-settings id, mirroring `change_member_role`.
    let weak_window = window.as_weak();
    let ui_ctx_mute = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_toggle_mute(move |muted| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_mute.clone();
            // Reflect the toggle immediately, then persist off the UI thread.
            let weak_ui = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_ui.upgrade() {
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_muted(muted);
                }
            });
            // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
            let (conv, settings_id) = match weak.upgrade() {
                Some(w) => (
                    settings_conversation(&w, &ui_ctx),
                    w.global::<crate::ModalHostState>().get_channel_settings_id().to_string(),
                ),
                None => return,
            };
            tokio::spawn(async move {
                let Some(conv) = conv else { return };
                if let Err(e) = ui_ctx.mute_conversation(conv, muted) {
                    tracing::error!(error = %e, "Failed to persist channel mute");
                    // Snap the switch back so it never shows a state we did not
                    // manage to persist (same as the AI toggle above).
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            if w.global::<crate::ModalHostState>().get_channel_settings_id() == settings_id.as_str()
                                && settings_on_stack(&w) {
                                w.global::<crate::ModalHostState>().set_channel_settings_muted(!muted);
                                w.global::<crate::ModalHostState>()
                                    .set_channel_settings_error("Couldn't save mute setting".into());
                            }
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

    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_change_member_role(move |user_id, role| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_role.clone();
            let user_id_str = user_id.to_string();
            let role_str = role.to_string();

            // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
            let channel_id = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::ModalHostState>()
                        .get_channel_settings_id()
                        .to_string()
                })
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
                let Some(new_role) = chatter_core::domain::channel::MemberRole::parse(&role_str)
                else {
                    return;
                };
                let result = ui_ctx
                    .update_channel_member_role(&channel_uuid, &user_uuid, new_role)
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    match result {
                        Ok(m) => {
                            w.global::<crate::ModalHostState>()
                                .set_channel_settings_error("".into());
                            let members = w
                                .global::<crate::ModalHostState>()
                                .get_channel_settings_members();
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
                            w.global::<crate::ModalHostState>()
                                .set_channel_settings_error(
                                    format!("Couldn't change role: {e}").into(),
                                );
                        }
                    }
                });
            });
        });

    let weak_window = window.as_weak();
    window.global::<crate::ModalHostState>().on_channel_settings_remove_member(move |user_id| {
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
                        refresh_members_if_open(weak.clone(), ui_ctx.clone(), id);
                        if let ActiveConversation::AnonGroup(group_id) = conv {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
                                    if ui_ctx.active_conversation().matches_anon_group(&group_id) {
                                        crate::bin_ui::features::sidebar::private_group::load_private_group_messages(
                                            &w, &ui_ctx, &group_id.to_string(),
                                        );
                                    }
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to remove member");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.global::<crate::ModalHostState>().set_channel_settings_error(
                                format!("Couldn't remove member: {e}").into(),
                            );
                        }
                    });
                }
            }
        });
    });
}

mod actions;
mod invites;
mod moderation;

use actions::*;
use invites::*;
pub use invites::{open_invite_member_modal, open_private_group_invite_modal};
use moderation::*;

#[cfg(test)]
mod tests;
