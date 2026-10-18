//! Channel settings moderation.

use super::*;

/// The conversation the settings modal is showing: a private group when it
/// was opened for one (`type == "private-group"`), otherwise a channel.
pub(super) fn settings_conversation(
    w: &MainWindow,
    ui_ctx: &ChatterClient,
) -> Option<ActiveConversation> {
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

pub(super) fn wire_moderation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
pub(super) fn wire_transfer_ownership(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
pub(super) fn wire_group_blocking(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
