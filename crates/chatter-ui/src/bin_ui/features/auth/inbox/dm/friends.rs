//! Friend event handlers.
//!
//! Handles friend acceptance events for DM channel setup.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;

use crate::bin_ui::converters::get_initials;
use crate::{DMListItemData, MainWindow};

/// Handle FriendAccepted event by adding the DM channel to the sidebar.
/// This is called on the SENDER's side when their friend request is accepted.
///
/// DM bootstrap happens on-demand when the user sends their first message,
/// avoiding race conditions where both sides try to initiate simultaneously.
pub async fn handle_friend_accepted(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    friend_user_id: String,
    channel_id: String,
) {
    // Parse the channel ID
    let channel_uuid = match channel_id.parse::<chatter_core::domain::ids::ChannelId>() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, channel_id = %channel_id, "Invalid channel_id in FriendAccepted event");
            return;
        }
    };

    // Parse the friend user ID
    let friend_uuid = match friend_user_id.parse::<chatter_core::domain::UserId>() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, friend_user_id = %friend_user_id, "Invalid friend_user_id in FriendAccepted event");
            return;
        }
    };

    // Fetch channel from server via ChannelService (handles storage + memberships)
    let channel = match ui_ctx.channels().get_channel_with_memberships(&channel_uuid).await {
        Ok(ch) => ch,
        Err(e) => {
            tracing::error!(error = %e, channel_id = %channel_id, "Failed to fetch channel via service");
            return;
        }
    };

    // Also save via DmBootstrapService for DM-specific handling
    if let Err(e) = ui_ctx.dm_bootstrap().save_dm_channel(&channel) {
        tracing::error!(error = %e, "Failed to save DM channel via bootstrap service");
        // Continue anyway to update UI
    }

    // Get friend's display name via UserService (handles fetch + storage)
    let friend_name = match ui_ctx.users().get_user_profile(&friend_uuid).await {
        Ok(profile) => {
            if profile.display_name.is_empty() {
                friend_user_id.clone()
            } else {
                profile.display_name.clone()
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch friend profile via service, using ID");
            friend_user_id.clone()
        }
    };

    let initials = get_initials(&friend_name);

    // Add DM to sidebar UI
    let weak = weak_window.clone();
    let channel_id_clone = channel_id.clone();
    let friend_user_id_clone = friend_user_id.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            let current_dms = window.get_dm_conversations();
            let mut dms: Vec<DMListItemData> = (0..current_dms.row_count())
                .filter_map(|i| current_dms.row_data(i))
                .collect();

            // Check if DM already exists
            let exists = dms.iter().any(|dm| dm.channel_id.as_str() == channel_id_clone);
            if !exists {
                // Create DMListItemData inside event loop (slint::Image is not Send)
                let dm_item = DMListItemData {
                    channel_id: channel_id.clone().into(),
                    user_id: friend_user_id.clone().into(),
                    display_name: friend_name.into(),
                    initials: initials.into(),
                    avatar_url: "".into(),
                    avatar_source: slint::Image::default(),
                    has_avatar: false,
                    presence_status: "offline".into(),
                    last_message: "".into(),
                    timestamp: "".into(),
                    unread_count: 0,
                    is_typing: false,
                };
                dms.push(dm_item);
                window.set_dm_conversations(ModelRc::new(VecModel::from(dms)));
                tracing::info!(
                    channel_id = %channel_id_clone,
                    friend_user_id = %friend_user_id_clone,
                    "Added DM to sidebar for accepted friend"
                );
            }
        }
    });
}
