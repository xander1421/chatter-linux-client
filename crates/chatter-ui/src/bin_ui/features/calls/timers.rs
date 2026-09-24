//! Call timer management.
//!
//! Manages call duration and video frame polling timers.

use std::cell::RefCell;
#[cfg(not(target_os = "linux"))]
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Timer, TimerMode};
#[cfg(not(target_os = "linux"))]
use slint::{Model, ModelRc, VecModel};

use chatter_core::calls::SharedVideoFrames;
use crate::MainWindow;

thread_local! {
    /// Call duration timer (must be on UI thread).
    static CALL_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Call start timestamp (nanoseconds since epoch) for synchronized duration.
    static CALL_START_NS: RefCell<Option<u64>> = const { RefCell::new(None) };
    /// Video frame keepalive timer (low-rate poll alongside the push subscriber).
    static VIDEO_FRAME_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Stop flag for the push-driven video frame subscriber task.
    static VIDEO_PUSH_STOP: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    /// Stable model backing `call_remote_video_frames` (non-Linux CPU path).
    ///
    /// We keep ONE `VecModel` for the whole call and mutate its rows in place
    /// (`set_row_data` / `push` / `remove`) instead of rebuilding a fresh
    /// `ModelRc::new(VecModel::from(..))` every frame. Rebuilding reset the
    /// model + reallocated + re-bound the Slint property on every pushed
    /// frame; at ~240 fps headroom on a multi-tile screen-share that churn
    /// was pure overhead.
    ///
    /// On Linux this is unused: the GL-texture path (`video::gl_call`) owns
    /// its own persistent model and uploads frames in place via
    /// `glTexSubImage2D` from inside the rendering notifier, avoiding the
    /// per-frame 30 MB Skia copy + GPU re-upload that `Image::from_rgba8`
    /// incurs. See `start_video_frame_timer`.
    #[cfg(not(target_os = "linux"))]
    static REMOTE_VIDEO_MODEL: RefCell<Option<Rc<VecModel<slint::Image>>>> =
        const { RefCell::new(None) };
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

/// Start the video frame UI subscriber.
///
/// Core owns the push + rate-limit loop (`SharedVideoFrames::subscribe`).
/// The subscriber callback runs on a tokio worker.
///
/// **Linux (GL path).** The callback publishes each participant's latest
/// `Arc<Vec<u8>>` RGBA + (w, h, rotation, generation) into the
/// `video::gl_call` inbox — a cheap Arc refcount bump, NO 30 MB copy and NO
/// `slint::Image` construction on the tokio thread. It then asks the UI
/// thread to redraw. The real work — uploading each frame into a *persistent*
/// per-participant GL texture in place (`glTexSubImage2D`) and populating the
/// `call_remote_video_frames` model with `BorrowedOpenGLTexture` images —
/// happens on the GL thread inside `gl_linux`'s rendering notifier. This
/// avoids the per-frame ~30 MB Skia `Data::new_copy` + fresh GPU upload that
/// the old `clone_from_slice` + `Image::from_rgba8` path forced on every
/// present (`from_rgba8` yields `ImageCacheKey::Invalid`).
///
/// **Non-Linux (CPU path).** No `gl_call` module exists, so the callback
/// keeps the prior behavior: deep-copy each frame into a `SharedPixelBuffer`
/// OFF the UI thread, then wrap it in a `slint::Image` on the UI thread and
/// update the persistent `VecModel` in place.
///
/// `ui_busy` provides backpressure on both paths: if the prior push hasn't
/// landed on the UI thread yet, the next one is dropped. A stale frame is
/// preferable to a stalled input queue.
///
/// The push rate-limit is deliberately small (≈4 ms ⇒ ~240 fps headroom)
/// rather than the old 33 ms (hard 30 fps). It is NOT the real throttle —
/// `ui_busy` + the compositor's vsync coalesce frames the renderer can't
/// keep up with. Capping at 33 ms was the single biggest reason a 60 fps
/// source rendered at 30; lifting it lets the source/refresh rate win.
const VIDEO_PUSH_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);

pub fn start_video_frame_timer(window: &MainWindow, shared_frames: SharedVideoFrames) {
    let weak = window.as_weak();
    let first_frame_logged = Arc::new(AtomicBool::new(false));
    let first_for_cb = first_frame_logged.clone();
    let ui_busy = Arc::new(AtomicBool::new(false));
    let ui_busy_for_cb = ui_busy.clone();

    let stop = shared_frames.subscribe(
        crate::rt_handle(),
        VIDEO_PUSH_MIN_INTERVAL,
        move |snapshot| {
            // Drop this frame if the previous push hasn't landed on the UI
            // thread yet. Better stale pixels than frozen input.
            if ui_busy_for_cb.swap(true, Ordering::Relaxed) {
                return;
            }

            // Stable tile order. The core snapshot iterates a HashMap, so its
            // order is unspecified and can shuffle between frames; the Slint
            // side indexes `remote-video-frames` positionally ([0], [1], …),
            // so an unstable order would swap participants' tiles. Sort by
            // participant id once, off-thread, to pin each participant to a
            // row.
            let mut ordered: Vec<&(String, _)> = snapshot.iter().collect();
            ordered.sort_by(|a, b| a.0.cmp(&b.0));
            let snapshot_len = ordered.len();

            #[cfg(target_os = "linux")]
            {
                // GL path: publish Arc-cloned frames into the gl_call inbox
                // (no pixel copy), then request a redraw so the notifier
                // drains the inbox and uploads on the GL thread.
                let batch: Vec<(String, Arc<Vec<u8>>, u32, u32, u32, u64)> = ordered
                    .iter()
                    .map(|(id, frame)| {
                        (
                            id.clone(),
                            frame.rgba_data.clone(),
                            frame.width,
                            frame.height,
                            frame.rotation_degrees,
                            frame.frame_id,
                        )
                    })
                    .collect();
                crate::bin_ui::video::gl_call::publish_batch(batch);

                let weak_invoke = weak.clone();
                let first_inner = first_for_cb.clone();
                let ui_busy_for_invoke = ui_busy_for_cb.clone();
                let dispatch = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_invoke.upgrade() {
                        if !first_inner.swap(true, Ordering::Relaxed) {
                            tracing::info!(
                                "Video subscriber: first frames received, publishing {} remote video frames (GL path)",
                                snapshot_len
                            );
                        }
                        // The notifier's BeforeRendering does the upload +
                        // model update; we only need to provoke a present.
                        w.window().request_redraw();
                    }
                    ui_busy_for_invoke.store(false, Ordering::Relaxed);
                });
                if dispatch.is_err() {
                    ui_busy_for_cb.store(false, Ordering::Relaxed);
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                // CPU path: deep-copy each frame OFF the UI thread. The
                // `SharedPixelBuffer` carries the data; wrapping it in
                // `slint::Image` on the UI side is cheap (no copy). We can't
                // build `slint::Image` here because it isn't `Send`. Keep the
                // participant id with each buffer so the local self-preview
                // frame can be split out on the UI thread.
                let buffers: Vec<(String, slint::SharedPixelBuffer<slint::Rgba8Pixel>)> = ordered
                    .iter()
                    .map(|(id, frame)| ((*id).clone(), slint::SharedPixelBuffer::clone_from_slice(
                        &frame.rgba_data, frame.width, frame.height,
                    )))
                    .collect();

                let weak_invoke = weak.clone();
                let first_inner = first_for_cb.clone();
                let ui_busy_for_invoke = ui_busy_for_cb.clone();
                let dispatch = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_invoke.upgrade() {
                        if !first_inner.swap(true, Ordering::Relaxed) {
                            tracing::info!(
                                "Video subscriber: first frames received, pushing {} remote video frames",
                                snapshot_len
                            );
                        }
                        // The last tile is the local "You" participant; its
                        // frame draws into `local-video-frame`, not a remote row.
                        // Remote slots MIRROR the tiles (same order, looked up by
                        // user_id, blank where a peer has no frame) — as the GL
                        // path does — so a video-off peer can't shift everyone
                        // else's frame onto the wrong tile.
                        let participants = w.get_active_call_data().participants;
                        let tile_ids: Vec<String> = (0..participants.row_count())
                            .filter_map(|i| participants.row_data(i))
                            .map(|p| p.user_id.to_string())
                            .collect();
                        let mut frames: std::collections::HashMap<String, _> = buffers.into_iter().collect();
                        let (remote_ids, local_id) = tile_ids.split_at(tile_ids.len().saturating_sub(1));
                        let local_img = local_id
                            .first()
                            .and_then(|id| frames.remove(id))
                            .map(slint::Image::from_rgba8)
                            .unwrap_or_default();
                        let remote_images: Vec<slint::Image> = remote_ids
                            .iter()
                            .map(|id| frames.remove(id).map(slint::Image::from_rgba8).unwrap_or_default())
                            .collect();
                        w.set_call_local_video_frame(local_img);
                        apply_remote_video_frames(&w, remote_images);
                    }
                    ui_busy_for_invoke.store(false, Ordering::Relaxed);
                });
                if dispatch.is_err() {
                    ui_busy_for_cb.store(false, Ordering::Relaxed);
                }
            }
        },
    );

    VIDEO_PUSH_STOP.with(|cell| *cell.borrow_mut() = Some(stop));
    tracing::debug!("Video frame subscriber started (core-driven push, 4ms rate limit)");
}

/// Apply a new batch of remote video frames to the UI, reusing the persistent
/// `VecModel` and updating rows in place.
///
/// `images` is one slot per remote tile, in tile order (blank = no frame). We
/// `set_row_data` for existing rows, `push` for new participants and `remove`
/// trailing rows when a participant leaves — never rebuilding the model. This
/// keeps the `call_remote_video_frames` property bound to one `ModelRc` for
/// the lifetime of the call, so Slint only re-renders the rows that changed
/// instead of resetting + re-binding the whole list on every frame.
///
/// Must run on the UI thread (driven from `invoke_from_event_loop`).
///
/// CPU path only; on Linux the GL texture path owns its own model.
#[cfg(not(target_os = "linux"))]
fn apply_remote_video_frames(w: &MainWindow, images: Vec<slint::Image>) {
    REMOTE_VIDEO_MODEL.with(|cell| {
        let mut slot = cell.borrow_mut();
        let model = match slot.as_ref() {
            Some(m) => m.clone(),
            None => {
                // First batch of this call: create the stable model and bind
                // it to the property exactly once.
                let m = Rc::new(VecModel::<slint::Image>::default());
                w.set_call_remote_video_frames(ModelRc::from(m.clone()));
                *slot = Some(m.clone());
                m
            }
        };

        let new_len = images.len();
        let old_len = model.row_count();

        // Overwrite / append.
        for (i, img) in images.into_iter().enumerate() {
            if i < old_len {
                model.set_row_data(i, img);
            } else {
                model.push(img);
            }
        }

        // Drop trailing rows for participants that went away. Remove from the
        // back so earlier indices stay valid.
        for i in (new_len..old_len).rev() {
            model.remove(i);
        }
    });
}

/// Stop the video frame subscriber (push task + keepalive timer).
pub fn stop_video_frame_timer() {
    VIDEO_FRAME_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
    VIDEO_PUSH_STOP.with(|cell| {
        if let Some(stop) = cell.borrow_mut().take() {
            stop.store(true, Ordering::Relaxed);
        }
    });

    // CPU path: drop the persistent model so the next call starts with a
    // fresh, empty list (and releases the held `slint::Image` textures).
    #[cfg(not(target_os = "linux"))]
    REMOTE_VIDEO_MODEL.with(|cell| {
        if let Some(model) = cell.borrow_mut().take() {
            model.set_vec(Vec::new());
        }
    });

    // GL path: clear the gl_call inbox so no stale frames are uploaded into
    // a subsequent call. The textures themselves are freed on rendering
    // teardown (`gl_call::on_teardown`); clearing the inbox makes the
    // texture set drain to empty on the next present, and a fresh call
    // refills it.
    #[cfg(target_os = "linux")]
    crate::bin_ui::video::gl_call::clear_inbox();

    tracing::debug!("Video frame subscriber stopped");
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
