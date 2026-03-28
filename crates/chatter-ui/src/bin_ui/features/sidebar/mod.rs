//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

pub mod channel;
pub mod dm;
mod navigation;
pub mod private_group;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::bin_ui::audio_state;
use crate::MainWindow;

// Re-export public functions
pub use channel::load_channel_into_ui;
pub use private_group::add_private_group_to_sidebar;

/// Wire sidebar callbacks (channel/DM selection, settings, profile, org switcher).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    channel::wire_channel_selected(window, ui_ctx);
    dm::wire_dm_selected(window, ui_ctx);
    private_group::wire_private_group_selected(window, ui_ctx);
    navigation::wire_saved_messages(window, ui_ctx);
    navigation::wire_settings_clicked(window, ui_ctx);
    navigation::wire_profile_clicked(window, ui_ctx);
    navigation::wire_create_join_channel(window);
    navigation::wire_toggle_sidebar(window, ui_ctx);
    navigation::load_sidebar_collapsed_state(window, ui_ctx);
    navigation::wire_message_requests_clicked(window, ui_ctx);
    navigation::load_pending_message_requests_count(window, ui_ctx);
    navigation::wire_group_invites_clicked(window, ui_ctx);
    navigation::load_pending_group_invites_count(window, ui_ctx);
    navigation::wire_org_selected(window, ui_ctx);
    navigation::wire_manage_org_clicked(window, ui_ctx);
    navigation::wire_org_management_callbacks(window, ui_ctx);
    navigation::load_org_state(window, ui_ctx);
    navigation::subscribe_org_state(window, ui_ctx);
}

/// Apply pinned message state to the UI.
///
/// Shared helper used by channel, DM, and private group sidebar handlers
/// to update the pinned message bar after loading pinned messages.
pub(crate) fn apply_pinned_ui(
    weak: &slint::Weak<MainWindow>,
    pinned_state: &chatter_core::state::PinnedState,
    channel_id: chatter_core::domain::ids::ChannelId,
    message_ids: Vec<chatter_core::domain::ids::MessageId>,
    preview: String,
) {
    if message_ids.is_empty() {
        pinned_state.clear();
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_has_pinned_messages(false);
                w.set_pinned_message_preview("".into());
                w.set_pinned_total_count(0);
            }
        });
    } else {
        let total_count = message_ids.len() as i32;
        pinned_state.set_pinned_messages(channel_id, message_ids);
        let should_show = pinned_state.should_show();
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_has_pinned_messages(should_show);
                w.set_pinned_message_preview(preview.into());
                w.set_pinned_current_index(1);
                w.set_pinned_total_count(total_count);
            }
        });
    }
}

/// Update a sidebar item's preview text and timestamp, moving it to the top (MRU).
///
/// Works for all conversation types: DM, Channel, and Private Group.
/// The item is identified by `ActiveConversation` and moved to position 0
/// so the most recently active conversation is always at the top.
///
/// If `increment_unread` is true, bumps the unread count by 1 unless this
/// conversation is currently being viewed (used for received messages).
/// Pass `false` for sent messages.
pub(crate) fn update_sidebar_preview(
    window: &MainWindow,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
    increment_unread: bool,
) {
    use chatter_core::state::ActiveConversation;
    use slint::Model;

    // Determine if the conversation being updated is the one currently viewed.
    // If so, don't increment unread — the user is looking at it.
    let is_currently_viewed = if increment_unread {
        let selected_dm = window.get_selected_dm_id().to_string();
        let selected_channel = window.get_selected_channel_id().to_string();
        let selected_group = window.get_selected_private_group_id().to_string();

        match conversation {
            ActiveConversation::MailboxDm { peer_id, channel_id, .. } | ActiveConversation::ServerDm { peer_id, channel_id, .. } => {
                selected_dm == channel_id.to_string() || selected_dm == peer_id.to_string()
            }
            ActiveConversation::Channel(id) => selected_channel == id.to_string(),
            ActiveConversation::AnonGroup(id) => selected_group == id.to_string(),
            _ => false,
        }
    } else {
        true // sent messages: never increment
    };
    let should_increment = increment_unread && !is_currently_viewed;

    match conversation {
        ActiveConversation::MailboxDm { peer_id, .. } | ActiveConversation::ServerDm { peer_id, .. } => {
            let peer_id_str = peer_id.to_string();
            let model = window.get_dm_conversations();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.user_id.as_str() == peer_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if should_increment { u.unread_count += 1; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::Channel(channel_id) => {
            let channel_id_str = channel_id.to_string();
            let model = window.get_channels();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.channel_id.as_str() == channel_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if should_increment { u.unread_count += 1; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_channels(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::AnonGroup(group_id) => {
            let group_id_str = group_id.to_string();
            let model = window.get_private_groups();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.group_id.as_str() == group_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if should_increment { u.unread_count += 1; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_private_groups(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::AnonDm(_) | ActiveConversation::None => {
            // AnonDm has no sidebar list; None is a no-op
        }
    }
}

/// Check if a DM conversation for a peer exists in the sidebar.
pub(crate) fn sidebar_has_dm(window: &MainWindow, peer_id: &chatter_core::domain::UserId) -> bool {
    use slint::Model;
    let peer_id_str = peer_id.to_string();
    let model = window.get_dm_conversations();
    (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .any(|item| item.user_id.as_str() == peer_id_str)
}

/// Create a new DM sidebar entry for a peer not yet in the list.
///
/// Used when receiving the first message from a new peer. The entry is
/// inserted at position 0 (MRU) with unread_count = 1.
pub(crate) fn create_dm_sidebar_entry(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    peer_id: &chatter_core::domain::UserId,
    sender_name: &str,
    preview: &str,
    timestamp: &str,
) {
    use slint::Model;
    let peer_id_str = peer_id.to_string();

    match ui_ctx.channels().get_dm_channel_for_peer(peer_id) {
        Ok(Some(channel)) => {
            let mut new_dm = crate::bin_ui::converters::dm_to_slint(&channel, sender_name, &peer_id_str);
            new_dm.last_message = preview.into();
            new_dm.timestamp = timestamp.into();
            new_dm.unread_count = 1;

            let model = window.get_dm_conversations();
            let mut items: Vec<_> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .collect();
            items.insert(0, new_dm);
            window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(items)));

            tracing::info!(
                peer_id = %peer_id_str,
                channel_id = %channel.id,
                "Added missing DM conversation to sidebar"
            );
        }
        _ => {
            tracing::warn!(
                peer_id = %peer_id_str,
                sender_name = %sender_name,
                "DM conversation not in sidebar and channel not found"
            );
        }
    }
}

/// Stop any currently playing audio and clear state.
pub(crate) fn stop_audio_playback() {
    // Stop the audio player
    if let Ok(mut guard) = audio_state::get_audio_player().lock() {
        if let Some(ref mut player) = *guard {
            player.stop();
        }
    }
    // Clear the audio state
    audio_state::clear_state();
}
