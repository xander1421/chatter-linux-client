//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

mod anon_mode;
pub mod channel;
pub mod dm;
mod navigation;
pub mod private_group;
mod sections;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::bin_ui::audio_state;
use crate::{MainWindow, run_ui};

// Re-export public functions
pub use channel::load_channel_into_ui;
pub use private_group::add_private_group_to_sidebar;

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
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
    sections::wire_section_callbacks(window, ui_ctx);
    sections::load_sections(window, ui_ctx);
    anon_mode::wire(window, ui_ctx);
}

/// Recompute `is_selected` on every sidebar model item from the typed
/// `ActiveConversation` held by the client. Call after any selection change
/// so highlighting stays in sync without stringly-typed window properties.
///
/// Match logic is delegated to `ActiveConversation::matches_*` in `chatter-core`;
/// this function only iterates Slint models and writes the flag.
pub(crate) fn refresh_selection(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use slint::Model;

    let active = ui_ctx.active_conversation();

    // Channels (regular server channels only — DMs live in a separate model).
    {
        let model = window.get_channels();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                item.is_selected = item
                    .channel_id
                    .parse()
                    .map(|id| active.matches_channel(&id))
                    .unwrap_or(false);
                item
            })
            .collect();
        window.set_channels(slint::ModelRc::new(slint::VecModel::from(rows)));
    }

    // DM conversations (regular + anon, disambiguated via is_anon).
    {
        let model = window.get_dm_conversations();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                item.is_selected = if item.is_anon {
                    item.channel_id
                        .parse()
                        .map(|id| active.matches_anon_dm(&id))
                        .unwrap_or(false)
                } else {
                    match (item.channel_id.parse(), item.user_id.parse()) {
                        (Ok(cid), Ok(pid)) => active.matches_dm(&cid, &pid),
                        _ => false,
                    }
                };
                item
            })
            .collect();
        window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(rows)));
    }

    // Private (anonymous) groups.
    {
        let model = window.get_private_groups();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                item.is_selected = item
                    .group_id
                    .parse()
                    .map(|id| active.matches_anon_group(&id))
                    .unwrap_or(false);
                item
            })
            .collect();
        window.set_private_groups(slint::ModelRc::new(slint::VecModel::from(rows)));
    }

    // Custom-section conversations mirror the above across all kinds.
    {
        let model = window.get_section_conversations();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                let kind = item.conversation_type.as_str();
                let conv_id = item.conversation_id.as_str();
                item.is_selected = match kind {
                    "channel" => conv_id
                        .parse()
                        .map(|id| active.matches_channel(&id))
                        .unwrap_or(false),
                    "dm" => {
                        // Section rows may carry anon_dm_id, channel_id, or peer_id
                        // in `conversation_id`. Match against all shapes.
                        let anon = conv_id.parse().map(|id| active.matches_anon_dm(&id)).unwrap_or(false);
                        let peer = conv_id.parse().map(|p| active.matches_dm_peer(&p)).unwrap_or(false);
                        let chan = match (conv_id.parse(), conv_id.parse()) {
                            (Ok(cid), Ok(pid)) => active.matches_dm(&cid, &pid),
                            _ => false,
                        };
                        anon || peer || chan
                    }
                    "private_group" => conv_id
                        .parse()
                        .map(|id| active.matches_anon_group(&id))
                        .unwrap_or(false),
                    _ => false,
                };
                item
            })
            .collect();
        window.set_section_conversations(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
}

/// Apply pinned message state to the UI.
///
/// Shared helper used by channel, DM, and private group sidebar handlers
/// to update the pinned message bar after loading pinned messages.
pub(crate) fn apply_pinned_ui(
    weak: &slint::Weak<MainWindow>,
    chat_state: &chatter_core::state::ChatState,
    channel_id: chatter_core::domain::ids::ChannelId,
    message_ids: Vec<chatter_core::domain::ids::MessageId>,
    preview: String,
) {
    if message_ids.is_empty() {
        chat_state.clear_pinned();
        run_ui!(weak, |w| {
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);
        });
    } else {
        let total_count = message_ids.len() as i32;
        chat_state.set_pinned_messages(channel_id, message_ids);
        let should_show = chat_state.pinned_should_show();
        run_ui!(weak, |w| {
            w.set_has_pinned_messages(should_show);
            w.set_pinned_message_preview(preview.into());
            w.set_pinned_current_index(1);
            w.set_pinned_total_count(total_count);
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
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
    increment_unread: bool,
) {
    use chatter_core::state::ActiveConversation;
    use slint::Model;

    // Determine if the conversation being updated is the one currently viewed
    // by comparing against the typed `ActiveConversation` in chat_state.
    // Avoids stringly-typed window-prop comparisons.
    let is_currently_viewed = if increment_unread {
        conversation == ui_ctx.chat_state().active_conversation()
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
                if should_increment {
                    crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);
                }
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
                if should_increment {
                    crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);
                }
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
                if should_increment {
                    crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);
                }
            }
        }
        ActiveConversation::AnonDm(anon_dm_id) => {
            let channel_id_str = anon_dm_id.to_string();
            let model = window.get_dm_conversations();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.is_anon && item.channel_id.as_str() == channel_id_str {
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
                if should_increment {
                    crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);
                }
            }
        }
        ActiveConversation::None => {
            // None is a no-op
        }
    }
}

/// Check if a DM conversation for a peer exists (in the local channel DB).
///
/// Authoritative source — does not depend on whether the Slint sidebar model
/// has been populated yet.
pub(crate) fn sidebar_has_dm(
    ui_ctx: &Arc<ChatterClient>,
    peer_id: &chatter_core::domain::UserId,
) -> bool {
    ui_ctx.get_dm_channel_for_peer(peer_id).is_some()
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

    // Anon Mode suppresses server-DM sidebar entries entirely. The DM is
    // still persisted to local storage by core; it just doesn't surface in
    // the sidebar until the user disables anon mode and a refresh runs.
    if ui_ctx.anon_mode_enabled() {
        return;
    }

    let peer_id_str = peer_id.to_string();

    match ui_ctx.get_dm_channel_for_peer(peer_id) {
        Some(channel) => {
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
            crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);

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
