//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

pub mod channel;
pub mod dm;
mod navigation;
pub mod private_group;

use std::sync::Arc;

use chatter_linux::app::UIContext;

use crate::bin_ui::audio_state;
use crate::MainWindow;

// Re-export public functions
pub use channel::load_channel_into_ui;
pub use private_group::add_private_group_to_sidebar;

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
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
}

/// Apply pinned message state to the UI.
///
/// Shared helper used by channel, DM, and private group sidebar handlers
/// to update the pinned message bar after loading pinned messages.
pub(crate) fn apply_pinned_ui(
    weak: &slint::Weak<MainWindow>,
    pinned_state: &chatter_linux::state::PinnedState,
    channel_id: chatter_linux::domain::ids::ChannelId,
    message_ids: Vec<chatter_linux::domain::ids::MessageId>,
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
