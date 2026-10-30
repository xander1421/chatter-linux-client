//! Video player callbacks.
//!
//! Handles video playback controls.

use slint::ComponentHandle;

use crate::MainWindow;
use crate::bin_ui::video_state;

use super::timers::stop_video_progress_timer;

/// Wire video player callbacks.
pub fn wire_video_player_callbacks(window: &MainWindow) {
    // Close video player
    let weak_window = window.as_weak();
    window
        .global::<crate::ChatPane>()
        .on_close_video_player(move || {
            super::media_viewer::invalidate();
            video_state::stop_video();
            stop_video_progress_timer();
            if let Some(w) = weak_window.upgrade() {
                w.global::<crate::ChatPane>()
                    .set_video_player_visible(false);
            }
            tracing::debug!("Video player closed");
        });

    // Toggle play/pause
    let weak_window = window.as_weak();
    window
        .global::<crate::ChatPane>()
        .on_video_player_toggle_play_pause(move || {
            video_state::toggle_play_pause();
            if let Some((_, _, is_playing)) = video_state::get_playback_state() {
                if let Some(w) = weak_window.upgrade() {
                    w.global::<crate::ChatPane>()
                        .set_video_player_is_playing(is_playing);
                }
            }
        });

    // Seek
    window
        .global::<crate::ChatPane>()
        .on_video_player_seek(|position| {
            video_state::seek(position);
        });

    // Set volume
    window
        .global::<crate::ChatPane>()
        .on_video_player_set_volume(|volume| {
            video_state::set_volume(volume);
        });
}
