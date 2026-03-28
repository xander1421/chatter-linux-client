//! Anonymous group invite event handler.
//!
//! Handles DomainEvent::AnonGroupInviteReceived and shows accept/decline dialog
//! in the FriendsModal "Groups" requests sub-tab.

use std::sync::{Arc, Mutex, OnceLock};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::UserId;
use chatter_core::inbox::DomainEvent;
use crate::{FriendRequestData, MainWindow};

/// All data needed to accept a group invite later.
#[derive(Clone)]
struct PendingGroupInvite {
    group_id: AnonGroupId,
    group_name: String,
    inviter_id: UserId,
    inviter_mailbox_pk: [u8; 32],
    profile_key: Option<[u8; 32]>,
    inviter_display_name: Option<String>,
    inviter_avatar: Option<Vec<u8>>,
}

static PENDING_INVITES: OnceLock<Mutex<Vec<PendingGroupInvite>>> = OnceLock::new();

fn pending_invites() -> &'static Mutex<Vec<PendingGroupInvite>> {
    PENDING_INVITES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Send-safe intermediate type for group invite display data.
struct GroupInviteSendData {
    group_id: String,
    group_name: String,
    inviter_name: String,
}

/// Push the current pending invite list into the Slint model.
fn refresh_group_invites_ui(weak: &Weak<MainWindow>) {
    let invites = pending_invites().lock().unwrap();
    let send_items: Vec<GroupInviteSendData> = invites
        .iter()
        .map(|inv| {
            let inviter_name = inv
                .inviter_display_name
                .clone()
                .unwrap_or_else(|| format!("User {}", &inv.inviter_id.to_string()[..8]));
            GroupInviteSendData {
                group_id: inv.group_id.to_string(),
                group_name: inv.group_name.clone(),
                inviter_name,
            }
        })
        .collect();
    drop(invites); // release lock before UI call

    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let items: Vec<FriendRequestData> = send_items
                .into_iter()
                .map(|d| FriendRequestData {
                    user_id: d.group_id.into(),
                    display_name: d.group_name.into(),
                    username: d.inviter_name.into(),
                    avatar_url: SharedString::default(),
                    avatar_source: slint::Image::default(),
                    has_avatar: false,
                    sent_at: SharedString::default(),
                })
                .collect();
            w.set_friends_group_invites(ModelRc::new(VecModel::from(items)));
        }
    });
}

/// Spawn anon group invite handler to process incoming invite events.
///
/// Subscribes to `DomainEvent::AnonGroupInviteReceived` from the event bus
/// and adds invites to the pending list for user approval.
pub fn spawn_anon_group_invite_handler(
    weak_window: Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let user_service = ui_ctx.users().clone();
    let mut event_rx = ui_ctx.event_bus().subscribe_domain();

    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                DomainEvent::AnonGroupInviteReceived {
                    group_id,
                    group_name,
                    inviter_id,
                    inviter_mailbox_pk,
                    profile_key,
                    inviter_display_name,
                    inviter_avatar,
                } => {
                    let inviter_name = inviter_display_name.clone()
                        .unwrap_or_else(|| user_service.get_display_name(&inviter_id));
                    tracing::info!(
                        group_id = %group_id,
                        group_name = %group_name,
                        inviter_name = %inviter_name,
                        "Received anon group invite - adding to pending list"
                    );

                    // Store invite for later accept/decline
                    {
                        let mut invites = pending_invites().lock().unwrap();
                        // Deduplicate by group_id
                        if !invites.iter().any(|i| i.group_id == group_id) {
                            invites.push(PendingGroupInvite {
                                group_id,
                                group_name,
                                inviter_id,
                                inviter_mailbox_pk,
                                profile_key,
                                inviter_display_name,
                                inviter_avatar,
                            });
                        }
                    }

                    refresh_group_invites_ui(&weak_window);
                }
                _ => {}
            }
        }
    })
}

/// Wire accept/decline callbacks for group invites.
pub fn wire_group_invite_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Accept group invite
    {
        let weak = window.as_weak();
        let anon_group_service = ui_ctx.anon_groups().clone();

        window.on_friends_accept_group_invite(move |group_id_str: SharedString| {
            let weak = weak.clone();
            let service = anon_group_service.clone();
            let gid_str = group_id_str.to_string();

            tokio::spawn(async move {
                // Pull invite data from pending store
                let invite = {
                    let invites = pending_invites().lock().unwrap();
                    invites.iter().find(|i| i.group_id.to_string() == gid_str).cloned()
                };

                let Some(invite) = invite else {
                    tracing::warn!(group_id = %gid_str, "Group invite not found in pending list");
                    return;
                };

                match service.accept_invite(
                    invite.group_id,
                    invite.group_name.clone(),
                    invite.inviter_id, // owner_id
                    invite.inviter_id,
                    invite.inviter_mailbox_pk,
                    invite.profile_key,
                    invite.inviter_display_name,
                    invite.inviter_avatar,
                ).await {
                    Ok(_group) => {
                        tracing::info!(group_id = %gid_str, "Accepted anon group invite");

                        // Remove from pending
                        {
                            let mut invites = pending_invites().lock().unwrap();
                            invites.retain(|i| i.group_id.to_string() != gid_str);
                        }
                        refresh_group_invites_ui(&weak);

                        // Add to sidebar
                        let gid = gid_str.clone();
                        let gname = invite.group_name.clone();
                        let weak_sidebar = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_sidebar.upgrade() {
                                crate::bin_ui::features::sidebar::add_private_group_to_sidebar(
                                    &w, &gid, &gname, 1,
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(
                            group_id = %gid_str,
                            error = %e,
                            "Failed to accept anon group invite"
                        );
                    }
                }
            });
        });
    }

    // Decline group invite
    {
        let weak = window.as_weak();

        window.on_friends_decline_group_invite(move |group_id_str: SharedString| {
            let gid_str = group_id_str.to_string();
            tracing::info!(group_id = %gid_str, "Declined anon group invite");

            // Remove from pending
            {
                let mut invites = pending_invites().lock().unwrap();
                invites.retain(|i| i.group_id.to_string() != gid_str);
            }
            refresh_group_invites_ui(&weak);
        });
    }
}
