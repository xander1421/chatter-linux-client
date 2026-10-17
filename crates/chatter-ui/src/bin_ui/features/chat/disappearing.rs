//! Disappearing messages: live status below the chat header, and independent
//! message lifetime / activation controls on the contact profile.
//!
//! Everything shown comes from `chatter_core::services::disappearing` and the
//! client facade (`disappearing_timer`, `set_disappearing_timer`); this module
//! only moves values into `DisappearingGlobal` and the picker's choice back.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use chatter_core::services::disappearing;
use chatter_core::state::ActiveConversation;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{DisappearingGlobal, DisappearingOption, MainWindow};

/// Wire the picker callback and keep the header's timer current.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window
        .global::<DisappearingGlobal>()
        .set_options(ModelRc::new(VecModel::from(options())));
    wire_set_dm_window(window, ui_ctx);
    wire_secret_timer(window, ui_ctx);
    wire_refresh(window, ui_ctx);
    wire_expired(window, ui_ctx);
}

fn wire_secret_timer(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ctx = ui_ctx.clone();
    window.on_secret_timer_set(move |seconds| {
        let Ok(seconds) = u32::try_from(seconds) else {
            return;
        };
        let ActiveConversation::AnonDm(id) = ctx.chat_state().active_conversation() else {
            return;
        };
        let (ctx, weak) = (ctx.clone(), weak.clone());
        tokio::spawn(async move {
            let result = ctx
                .set_disappearing_timer(ActiveConversation::AnonDm(id), seconds)
                .await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                refresh_header(&w, &ctx);
                if let Err(e) = result {
                    tracing::warn!(error = %e, "secret-chat timer change failed");
                    w.set_toast_message("Could not announce timer change".into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                }
            });
        });
    });
}

/// Drop expired bubbles and quote snapshots, then redraw rows from core's
/// stored preview, timestamp, and unread count for every conversation kind.
fn wire_expired(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut expired = ui_ctx.subscribe_expired_messages();
    let weak = window.as_weak();
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let (gone, reply_targets, attachments): (
                std::collections::HashSet<String>,
                std::collections::HashSet<(String, String)>,
                std::collections::HashSet<String>,
            ) = match expired.recv().await {
                Ok(batch) => (
                    batch
                        .messages
                        .iter()
                        .flat_map(|m| {
                            [
                                m.id.to_string(),
                                chatter_core::domain::ids::MessageId::from_timestamp(m.sent_at)
                                    .to_string(),
                            ]
                        })
                        .collect(),
                    batch
                        .messages
                        .iter()
                        .filter_map(|m| {
                            m.sender_id
                                .map(|author| (author.to_string(), m.sent_at.to_string()))
                        })
                        .collect(),
                    batch
                        .removed_attachment_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                ),
                Err(RecvError::Lagged(_)) => {
                    // We no longer know which attachment ids expired.
                    crate::bin_ui::decode_cache::wipe();
                    crate::bin_ui::features::sidebar::history_changed(&weak, &ctx);
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            crate::bin_ui::decode_cache::evict_attachments(&attachments);
            crate::bin_ui::features::sidebar::history_changed(&weak, &ctx);
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                super::remove_messages_where(&w, |m| gone.contains(m.message_id.as_str()));
                if let Some(results) = w
                    .get_unified_search_messages()
                    .as_any()
                    .downcast_ref::<VecModel<crate::SearchResult>>()
                {
                    for index in (0..results.row_count()).rev() {
                        if results
                            .row_data(index)
                            .is_some_and(|hit| gone.contains(hit.message_id.as_str()))
                        {
                            results.remove(index);
                        }
                    }
                }
                if reply_targets.contains(&(
                    w.get_composer_reply_to_sender().to_string(),
                    w.get_composer_reply_to_timestamp().to_string(),
                )) {
                    super::send_message::clear_reply(&w);
                }
                let model = w.get_messages();
                for index in 0..model.row_count() {
                    let Some(mut row) = model.row_data(index) else {
                        continue;
                    };
                    if !gone.contains(row.reply_to_message_id.as_str()) {
                        continue;
                    }
                    row.reply_to_preview = "".into();
                    row.reply_to_not_found = true;
                    row.reply_to_category = "".into();
                    row.reply_to_attachment_id = "".into();
                    row.reply_to_has_thumbnail = false;
                    row.reply_to_thumbnail = slint::Image::default();
                    model.set_row_data(index, row);
                }
            });
        }
    });
}

/// The picker's choices, in core's order (Off, 30 seconds … 4 weeks).
pub fn options() -> Vec<DisappearingOption> {
    disappearing::TIMER_PRESETS
        .iter()
        .map(|&seconds| DisappearingOption {
            seconds: seconds as i32,
            label: disappearing::timer_label(seconds).into(),
        })
        .collect()
}

/// Fill the DM-settings row for the contact screen of `peer_id`. Friends
/// only — core announces a timer over the friend DM and has none with anyone
/// else. Called from `open_user_profile` on the UI thread.
pub fn show_for_profile(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    peer_id: &UserId,
    is_friend: bool,
) {
    let dm = ActiveConversation::ServerDm { peer_id: *peer_id };
    let can_set = is_friend && ui_ctx.can_set_disappearing_timer(dm.clone());
    let seconds = if can_set {
        ui_ctx.disappearing_timer(dm)
    } else {
        0
    };
    let g = w.global::<DisappearingGlobal>();
    g.set_dm_can_set(can_set);
    g.set_dm_seconds(seconds as i32);
    g.set_dm_label(disappearing::timer_label(seconds).into());
    let state = ui_ctx.disappearing_state(ActiveConversation::ServerDm { peer_id: *peer_id });
    g.set_dm_activation_label(
        disappearing::timer_label(if state.activation_seconds == 0 {
            disappearing::DEFAULT_ACTIVATION_SECONDS
        } else {
            state.activation_seconds
        })
        .into(),
    );
}

/// Hide the DM-settings row (the contact screen is showing ourselves).
pub fn hide_for_profile(w: &MainWindow) {
    w.global::<DisappearingGlobal>().set_dm_can_set(false);
}

/// The open conversation's timer onto the header.
fn refresh_header(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let state = ui_ctx.disappearing_state(ui_ctx.chat_state().active_conversation());
    let now = chatter_core::util::time::TimestampNanos::now().as_u64();
    let seconds = state.effective_seconds(now);
    let g = w.global::<DisappearingGlobal>();
    g.set_header_label(disappearing::timer_short_label(seconds).into());
    g.set_active_label(disappearing::timer_label(seconds).into());
    g.set_status_label(state.status_label(now).into());
}

/// Re-read everything that shows a timer: the header, and the contact screen
/// when it is showing `peer` (a change can arrive from the friend while it is
/// open).
fn refresh_all(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    refresh_header(w, ui_ctx);
    if let Ok(peer) = w.get_profile_user_id().parse::<UserId>() {
        if w.global::<DisappearingGlobal>().get_dm_can_set() {
            show_for_profile(w, ui_ctx, &peer, true);
        }
    }
}

fn wire_set_dm_window(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ctx = ui_ctx.clone();
    window.global::<DisappearingGlobal>().on_set_dm_window(move |peer, seconds, minutes| {
        let Ok(peer_id) = peer.parse::<UserId>() else { return };
        let Ok(seconds) = u32::try_from(seconds) else { return };
        let Some(activation_seconds) = (if seconds == 0 { Some(0) } else { disappearing::activation_minutes(&minutes) }) else {
            if let Some(w) = weak.upgrade() {
                w.set_toast_message("Enter a whole number of minutes from 1 to 525600.".into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
            }
            return;
        };
        let (ctx, weak) = (ctx.clone(), weak.clone());
        tokio::spawn(async move {
            let result = ctx.set_disappearing_window(ActiveConversation::ServerDm { peer_id }, seconds, activation_seconds).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                refresh_all(&w, &ctx);
                if let Err(e) = result {
                    tracing::warn!(%peer_id, error = %e, "disappearing: activation change not announced");
                    w.set_toast_message("Timer changed here. Your friend receives it with your next message.".into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                }
            });
        });
    });
}

/// Refresh on every conversation switch and on every timer change (ours from
/// another device, or the friend's).
fn wire_refresh(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    refresh_header(window, ui_ctx);

    let key = |d: &chatter_core::state::ChatStateData| d.active_conversation.clone();
    let mut current = key(&ui_ctx.chat_state().get());
    let mut switches = ui_ctx.chat_state().subscribe();
    let mut changes = ui_ctx.subscribe_disappearing_timer_changes();
    let weak = window.as_weak();
    let ctx = ui_ctx.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {},
                snap = switches.recv() => match snap {
                    Ok(snap) => {
                        let entering = key(&snap);
                        if entering == current {
                            continue;
                        }
                        current = entering;
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                },
                changed = changes.recv() => match changed {
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                },
            }
            let (weak, ctx) = (weak.clone(), ctx.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    refresh_all(&w, &ctx);
                }
            });
        }
    });
}
