//! Friend request, presence, and profile update handlers.

use std::sync::Arc;

use slint::{Model, Weak};

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::UserId;
use crate::MainWindow;

/// Handle friend requests updated event.
pub(super) fn handle_friend_requests_updated(
    window: &Weak<MainWindow>,
    received: Vec<chatter_linux::inbox::events::FriendRequestInfo>,
    sent: Vec<chatter_linux::inbox::events::FriendRequestInfo>,
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
    ui_ctx: &Arc<UIContext>,
    friend_id: UserId,
    channel_id: chatter_linux::domain::ids::ChannelId,
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
    presence: chatter_linux::proto::UserPresence,
    timestamp: u64,
) {
    let weak = window.clone();
    let user_id_str = user_id.to_string();
    let presence_str = match presence {
        chatter_linux::proto::UserPresence::Online => "online",
        chatter_linux::proto::UserPresence::Away => "away",
        chatter_linux::proto::UserPresence::DoNotDisturb => "dnd",
        chatter_linux::proto::UserPresence::Offline | _ => "offline",
    };

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(_w) = weak.upgrade() {
            // Update user presence in UI
            // This could update:
            // 1. Sidebar DM status indicator
            // 2. Chat header presence indicator
            // 3. User profile presence status

            // TODO: Implement UI updates when the Slint UI has presence indicators
            // For now, just log the update
            tracing::debug!(
                user_id = %user_id_str,
                presence = presence_str,
                timestamp,
                "Would update presence in UI (not yet implemented)"
            );
        }
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
    conversation_id: chatter_linux::domain::anon_dm::AnonDmId,
    user_id: UserId,
    display_name: Option<String>,
    avatar_url: Option<String>,
    bio: Option<String>,
    timestamp: u64,
) {
    let weak = window.clone();
    let conversation_id_str = conversation_id.to_string();
    let user_id_str = user_id.to_string();
    let display_name_clone = display_name.clone();
    let avatar_url_clone = avatar_url.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Update the DM conversation in sidebar with new profile info
            let dms = w.get_dm_conversations();
            let mut found = false;

            for i in 0..dms.row_count() {
                if let Some(mut dm) = dms.row_data(i) {
                    // Check if this is the conversation to update
                    // For DMs, the channel_id holds the conversation ID
                    if dm.channel_id == conversation_id_str {
                        // Update display name if provided
                        if let Some(name) = &display_name_clone {
                            dm.display_name = name.clone().into();
                        }

                        // Update avatar if provided
                        if let Some(avatar) = &avatar_url_clone {
                            // Load avatar image from URL
                            // For now, just log that we would update it
                            tracing::debug!(
                                "Would update avatar for {} to: {}",
                                conversation_id_str, avatar
                            );
                        }

                        // Update the model
                        dms.set_row_data(i, dm);
                        found = true;
                        break;
                    }
                }
            }

            if found {
                w.set_dm_conversations(dms);
                tracing::info!(
                    "Updated profile for anon DM {} with user {}",
                    conversation_id_str, user_id_str
                );
            }
        }
    });

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
