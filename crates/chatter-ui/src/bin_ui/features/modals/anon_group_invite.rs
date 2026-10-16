//! Anonymous group invite presentation. Core owns the pending invitations.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::{FriendRequestData, MainWindow};
use chatter_core::client::ChatterClient;
use chatter_core::client::events::AnonGroupInviteEvent;

/// Refresh the requests model and badge from durable core storage.
pub fn refresh_group_invites(weak: &Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let invites = match ui_ctx.list_pending_anon_group_invites() {
        Ok(invites) => invites,
        Err(error) => {
            tracing::error!(%error, "Failed to load pending anonymous group invites");
            return;
        }
    };
    let count = invites.len() as i32;
    let items: Vec<(String, String, String)> = invites
        .into_iter()
        .map(|invite| {
            let inviter_name = invite.inviter_display_name.unwrap_or_else(|| {
                chatter_core::services::user::friend_display_name_placeholder(&invite.inviter_id)
            });
            (invite.group_id.to_string(), invite.group_name, inviter_name)
        })
        .collect();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_pending_group_invites(count);
            let items: Vec<FriendRequestData> = items
                .into_iter()
                .map(|(id, name, inviter_name)| FriendRequestData {
                    user_id: id.into(),
                    display_name: name.into(),
                    username: inviter_name.into(),
                    avatar_source: slint::Image::default(),
                    has_avatar: false,
                    sent_at: SharedString::default(),
                    note: SharedString::default(),
                })
                .collect();
            w.set_friends_group_invites(ModelRc::new(VecModel::from(items)));
        }
    });
}

pub fn spawn_anon_group_invite_handler(
    weak_window: Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let mut event_rx = ui_ctx.subscribe_anon_group_invites();
    refresh_group_invites(&weak_window, ui_ctx);
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        loop {
            let AnonGroupInviteEvent { .. } = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Anon group invite receiver lagged");
                    refresh_group_invites(&weak_window, &ui_ctx);
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            refresh_group_invites(&weak_window, &ui_ctx);
        }
    })
}

pub fn wire_group_invite_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        window.on_friends_accept_group_invite(move |group_id_str: SharedString| {
            let Ok(group_id) = group_id_str.parse() else {
                return;
            };
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            tokio::spawn(async move {
                match ui_ctx.accept_pending_anon_group_invite(group_id).await {
                    Ok(_) => {
                        refresh_group_invites(&weak, &ui_ctx);
                        crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                    }
                    Err(error) => tracing::error!(%error, "Failed to accept group invitation"),
                }
            });
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        window.on_friends_decline_group_invite(move |group_id_str: SharedString| {
            let Ok(group_id) = group_id_str.parse() else {
                return;
            };
            if let Err(error) = ui_ctx.decline_pending_anon_group_invite(&group_id) {
                tracing::error!(%error, "Failed to decline group invitation");
            }
            refresh_group_invites(&weak, &ui_ctx);
        });
    }
}
