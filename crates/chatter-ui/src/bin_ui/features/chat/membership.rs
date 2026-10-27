//! Verified channel membership: the banner under the channel header, the
//! public badge, and Channel Settings' Access pane (join links, join
//! requests, server-operated readers).
//!
//! Every value comes from `chatter_core::client::membership`; this module
//! only moves it into `MembershipGlobal` and the user's choices back.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use chatter_core::client::membership::{
    BannerSeverity, ChannelAccessView, ChannelMembershipHeader,
};
use chatter_core::domain::ids::{ChannelId, UserId};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{
    InviteLinkRowData, JoinRequestRowData, MainWindow, MembershipGlobal, ServerOperatedRowData,
};

/// UI thread only.
pub fn apply_header(w: &MainWindow, header: &ChannelMembershipHeader) {
    let g = w.global::<MembershipGlobal>();
    match &header.banner {
        Some(b) => {
            g.set_banner_severity(match b.severity {
                BannerSeverity::Warning => "warning".into(),
                BannerSeverity::Alert => "alert".into(),
            });
            g.set_banner_title(b.title.as_str().into());
            g.set_banner_detail(b.detail.as_str().into());
            g.set_banner_can_acknowledge(b.can_acknowledge);
        }
        None => {
            g.set_banner_severity("".into());
            g.set_banner_title("".into());
            g.set_banner_detail("".into());
            g.set_banner_can_acknowledge(false);
        }
    }
    g.set_public_badge(header.public_badge.as_str().into());
}

/// UI thread only.
fn apply_access(w: &MainWindow, view: &ChannelAccessView) {
    let g = w.global::<MembershipGlobal>();
    g.set_access_public_badge(view.public_badge.as_str().into());
    g.set_access_join_summary(view.join_summary.as_str().into());
    g.set_can_manage_invites(view.can_manage_invites);
    g.set_can_approve_joins(view.can_approve_joins);
    let links: Vec<InviteLinkRowData> = view
        .invite_links
        .iter()
        .map(|l| InviteLinkRowData {
            id: l.id.as_str().into(),
            summary: l.summary.as_str().into(),
        })
        .collect();
    g.set_invite_links(ModelRc::new(VecModel::from(links)));
    let requests: Vec<JoinRequestRowData> = view
        .join_requests
        .iter()
        .map(|r| JoinRequestRowData {
            user_id: r.user_id.to_string().into(),
            display_name: r.display_name.as_str().into(),
        })
        .collect();
    g.set_join_requests(ModelRc::new(VecModel::from(requests)));
    let bots: Vec<ServerOperatedRowData> = view
        .server_operated
        .iter()
        .map(|b| ServerOperatedRowData {
            name: b.name.as_str().into(),
            label: b.label.into(),
        })
        .collect();
    g.set_server_operated(ModelRc::new(VecModel::from(bots)));
}

/// Clear the Access pane's transient state for a newly opened channel.
/// UI thread only.
pub fn reset_access(w: &MainWindow) {
    let g = w.global::<MembershipGlobal>();
    g.set_new_link("".into());
    g.set_access_status("".into());
    g.set_access_busy(false);
    apply_access(w, &ChannelAccessView::default());
}

/// Fetch the Access pane from core and show it.
pub fn load_access(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
) {
    tokio::spawn(async move {
        let view = ui_ctx.channel_access_view(&channel_id).await;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                apply_access(&w, &view);
            }
        });
    });
}

fn parse_channel(id: &str) -> Option<ChannelId> {
    id.parse::<uuid::Uuid>().ok().map(ChannelId::from)
}

fn parse_user(id: &str) -> Option<UserId> {
    id.parse::<uuid::Uuid>().ok().map(UserId::from)
}

/// Run one Access-pane action: busy while it runs, then the outcome in the
/// status line, the pane reloaded, and the open channel's banner refreshed.
fn run_action<F>(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    channel_id: ChannelId,
    done: &'static str,
    action: F,
) where
    F: std::future::Future<
            Output = Result<Option<String>, chatter_core::services::channel::ChannelError>,
        > + Send
        + 'static,
{
    let busy = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = busy.upgrade() {
            let g = w.global::<MembershipGlobal>();
            g.set_access_busy(true);
            g.set_access_status("".into());
        }
    });
    tokio::spawn(async move {
        let result = action.await;
        let view = ui_ctx.channel_access_view(&channel_id).await;
        let header = ui_ctx.channel_membership_header(&channel_id);
        let on_screen = ui_ctx.chat_snapshot().active_channel_id == Some(channel_id);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            let g = w.global::<MembershipGlobal>();
            g.set_access_busy(false);
            match result {
                Ok(link) => {
                    if let Some(link) = link {
                        g.set_new_link(link.into());
                    }
                    g.set_access_status(done.into());
                }
                Err(e) => g.set_access_status(e.user_message().into()),
            }
            apply_access(&w, &view);
            if on_screen {
                apply_header(&w, &header);
            }
        });
    });
}

/// Wire the Access pane's callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let g = window.global::<MembershipGlobal>();

    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    g.on_create_link(move |channel_id, needs_approval| {
        let Some(channel_id) = parse_channel(&channel_id) else {
            return;
        };
        let c = ctx.clone();
        run_action(
            weak.clone(),
            ctx.clone(),
            channel_id,
            "Link created. Copy it now: it cannot be shown again.",
            async move {
                c.create_channel_invite_link(&channel_id, needs_approval)
                    .await
                    .map(Some)
            },
        );
    });

    let weak = window.as_weak();
    g.on_copy_link(move |link| {
        let link = link.to_string();
        if link.is_empty() {
            return;
        }
        std::thread::spawn(move || super::actions::copy_text(&link));
        if let Some(w) = weak.upgrade() {
            w.global::<MembershipGlobal>()
                .set_access_status("Link copied".into());
        }
    });

    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    g.on_revoke_link(move |channel_id, link_id| {
        let Some(channel_id) = parse_channel(&channel_id) else {
            return;
        };
        let (c, link_id) = (ctx.clone(), link_id.to_string());
        run_action(
            weak.clone(),
            ctx.clone(),
            channel_id,
            "Link revoked",
            async move {
                c.revoke_channel_invite_link(&channel_id, &link_id)
                    .await
                    .map(|()| None)
            },
        );
    });

    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    g.on_approve_request(move |channel_id, user_id| {
        let (Some(channel_id), Some(user_id)) = (parse_channel(&channel_id), parse_user(&user_id))
        else {
            return;
        };
        let c = ctx.clone();
        run_action(
            weak.clone(),
            ctx.clone(),
            channel_id,
            "Request approved",
            async move {
                c.approve_channel_join_request(&channel_id, &user_id)
                    .await
                    .map(|()| None)
            },
        );
    });

    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    g.on_acknowledge_fork(move |channel_id| {
        let Some(channel_id) = parse_channel(&channel_id) else {
            return;
        };
        let (weak, ctx) = (weak.clone(), ctx.clone());
        tokio::spawn(async move {
            if let Err(e) = ctx.acknowledge_membership_fork(&channel_id) {
                tracing::warn!(%channel_id, error = %e, "Could not acknowledge membership fork");
            }
            let header = ctx.channel_membership_header(&channel_id);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    apply_header(&w, &header);
                }
            });
        });
    });

    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    g.on_reject_request(move |channel_id, user_id| {
        let (Some(channel_id), Some(user_id)) = (parse_channel(&channel_id), parse_user(&user_id))
        else {
            return;
        };
        let c = ctx.clone();
        run_action(
            weak.clone(),
            ctx.clone(),
            channel_id,
            "Request rejected",
            async move {
                c.reject_channel_join_request(&channel_id, &user_id)
                    .await
                    .map(|()| None)
            },
        );
    });
}
