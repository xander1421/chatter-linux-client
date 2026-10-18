//! Channel settings actions.

use super::*;
use slint::ComponentHandle;

pub(super) fn wire_channel_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_archive = ui_ctx.clone();

    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_archive(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_archive.clone();
            let currently_archived = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::ModalHostState>()
                        .get_channel_settings_archived()
                })
                .unwrap_or(false);
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
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_leave(move || {
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
    window
        .global::<crate::SidebarPane>()
        .on_leave_conversation(move |row_id, kind| {
            let kind = crate::bin_ui::converters::conversation_type(kind);
            let Some(conv) = ui_ctx_leave.resolve_row_conversation(row_id.as_str(), &kind) else {
                return;
            };
            leave_conversation(weak_window.clone(), ui_ctx_leave.clone(), conv);
        });

    let weak_window = window.as_weak();
    let ui_ctx_delete = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_channel_settings_delete(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_delete.clone();
            // Read on the UI thread: a Slint Weak never upgrades inside tokio::spawn.
            let Some(w) = weak.upgrade() else { return };
            let channel_id = w
                .global::<crate::ModalHostState>()
                .get_channel_settings_id()
                .to_string();
            let settings_epoch = SETTINGS_EPOCH.with(|epoch| epoch.get());
            let session_revision = ui_ctx.auth_snapshot().session_revision;
            tokio::spawn(async move {
                if channel_id.is_empty() {
                    return;
                }
                let channel_uuid = match channel_id.parse::<uuid::Uuid>() {
                    Ok(id) => ChannelId::from(id),
                    Err(_) => return,
                };
                let result = ui_ctx.delete_channel(&channel_uuid).await;
                finish_channel_delete(
                    weak,
                    ui_ctx,
                    channel_uuid,
                    session_revision,
                    settings_epoch,
                    result,
                );
            });
        });
}

/// Apply the awaited delete result on the same path used by Settings.
pub(super) fn finish_channel_delete(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
    session_revision: u64,
    settings_epoch: u64,
    result: Result<(), chatter_core::services::channel::ChannelError>,
) {
    if !ui_ctx.composer_session_is_current(session_revision) {
        return;
    }
    if result.is_ok() {
        crate::bin_ui::features::sidebar::conversation_removed(
            &weak,
            &ui_ctx,
            ActiveConversation::Channel(channel_id),
        );
    }
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        if !ui_ctx.composer_session_is_current(session_revision) {
            return;
        }
        match result {
            Ok(()) => {
                // A late completion must not close a different settings screen.
                if w.global::<crate::ModalHostState>()
                    .get_channel_settings_id()
                    == channel_id.to_string().as_str()
                    && SETTINGS_EPOCH.with(|epoch| epoch.get() == settings_epoch)
                {
                    crate::bin_ui::features::back::close(&w, "channel-settings");
                }
                crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
            }
            Err(e) => {
                tracing::warn!(%channel_id, error = %e, "Channel deletion failed");
                let message = format!("Couldn't delete channel: {}", e.user_message());
                if w.global::<crate::ModalHostState>()
                    .get_channel_settings_id()
                    == channel_id.to_string().as_str()
                    && SETTINGS_EPOCH.with(|epoch| epoch.get() == settings_epoch)
                {
                    w.global::<crate::ModalHostState>()
                        .set_channel_settings_error(message.clone().into());
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
                    if w.global::<crate::ModalHostState>()
                        .get_channel_settings_id()
                        == id.as_str()
                    {
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
