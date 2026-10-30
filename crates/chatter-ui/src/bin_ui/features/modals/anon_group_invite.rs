//! Anonymous group invite presentation. Core owns the pending invitations.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::{FriendRequestData, MainWindow};
use chatter_core::client::ChatterClient;
use chatter_core::client::events::AnonGroupInviteEvent;

static INVITE_REFRESH_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Refresh the requests model and badge from durable core storage.
pub fn refresh_group_invites(weak: &Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let generation = INVITE_REFRESH_GENERATION.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let invites = match ui_ctx.list_pending_anon_group_invites() {
                Ok(invites) => invites,
                Err(error) => {
                    tracing::error!(%error, "Failed to load pending anonymous group invites");
                    return None;
                }
            };
            let count = invites.len() as i32;
            let items: Vec<(String, String, String)> = invites
                .into_iter()
                .map(|invite| {
                    let inviter_name = ui_ctx.contact_name(&invite.inviter_id)
                        .filter(|name| !name.is_empty())
                        .or(invite.inviter_display_name)
                        .unwrap_or_else(|| {
                            chatter_core::services::user::friend_display_name_placeholder(&invite.inviter_id)
                        });
                    (invite.group_id.to_string(), invite.group_name, inviter_name)
                })
                .collect();
            Some((count, items))
        }).await;
        let (count, items) = match result {
            Ok(Some(items)) => items,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(%error, "Anonymous group invite refresh worker failed");
                return;
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            if INVITE_REFRESH_GENERATION.load(Ordering::Relaxed) != generation {
                return;
            }
            if let Some(w) = weak.upgrade() {
                w.global::<crate::ModalHostState>()
                    .set_pending_group_invites(count);
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
                w.global::<crate::ModalHostState>()
                    .set_friends_group_invites(ModelRc::new(VecModel::from(items)));
            }
        });
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
        window
            .global::<crate::ModalHostState>()
            .on_friends_accept_group_invite(move |group_id_str: SharedString| {
                let Ok(group_id) = group_id_str.parse() else {
                    return;
                };
                if let Some(w) = weak.upgrade() {
                    if !w
                        .global::<crate::ModalHostState>()
                        .get_group_invite_decision()
                        .is_empty()
                    {
                        return;
                    }
                    w.global::<crate::ModalHostState>()
                        .set_group_invite_decision(format!("join:{group_id_str}").into());
                    w.global::<crate::ModalHostState>()
                        .set_group_invite_error_id("".into());
                    w.global::<crate::ModalHostState>()
                        .set_group_invite_error("".into());
                }
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                tokio::spawn(async move {
                    match ui_ctx.accept_pending_anon_group_invite(group_id).await {
                        Ok(_) => {
                            refresh_group_invites(&weak, &ui_ctx);
                            crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.global::<crate::ModalHostState>()
                                        .set_group_invite_decision("".into());
                                    if w.global::<crate::SidebarPane>().get_active_tab()
                                        == "contacts"
                                    {
                                        w.global::<crate::SidebarPane>()
                                            .set_active_tab("chats".into());
                                        w.global::<crate::SidebarPane>().set_mobile_show_chat(true);
                                        w.global::<crate::SidebarPane>()
                                            .invoke_private_group_selected(
                                                group_id.to_string().into(),
                                            );
                                    }
                                }
                            });
                        }
                        Err(error) => {
                            tracing::error!(%error, "Failed to accept group invitation");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.global::<crate::ModalHostState>()
                                        .set_group_invite_decision("".into());
                                    w.global::<crate::ModalHostState>()
                                        .set_group_invite_error_id(group_id_str);
                                    w.global::<crate::ModalHostState>()
                                        .set_group_invite_error("Could not join the group. Try again.".into());
                                }
                            });
                        }
                    }
                });
            });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::ModalHostState>()
            .on_friends_decline_group_invite(move |group_id_str: SharedString| {
                let Ok(group_id) = group_id_str.parse() else {
                    return;
                };
                if let Some(w) = weak.upgrade() {
                    if !w
                        .global::<crate::ModalHostState>()
                        .get_group_invite_decision()
                        .is_empty()
                    {
                        return;
                    }
                    w.global::<crate::ModalHostState>()
                        .set_group_invite_decision(format!("decline:{group_id_str}").into());
                    w.global::<crate::ModalHostState>()
                        .set_group_invite_error_id("".into());
                    w.global::<crate::ModalHostState>()
                        .set_group_invite_error("".into());
                }
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                tokio::spawn(async move {
                    let result = ui_ctx.decline_pending_anon_group_invite(&group_id).await;
                    if result.is_ok() {
                        refresh_group_invites(&weak, &ui_ctx);
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.global::<crate::ModalHostState>()
                                .set_group_invite_decision("".into());
                            if let Err(error) = result {
                                tracing::error!(%error, "Failed to decline group invitation");
                                w.global::<crate::ModalHostState>()
                                    .set_group_invite_error_id(group_id_str);
                                w.global::<crate::ModalHostState>()
                                    .set_group_invite_error("Could not decline the invitation. Try again.".into());
                            }
                        }
                    });
                });
            });
    }
}
