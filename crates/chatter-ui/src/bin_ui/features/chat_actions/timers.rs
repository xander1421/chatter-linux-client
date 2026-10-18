//! Audio and video progress timers.
//!
//! Manages progress update timers for media playback.

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, Timer, TimerMode};

use crate::MainWindow;
use crate::bin_ui::audio_state;
use crate::bin_ui::video_state;

thread_local! {
    /// Thread-local audio progress timer (must be on UI thread).
    static AUDIO_PROGRESS_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Thread-local video progress timer (must be on UI thread).
    static VIDEO_PROGRESS_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Start the audio progress timer.
/// Must be called from the UI thread.
pub fn start_audio_progress_timer(window: &MainWindow) {
    let weak_window = window.as_weak();

    AUDIO_PROGRESS_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            if let Some(w) = weak_window.upgrade() {
                // Get current progress from audio state
                let progress = audio_state::get_progress();
                let position = audio_state::get_position();
                let elapsed = audio_state::format_duration(position);

                w.global::<crate::ChatPane>().set_audio_progress(progress);
                w.global::<crate::ChatPane>()
                    .set_audio_elapsed(elapsed.into());

                // Completion comes from the player, including unknown durations.
                if audio_state::playback_finished() {
                    // Stop timer and clear state
                    stop_audio_progress_timer();
                    audio_state::clear_state();
                    w.global::<crate::ChatPane>()
                        .set_current_playing_attachment_id("".into());
                    w.global::<crate::ChatPane>().set_is_media_playing(false);
                    w.global::<crate::ChatPane>().set_audio_progress(0.0);
                    w.global::<crate::ChatPane>()
                        .set_audio_elapsed("0:00".into());
                }
            }
        });

        *timer_cell.borrow_mut() = Some(timer);
    });
}

/// Stop the audio progress timer.
/// Must be called from the UI thread.
pub fn stop_audio_progress_timer() {
    AUDIO_PROGRESS_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
}

/// Start the video progress timer.
/// Must be called from the UI thread.
/// This timer handles both progress updates AND frame rendering.
pub fn start_video_progress_timer(window: &MainWindow) {
    let weak_window = window.as_weak();

    VIDEO_PROGRESS_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer - run at ~1ms for high refresh rate video (up to 540Hz)
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            Duration::from_micros(1850), // ~540Hz max
            move || {
                if let Some(w) = weak_window.upgrade() {
                    // Take latest frame from buffer and render it
                    if let Some(frame) = video_state::take_frame() {
                        let image = crate::bin_ui::slint_conversions::rgb_to_slint_image(
                            frame.width,
                            frame.height,
                            &frame.rgb_data,
                        );
                        w.global::<crate::ChatPane>().set_video_player_frame(image);
                    }

                    // Get current state from video player
                    if let Some((position, duration, is_playing)) =
                        video_state::get_playback_state()
                    {
                        // Clamp position to duration (some videos report timestamps beyond duration)
                        let clamped_position = position.min(duration);
                        let progress = if duration > 0.0 {
                            (clamped_position / duration).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        let elapsed = chatter_core::media::format_duration(clamped_position);

                        w.global::<crate::ChatPane>()
                            .set_video_player_progress(progress as f32);
                        w.global::<crate::ChatPane>()
                            .set_video_player_elapsed(elapsed.into());
                        w.global::<crate::ChatPane>()
                            .set_video_player_is_playing(is_playing);

                        // Check if playback finished (only when not playing and at end)
                        // Don't stop on progress alone - wait for actual EOS from decoder
                        if !is_playing && progress >= 0.99 {
                            // Video ended - stop timer but keep player visible
                            stop_video_progress_timer();
                            w.global::<crate::ChatPane>()
                                .set_video_player_is_playing(false);
                        }
                    } else {
                        // Player was stopped/closed
                        stop_video_progress_timer();
                        w.global::<crate::ChatPane>()
                            .set_video_player_visible(false);
                    }
                }
            },
        );

        *timer_cell.borrow_mut() = Some(timer);
    });
}

/// Stop the video progress timer.
/// Must be called from the UI thread.
pub fn stop_video_progress_timer() {
    VIDEO_PROGRESS_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
}
