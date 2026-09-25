//! Anonymous DM (private) request actions: accept, decline.
//!
//! These use the anonymous DM service which has NO server involvement.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::bin_ui::converters::{get_initials, DMListItemExt, DMListItemSendData, FriendRequestExt, FriendRequestSendData};
use crate::{DMListItemData, FriendRequestData, MainWindow};

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
                    let peer_name = dm.peer_display_name.clone()
                        .unwrap_or_else(|| chatter_core::services::user::friend_display_name_placeholder(&peer_id));
                    let dm_id = dm.id;

                    tracing::info!(
                        "Accepted private message request from {} (conversation {})",
                        peer_name,
                        dm_id
                    );

                    // Refresh private requests and add to DM sidebar
                    refresh_private_requests(weak.clone(), &ui_ctx).await;

                    // Add to DM sidebar
                    add_anon_dm_to_sidebar(weak.clone(), &dm, &peer_name);

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
            match ui_ctx.decline_anon_dm(&conv_id) {
                Ok(_) => {
                    tracing::info!("Declined private message request {}", conv_id_str);

                    // Refresh private requests list
                    refresh_private_requests(weak.clone(), &ui_ctx).await;

                    // Remove from DM sidebar (raw uuid — no "anon_dm:" prefix)
                    let channel_id = conv_id_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            remove_dm_from_sidebar(&w, &channel_id);
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
            let peer_id = ui_ctx.get_anon_dm_conversation(&conv_id).map(|dm| dm.peer_id);

            // Block the conversation
            match ui_ctx.block_anon_dm_conversation(&conv_id) {
                Ok(_) => {
                    tracing::info!("Blocked private message request {}", conv_id_str);

                    // Also block the user globally if we have their ID
                    if let Some(user_id) = peer_id {
                        if let Err(e) = ui_ctx.block_anon_dm_user(&user_id, Some("Blocked from request")) {
                            tracing::warn!("Failed to globally block user: {}", e);
                        } else {
                            tracing::info!("Globally blocked user {} from anonymous DMs", user_id);
                        }
                    }

                    // Refresh private requests list and blocked users list
                    refresh_private_requests(weak.clone(), &ui_ctx).await;
                    refresh_blocked_users(weak.clone(), &ui_ctx);

                    // Remove from DM sidebar (raw uuid — no "anon_dm:" prefix)
                    let channel_id = conv_id_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            remove_dm_from_sidebar(&w, &channel_id);
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
        let peer_name = dm.peer_display_name.clone()
            .unwrap_or_else(|| chatter_core::services::user::friend_display_name_placeholder(&dm.peer_id));
        let conv_id_str = dm.id.to_string();

        private_requests.push(FriendRequestSendData {
            user_id: conv_id_str,
            display_name: peer_name,
            username: "Private".to_string(),
            avatar_bytes: dm.peer_avatar.clone(),
            avatar_decoded: None,
            sent_at: dm.created_at
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
            let private_slint: Vec<FriendRequestData> = private_requests.into_iter().map(|r| r.to_slint()).collect();
            w.set_friends_private_requests(ModelRc::new(VecModel::from(private_slint)));
            w.set_pending_message_requests(request_count);
        }
    });
}

/// Add an accepted anonymous DM to the sidebar.
fn add_anon_dm_to_sidebar(
    weak_window: slint::Weak<MainWindow>,
    dm: &chatter_core::domain::anon_dm::AnonDm,
    peer_name: &str,
) {
    // Use Send-safe type to pass across thread boundary
    let mut dm_send_data = DMListItemSendData {
        channel_id: dm.id.to_string(),
        user_id: dm.peer_id.to_string(),
        display_name: peer_name.to_string(),
        avatar_bytes: dm.peer_avatar.clone(),
        avatar_decoded: None,
        initials: get_initials(peer_name),
        presence_status: "online".to_string(),
        // Placeholder until the first message — matches the requester side
        // (anon_dm_request.rs) so a freshly-accepted DM isn't blank.
        last_message: "Conversation started".to_string(),
        timestamp: dm.updated_at
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string(),
        unread_count: 0,
        is_typing: false,
        is_anon: true,
        is_pinned: false,
    };

    // Pre-decode avatar before entering UI thread to avoid blocking
    dm_send_data.pre_decode_avatar();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            // Convert Send-safe data to Slint type inside event loop
            let dm_item = dm_send_data.to_slint();

            // Get existing DM list and append new DM
            let current_dms = w.get_dm_conversations();
            let mut dm_vec: Vec<DMListItemData> = Vec::new();

            // Copy existing DMs (excluding any duplicate)
            let new_channel_id = dm_item.channel_id.as_str();
            for i in 0..current_dms.row_count() {
                if let Some(existing_dm) = current_dms.row_data(i) {
                    if existing_dm.channel_id.as_str() != new_channel_id {
                        dm_vec.push(existing_dm);
                    }
                }
            }

            // Add new DM
            dm_vec.push(dm_item);

            // Update UI
            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_vec)));
        }
    });
}

/// Remove a DM from the sidebar by channel_id.
fn remove_dm_from_sidebar(window: &MainWindow, channel_id: &str) {
    let current_dms = window.get_dm_conversations();
    let mut dm_vec: Vec<DMListItemData> = Vec::new();

    for i in 0..current_dms.row_count() {
        if let Some(dm) = current_dms.row_data(i) {
            if dm.channel_id.as_str() != channel_id {
                dm_vec.push(dm);
            }
        }
    }

    window.set_dm_conversations(ModelRc::new(VecModel::from(dm_vec)));
}

/// Refresh the blocked users list in the UI.
pub(crate) fn refresh_blocked_users(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let blocked_users = ui_ctx.blocked_users();
    let blocked_data: Vec<crate::BlockedUserData> = blocked_users
        .into_iter()
        .map(|user_id| crate::BlockedUserData {
            user_id: user_id.to_string().into(),
            display_name: ui_ctx.display_name(&user_id).into(),
            username: "Private".into(),
            blocked_at: String::new().into(),
        })
        .collect();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_friends_blocked_users(ModelRc::new(VecModel::from(blocked_data)));
        }
    });
}
