//! Friend request, presence, and profile update handlers.

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use crate::MainWindow;

/// Handle friend requests updated event.
pub(super) fn handle_friend_requests_updated(
    window: &Weak<MainWindow>,
    received: Vec<chatter_core::inbox::events::FriendRequestInfo>,
    sent: Vec<chatter_core::inbox::events::FriendRequestInfo>,
) {
    let weak = window.clone();
    let received_count = received.len();
    let sent_count = sent.len();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Update friend request lists
            let received_model: Vec<crate::FriendRequestData> = received
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    avatar_source: slint::Image::default(),
                    has_avatar: false,
                    sent_at: "".into(),
                })
                .collect();

            let sent_model: Vec<crate::FriendRequestData> = sent
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    avatar_source: slint::Image::default(),
                    has_avatar: false,
                    sent_at: "".into(),
                })
                .collect();

            // Update friend requests in UI
            super::super::super::initial_data::update_friend_requests(&w, received_model, sent_model);
        }
    });

    tracing::debug!(
        received = received_count,
        sent = sent_count,
        "Updated friend requests in UI"
    );
}

/// Handle friend accepted event.
pub(super) fn handle_friend_accepted(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    friend_id: UserId,
    channel_id: chatter_core::domain::ids::ChannelId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();

    // Add DM channel to sidebar, then re-merge anon DMs so they aren't lost
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh regular DM list (replaces entire list)
            super::super::super::initial_data::refresh_dm_list(&w, &ui_ctx);
            // Re-add anon DMs that were dropped by the full replace
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
            tracing::info!(
                friend_id = %friend_id,
                channel_id = %channel_id,
                "Friend accepted - refreshed DM sidebar with anon DMs preserved"
            );
        }
    });
}

/// Handle user presence changed event.
pub(super) fn handle_presence_changed(
    window: &Weak<MainWindow>,
    user_id: UserId,
    presence: chatter_core::proto::UserPresence,
    timestamp: u64,
) {
    let weak = window.clone();
    let user_id_str = user_id.to_string();
    let presence_str = match presence {
        chatter_core::proto::UserPresence::Online => "online",
        chatter_core::proto::UserPresence::Away => "away",
        chatter_core::proto::UserPresence::DoNotDisturb => "dnd",
        chatter_core::proto::UserPresence::Offline | _ => "offline",
    };

    let presence_slint: slint::SharedString = presence_str.into();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // 1. Update sidebar DM presence dots
        let dms = w.get_dm_conversations();
        for i in 0..dms.row_count() {
            if let Some(dm) = dms.row_data(i) {
                if dm.user_id == user_id_str {
                    let mut updated = dm.clone();
                    updated.presence_status = presence_slint.clone();
                    if let Some(model) = dms.as_any().downcast_ref::<slint::VecModel<crate::DMListItemData>>() {
                        model.set_row_data(i, updated);
                    }
                    break;
                }
            }
        }

        // 2. Update chat header presence indicator for active DM.
        // Read is_selected (Rust-maintained) from the DM model — avoids a
        // stringly-typed selected-id comparison.
        if w.get_is_dm() {
            for i in 0..dms.row_count() {
                if let Some(dm) = dms.row_data(i) {
                    if dm.is_selected && dm.user_id == user_id_str {
                        w.set_header_presence_status(presence_slint.clone());
                        break;
                    }
                }
            }
        }

        // 3. Update profile modal presence if showing this user
        if w.get_profile_user_id() == user_id_str {
            w.set_profile_presence(presence_slint.clone());
        }

        tracing::debug!(
            user_id = %user_id_str,
            presence = presence_str,
            timestamp,
            "Updated presence in UI"
        );
    });

    tracing::info!(
        user_id = %user_id,
        presence = ?presence,
        timestamp,
        "User presence updated"
    );
}

/// Handle anonymous DM profile updated event.
pub(super) fn handle_anon_dm_profile_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    conversation_id: chatter_core::domain::anon_dm::AnonDmId,
    user_id: UserId,
    display_name: Option<String>,
    avatar: Option<Vec<u8>>,
    avatar_url: Option<String>,
    bio: Option<String>,
    timestamp: u64,
) {
    let weak = window.clone();
    let channel_id_str = conversation_id.to_string();
    let display_name_clone = display_name.clone();

    // Update display name in sidebar immediately
    let weak_for_name = weak.clone();
    let conv_id_for_name = channel_id_str.clone();
    let name_for_ui = display_name_clone.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_for_name.upgrade() {
            let dms = w.get_dm_conversations();
            for i in 0..dms.row_count() {
                if let Some(mut dm) = dms.row_data(i) {
                    if dm.channel_id == conv_id_for_name {
                        if let Some(name) = &name_for_ui {
                            dm.display_name = name.clone().into();
                        }
                        dms.set_row_data(i, dm);
                        break;
                    }
                }
            }
        }
    });

    // Update avatar in sidebar
    if let Some(avatar_bytes) = avatar {
        // Use decrypted bytes from the event (most efficient)
        let weak_for_avatar = weak.clone();
        let channel_id_for_avatar = channel_id_str.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_for_avatar.upgrade() {
                update_dm_avatar(&w, &channel_id_for_avatar, &avatar_bytes);
            }
        });
    } else if let Some(ref url) = avatar_url {
        // Fallback to URL if no inline avatar was provided
        let ui_ctx_avatar = ui_ctx.clone();
        let weak_for_avatar = weak.clone();
        let channel_id_for_avatar = channel_id_str.clone();
        let url_for_task = url.clone();

        tokio::spawn(async move {
            let bytes = match ui_ctx_avatar.download_and_store_anon_dm_avatar(&conversation_id, &url_for_task).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("Failed to download avatar for anon DM {}: {}", conversation_id, e);
                    return;
                }
            };

            // Update avatar in sidebar UI
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_avatar.upgrade() {
                    update_dm_avatar(&w, &channel_id_for_avatar, &bytes);
                }
            });

            tracing::info!("Downloaded and stored avatar for anon DM from URL");
        });
    } else {
        // No inline avatar or URL — check if the DB already has it (fallback)
        let weak_for_avatar = weak.clone();
        let channel_id_for_avatar = channel_id_str.clone();

        if let Some(dm) = ui_ctx.get_anon_dm_conversation(&conversation_id) {
            if let Some(ref avatar_bytes) = dm.peer_avatar {
                let bytes = avatar_bytes.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_avatar.upgrade() {
                        update_dm_avatar(&w, &channel_id_for_avatar, &bytes);
                    }
                });
            }
        }
    }

    tracing::info!(
        conversation_id = %conversation_id,
        user_id = %user_id,
        ?display_name,
        ?avatar_url,
        ?bio,
        timestamp,
        "Anonymous DM profile updated"
    );
}

/// Update a DM's avatar in the sidebar. Must be called on the UI thread.
fn update_dm_avatar(w: &MainWindow, channel_id: &str, bytes: &[u8]) {
    let dms = w.get_dm_conversations();
    for i in 0..dms.row_count() {
        if let Some(mut dm) = dms.row_data(i) {
            if dm.channel_id == channel_id {
                if let Some(img) = crate::bin_ui::converters::avatar_bytes_to_image(bytes) {
                    dm.avatar_source = img;
                    dm.has_avatar = true;
                    dms.set_row_data(i, dm);
                }
                break;
            }
        }
    }
}
