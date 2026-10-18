//! Channel settings invites.

use super::*;

pub(super) fn wire_invite_members(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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

        let Ok(group_id) = group_id_str.parse() else {
            return;
        };
        open_private_group_invite_modal(weak, ui_ctx, group_id);
    });
}

/// Shared header/settings entry: validate ownership and populate the picker.
pub fn open_private_group_invite_modal(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    group_id: AnonGroupId,
) {
    let Some(fence) = weak
        .upgrade()
        .and_then(|w| crate::bin_ui::features::modals::create_channel::RequestFence::capture(&w, &ui_ctx))
    else {
        return;
    };
    tokio::spawn(async move {
        let allowed = ui_ctx.session_current_user_id().is_some_and(|uid| {
            ui_ctx
                .get_anon_group(&group_id)
                .is_some_and(|g| g.owner_id == uid && g.my_role.can_invite())
        });
        let candidates = if allowed {
            ui_ctx
                .list_anon_group_invite_candidates(Some(&group_id))
                .into_iter()
                .map(|c| {
                    let avatar = crate::bin_ui::converters::invite_candidate_avatar(&ui_ctx, &c);
                    (c, avatar)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            if !fence.owns_ui(&w, &ui_ctx) {
                return;
            }
            if !allowed {
                w.set_toast_message("Only the group owner can invite members".into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
                return;
            }
            let rows: Vec<PrivateGroupFriend> = candidates
                .into_iter()
                .map(|(c, avatar)| crate::bin_ui::converters::invite_candidate_to_slint(c, avatar))
                .collect();
            w.set_invite_private_group_id(group_id.to_string().into());
            w.set_invite_private_group_friends(ModelRc::new(VecModel::from(rows)));
            w.set_invite_private_group_selected_count(0);
            crate::bin_ui::features::back::push(&w, "invite-private-group");
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

pub(super) fn wire_invite_channel_members(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
            let selected_vec = ui_ctx.eligible_channel_invite_targets(users.iter()
                .map(|u| (u.id.to_string(), u.selected, u.already_member, u.banned))
                .collect::<Vec<_>>());
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
            for user_id in &selected_user_ids {
                match ui_ctx.invite_user_to_channel(&channel_id, user_id, None).await {
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
pub(super) fn wire_invite_member_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
