//! Channel invite loading + accept/decline wiring.
//!
//! Channel invites are poll-based (no real-time event stream): the list and
//! badge count are refreshed on login, on sidebar-banner click, and after each
//! accept/decline. Mirrors the "Groups" requests sub-tab, but backed by the
//! server via [`ChatterClient::list_pending_channel_invites`] /
//! [`ChatterClient::respond_to_channel_invite`].

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::ChannelId;
use crate::{FriendRequestData, MainWindow};

/// Send-safe intermediate type for channel invite display data.
struct ChannelInviteSendData {
    channel_id: String,
    channel_name: String,
    sent_at: String,
    note: String,
}

/// Format a unix-nanosecond timestamp as the same "Mon DD, HH:MM" shape used
/// by the other request rows. Empty string if the value can't be interpreted.
fn format_created_at(created_at: u64) -> String {
    let secs = (created_at / 1_000_000_000) as i64;
    let nanos = (created_at % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%b %d, %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Fetch the current user's pending channel invites and push them into the
/// Slint model + badge count. Poll-style refresh — safe to call repeatedly.
pub fn load_channel_invites(weak: Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let invites = ui_ctx.list_pending_channel_invites().await;
        let send_items: Vec<ChannelInviteSendData> = invites
            .into_iter()
            .map(|inv| {
                let channel_name = if inv.channel_name.is_empty() {
                    "Channel invite".to_string()
                } else {
                    inv.channel_name
                };
                ChannelInviteSendData {
                    channel_id: inv.channel_id,
                    channel_name,
                    sent_at: format_created_at(inv.created_at),
                    note: inv.message,
                }
            })
            .collect();

        let count = send_items.len() as i32;

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let items: Vec<FriendRequestData> = send_items
                    .into_iter()
                    .map(|d| FriendRequestData {
                        user_id: d.channel_id.into(),
                        display_name: d.channel_name.into(),
                        username: "Channel invite".into(),
                        avatar_source: slint::Image::default(),
                        has_avatar: false,
                        sent_at: d.sent_at.into(),
                        note: d.note.into(),
                    })
                    .collect();
                w.set_friends_channel_invites(ModelRc::new(VecModel::from(items)));
                w.set_pending_channel_invites(count);
            }
        });
    });
}

/// Wire accept/decline callbacks for channel invites.
///
/// Each response spawns the server call, then re-runs [`load_channel_invites`]
/// so the list and badge count reflect the new state.
pub fn wire_channel_invite_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Accept channel invite
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();

        window.on_friends_accept_channel_invite(move |channel_id_str: SharedString| {
            respond(&weak, &ui_ctx, channel_id_str, true);
        });
    }

    // Decline channel invite
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();

        window.on_friends_decline_channel_invite(move |channel_id_str: SharedString| {
            respond(&weak, &ui_ctx, channel_id_str, false);
        });
    }
}

/// Wire the sidebar-banner click handler to refresh the invite list on open.
///
/// The Slint side already opens the modal on the "channels" sub-tab; this only
/// re-polls the server so the list is current.
pub fn wire_channel_invites_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_channel_invites_clicked(move || {
        load_channel_invites(weak.clone(), &ui_ctx);
    });
}

/// Parse the channel id, respond to the invite, then refresh the list/count.
fn respond(
    weak: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id_str: SharedString,
    accept: bool,
) {
    let weak = weak.clone();
    let ui_ctx = ui_ctx.clone();
    let cid_str = channel_id_str.to_string();

    tokio::spawn(async move {
        let cid: ChannelId = match cid_str.parse() {
            Ok(cid) => cid,
            Err(e) => {
                tracing::warn!(channel_id = %cid_str, error = %e, "Invalid channel id on invite response");
                return;
            }
        };

        match ui_ctx.respond_to_channel_invite(&cid, accept).await {
            Ok(()) => {
                tracing::info!(channel_id = %cid_str, accept, "Responded to channel invite");
            }
            Err(e) => {
                tracing::error!(channel_id = %cid_str, error = %e, "Failed to respond to channel invite");
            }
        }

        // Refresh list + badge regardless of outcome so a declined/errored
        // invite doesn't linger in the UI when the server already dropped it.
        load_channel_invites(weak, &ui_ctx);
    });
}
