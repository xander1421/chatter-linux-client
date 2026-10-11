//! Anonymous DM (private) request actions: accept, decline.
//!
//! These use the anonymous DM service which has NO server involvement.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::bin_ui::converters::{FriendRequestExt, FriendRequestSendData};
use crate::{FriendRequestData, MainWindow};

/// Wire callback to accept an anonymous DM request.
///
/// This uses the anonymous DM service which has NO server involvement.
/// The conversation_id is passed as user-id field in the FriendRequestData struct.
pub(crate) fn wire_accept_private_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_accept_private_request(move |conversation_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let conv_id_str = conversation_id.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            // Parse conversation ID
            let conv_id: chatter_core::domain::anon_dm::AnonDmId = match conv_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid conversation ID: {}", conv_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                    return;
                }
            };

            // Accept the request
            match ui_ctx.accept_anon_dm(&conv_id).await {
                Ok(dm) => {
                    let peer_id = dm.peer_id;
                    // Use stored display name, not user_service (strangers aren't in the database)
                    let peer_name = dm.peer_display_name.clone().unwrap_or_else(|| {
                        chatter_core::services::user::friend_display_name_placeholder(&peer_id)
                    });
                    let dm_id = dm.id;

                    tracing::info!(
                        "Accepted private message request from {} (conversation {})",
                        peer_name,
                        dm_id
                    );

                    // Refresh private requests; the accepted DM's row is core's.
                    refresh_private_requests(weak.clone(), &ui_ctx).await;
                    crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                    // Open in the chat view: its Accept bar gives way to the composer.
                    crate::bin_ui::features::sidebar::dm::refresh_open_anon_dm(&weak, &ui_ctx, dm_id);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to accept private request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
            }
        });
    });
}

/// Wire callback to decline an anonymous DM request.
pub(crate) fn wire_decline_private_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_decline_private_request(move |conversation_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let conv_id_str = conversation_id.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            // Parse conversation ID
            let conv_id: chatter_core::domain::anon_dm::AnonDmId = match conv_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid conversation ID: {}", conv_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                    return;
                }
            };

            // Decline the request
            match ui_ctx.decline_anon_dm(&conv_id).await {
                Ok(_) => {
                    tracing::info!("Declined private message request {}", conv_id_str);

                    // Refresh private requests list
                    refresh_private_requests(weak.clone(), &ui_ctx).await;

                    // The request's row leaves the chat list with it, and a
                    // chat view it was declined from closes (RC108 O2).
                    crate::bin_ui::features::sidebar::conversation_removed(
                        &weak,
                        &ui_ctx,
                        chatter_core::state::ActiveConversation::AnonDm(conv_id),
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to decline private request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
            }
        });
    });
}

/// Wire callback to block an anonymous DM request.
///
/// This blocks the conversation AND the user globally.
pub(crate) fn wire_block_private_request(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_friends_block_private_request(move |conversation_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let conv_id_str = conversation_id.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            // Parse conversation ID
            let conv_id: chatter_core::domain::anon_dm::AnonDmId = match conv_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    tracing::error!("Invalid conversation ID: {}", conv_id_str);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                    return;
                }
            };

            // Get the conversation to find the peer ID for global block
            let peer_id = ui_ctx
                .get_anon_dm_conversation(&conv_id)
                .map(|dm| dm.peer_id);

            // Block the conversation
            match ui_ctx.block_anon_dm_conversation(&conv_id) {
                Ok(_) => {
                    tracing::info!("Blocked private message request {}", conv_id_str);

                    // Also block the user globally if we have their ID
                    if let Some(user_id) = peer_id {
                        if let Err(e) =
                            ui_ctx.block_anon_dm_user(&user_id, Some("Blocked from request"))
                        {
                            tracing::warn!("Failed to globally block user: {}", e);
                        } else {
                            tracing::info!("Globally blocked user {} from anonymous DMs", user_id);
                        }
                    }

                    // Refresh private requests list and blocked users list
                    refresh_private_requests(weak.clone(), &ui_ctx).await;
                    refresh_blocked_users(weak.clone(), &ui_ctx);

                    // The request's row leaves the chat list with it.
                    crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to block private request: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                        }
                    });
                }
            }
        });
    });
}

/// Refresh the private (anon DM) requests list in the UI.
async fn refresh_private_requests(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let requests = ui_ctx.list_anon_dm_message_requests();
    let mut private_requests: Vec<FriendRequestSendData> = Vec::with_capacity(requests.len());

    for dm in requests {
        let peer_name = dm.peer_display_name.clone().unwrap_or_else(|| {
            chatter_core::services::user::friend_display_name_placeholder(&dm.peer_id)
        });
        let conv_id_str = dm.id.to_string();

        private_requests.push(FriendRequestSendData {
            user_id: conv_id_str,
            display_name: peer_name,
            username: "Private".to_string(),
            avatar_bytes: dm.peer_avatar.clone(),
            avatar_decoded: None,
            sent_at: dm
                .created_at
                .with_timezone(&chrono::Local)
                .format("%b %d, %H:%M")
                .to_string(),
        });
    }

    for req in &mut private_requests {
        req.pre_decode_avatar();
    }

    let request_count = private_requests.len() as i32;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            let private_slint: Vec<FriendRequestData> =
                private_requests.into_iter().map(|r| r.to_slint()).collect();
            w.set_friends_private_requests(ModelRc::new(VecModel::from(private_slint)));
            w.set_pending_message_requests(request_count);
        }
    });
}

/// Send-safe Friends › Blocked row; `to_slint` decodes the avatar on the
/// UI thread.
pub(crate) struct BlockedUserRow {
    user_id: String,
    display_name: String,
    anon: bool,
    anon_chat: bool,
    avatar: Option<Vec<u8>>,
}

impl BlockedUserRow {
    pub(crate) fn to_slint(self) -> crate::BlockedUserData {
        crate::BlockedUserData {
            user_id: self.user_id.into(),
            display_name: self.display_name.into(),
            username: "Private".into(),
            blocked_at: String::new().into(),
            anon: self.anon,
            anon_chat: self.anon_chat,
            avatar_source: self
                .avatar
                .as_deref()
                .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                .unwrap_or_default(),
        }
    }
}

/// Friends › Blocked rows, as core lists them (`blocked_users`).
pub(crate) fn blocked_user_rows(ui_ctx: &Arc<ChatterClient>) -> Vec<BlockedUserRow> {
    ui_ctx
        .blocked_users()
        .into_iter()
        .map(|c| BlockedUserRow {
            user_id: c.id.to_string(),
            display_name: ui_ctx.display_name(&c.id),
            anon: c.anon,
            anon_chat: c.anon_chat,
            avatar: if c.anon {
                ui_ctx.anon_dm_peer_avatar(&c.id)
            } else {
                ui_ctx.user_avatar(&c.id)
            },
        })
        .collect()
}

/// Refresh the blocked users list in the UI.
pub(crate) fn refresh_blocked_users(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let blocked_data = blocked_user_rows(ui_ctx);

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            let rows: Vec<crate::BlockedUserData> = blocked_data
                .into_iter()
                .map(BlockedUserRow::to_slint)
                .collect();
            w.set_friends_blocked_users(ModelRc::new(VecModel::from(rows)));
        }
    });
}
