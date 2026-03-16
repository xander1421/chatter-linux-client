//! Call timer management.
//!
//! Manages call duration and video frame polling timers.

use std::cell::RefCell;
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};

use chatter_linux::services::CallWorkerHandle;
use crate::MainWindow;

thread_local! {
    /// Call duration timer (must be on UI thread).
    static CALL_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Call start timestamp (nanoseconds since epoch) for synchronized duration.
    static CALL_START_NS: RefCell<Option<u64>> = const { RefCell::new(None) };
    /// Video frame polling timer (30fps for smooth video).
    static VIDEO_FRAME_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Get current time in nanoseconds since epoch.
pub fn current_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Start the call duration timer.
///
/// # Arguments
/// * `call_start_ns` - Optional call start timestamp in nanoseconds since epoch.
///                     If None, uses current time (caller side).
///                     If Some, uses provided timestamp (receiver side for sync).
pub fn start_call_timer(window: &MainWindow, call_start_ns: Option<u64>) {
    let weak = window.as_weak();

    // Use provided timestamp or current time
    let start_timestamp = call_start_ns.unwrap_or_else(current_time_ns);

    CALL_START_NS.with(|start| {
        *start.borrow_mut() = Some(start_timestamp);
    });

    // Update UI immediately with current elapsed time
    let now = current_time_ns();
    let initial_elapsed = if now > start_timestamp {
        ((now - start_timestamp) / 1_000_000_000) as i32
    } else {
        0
    };

    if let Some(w) = weak.upgrade() {
        let mut data = w.get_active_call_data();
        data.duration_seconds = initial_elapsed;
        w.set_active_call_data(data);
    }

    CALL_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer that updates every second
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                if let Some(w) = weak.upgrade() {
                    CALL_START_NS.with(|start| {
                        if let Some(start_timestamp) = *start.borrow() {
                            let now = current_time_ns();
                            let elapsed = if now > start_timestamp {
                                ((now - start_timestamp) / 1_000_000_000) as i32
                            } else {
                                0
                            };
                            let mut data = w.get_active_call_data();
                            data.duration_seconds = elapsed;
                            w.set_active_call_data(data);
                        }
                    });
                }
            },
        );
        *timer_cell.borrow_mut() = Some(timer);
    });

    tracing::debug!(
        start_timestamp_ns = start_timestamp,
        initial_elapsed_secs = initial_elapsed,
        "Call timer started"
    );
}

/// Stop the call duration timer.
pub fn stop_call_timer() {
    CALL_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });

    CALL_START_NS.with(|start| {
        *start.borrow_mut() = None;
    });

    tracing::debug!("Call timer stopped");
}

/// Start the video frame polling timer.
///
/// Polls SharedVideoFrames at ~30fps and updates Slint image properties.
pub fn start_video_frame_timer(window: &MainWindow, call_service: &CallWorkerHandle) {
    let weak = window.as_weak();
    let shared_frames = call_service.video_frames().clone();

    VIDEO_FRAME_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer that polls at ~30fps (33ms interval)
        let timer = Timer::default();
        let first_frame_logged = std::cell::Cell::new(false);
        timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_millis(33),
            move || {
                if let Some(w) = weak.upgrade() {
                    // Get all frames (camera + screen share) as raw data
                    let all_frames = shared_frames.get_all_frames();

                    if !all_frames.is_empty() {
                        // Log first frame received
                        if !first_frame_logged.get() {
                            first_frame_logged.set(true);
                            tracing::info!(
                                "Video frame timer: First frames received, updating UI with {} remote video frames",
                                all_frames.len()
                            );
                        }

                        // Convert to Slint images
                        let remote_images: Vec<slint::Image> = all_frames
                            .into_iter()
                            .map(|(_, frame)| crate::bin_ui::slint_conversions::video_frame_to_slint_image(&frame))
                            .collect();

                        // Update remote video frames
                        w.set_call_remote_video_frames(
                            ModelRc::new(VecModel::from(remote_images))
                        );
                    }
                }
            },
        );
        *timer_cell.borrow_mut() = Some(timer);
    });

    tracing::debug!("Video frame timer started");
}

/// Stop the video frame polling timer.
pub fn stop_video_frame_timer() {
    VIDEO_FRAME_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });

    tracing::debug!("Video frame timer stopped");
}

/// Get the current call duration in seconds.
///
/// Returns 0 if no call is active or timer hasn't started.
pub fn get_call_duration_secs() -> i64 {
    CALL_START_NS.with(|start| {
        if let Some(start_ns) = *start.borrow() {
            let now = current_time_ns();
            if now > start_ns {
                ((now - start_ns) / 1_000_000_000) as i64
            } else {
                0
            }
        } else {
            0
        }
    })
}
