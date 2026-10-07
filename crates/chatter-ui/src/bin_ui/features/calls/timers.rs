//! Call timer management.
//!
//! Manages call duration and video frame polling timers.

use std::cell::RefCell;
#[cfg(not(target_os = "linux"))]
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::{ComponentHandle, Timer};
#[cfg(not(target_os = "linux"))]
use slint::{Model, ModelRc, VecModel};

use crate::MainWindow;
use chatter_core::calls::SharedVideoFrames;
use chatter_core::client::ChatterClient;

thread_local! {
    /// Cancels an in-flight core snapshot when this call ends or is replaced.
    static CALL_TIMER_STOP: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
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

/// Start presentation ticks from core's connected-time snapshot.
/// Must run on the UI thread. Signaling timestamps are key-exchange metadata,
/// not the duration baseline, and delayed UI mounting must not restart the clock.
pub fn start_call_timer(window: &MainWindow, ui_ctx: Arc<ChatterClient>) {
    start_call_presentation_with(window, move || {
        let ui_ctx = ui_ctx.clone();
        async move { ui_ctx.call_room_snapshot().await }
    });
}

#[cfg(test)]
fn start_call_timer_with<F, Fut>(window: &MainWindow, snapshot: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<chatter_core::domain::call::Call>> + Send,
{
    start_call_presentation_with(window, move || {
        let read = snapshot();
        async move {
            read.await
                .map(|call| chatter_core::client::calls::CallRoomSnapshot {
                    call,
                    remotes: None,
                    verification: chatter_core::services::call_worker::CallEncryptionVerification::ExistingHandshake,
                })
        }
    });
}

// Poll even when the first read has no active call or is still ringing: room
// connection and UI mounting arrive through independent event streams.
fn start_call_presentation_with<F, Fut>(window: &MainWindow, snapshot: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<chatter_core::client::calls::CallRoomSnapshot>> + Send,
{
    stop_call_timer();
    let weak = window.as_weak();
    let presentation = window.get_active_call_data();
    let expected_call_id = presentation.call_id;
    let expected_channel_id = presentation.channel_id;
    let anonymous = presentation.is_anonymous_call;
    let stop = Arc::new(AtomicBool::new(false));
    CALL_TIMER_STOP.with(|cell| *cell.borrow_mut() = Some(stop.clone()));
    crate::rt_handle().spawn(async move {
        let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut snapshot_call_id = None;
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let call = snapshot().await;
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Some(snapshot) = call else { continue };
            let call = snapshot.call;
            // Standard presentation IDs are domain call IDs. Anonymous IDs
            // are room names; their setup supplies the synthetic channel ID.
            if (!anonymous && call.id.to_string() != expected_call_id.as_str())
                || (anonymous && call.channel_id.to_string() != expected_channel_id.as_str())
                || snapshot_call_id.is_some_and(|id| id != call.id)
            {
                continue;
            }
            snapshot_call_id = Some(call.id);
            let weak = weak.clone();
            let stop = stop.clone();
            let expected_call_id = expected_call_id.clone();
            let expected_channel_id = expected_channel_id.clone();
            if slint::invoke_from_event_loop(move || {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Some(w) = weak.upgrade() else { return };
                let mut data = w.get_active_call_data();
                if data.call_id != expected_call_id || data.channel_id != expected_channel_id {
                    return;
                }
                if let Some(remotes) = snapshot.remotes {
                    super::room_events::reconcile_roster(&mut data, remotes);
                }
                use chatter_core::services::call_worker::CallEncryptionVerification;
                match snapshot.verification {
                    CallEncryptionVerification::ExistingHandshake => {
                        // DM/anonymous SAS belongs to the existing handshake.
                        data.verification_key_version = "".into();
                    }
                    CallEncryptionVerification::WaitingForKey => {
                        data.verification_emoji_1 = "".into();
                        data.verification_emoji_2 = "".into();
                        data.verification_emoji_3 = "".into();
                        data.verification_emoji_4 = "".into();
                        data.verification_key_version = "".into();
                    }
                    CallEncryptionVerification::MediaKey { epoch, emojis } => {
                        let [e1, e2, e3, e4] = emojis;
                        data.verification_emoji_1 = e1.into();
                        data.verification_emoji_2 = e2.into();
                        data.verification_emoji_3 = e3.into();
                        data.verification_emoji_4 = e4.into();
                        data.verification_key_version = epoch.to_string().into();
                    }
                }
                data.is_connected = call.connected_at.is_some();
                data.duration_seconds = call.duration_seconds().min(i32::MAX as i64) as i32;
                w.set_active_call_data(data);
            })
            .is_err()
            {
                break;
            }
        }
    });
}

/// Stop ticks and invalidate any outstanding core snapshot request.
pub fn stop_call_timer() {
    CALL_TIMER_STOP.with(|cell| {
        if let Some(stop) = cell.borrow_mut().take() {
            stop.store(true, Ordering::Relaxed);
        }
    });
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
    stop_video_frame_timer();
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
                        let tile_ids: Vec<String> = participants.iter()
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use chatter_core::domain::call::{Call, CallType};
    use chatter_core::domain::ids::{ChannelId, UserId};
    use std::sync::Mutex;
    use std::time::Duration;

    pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
        static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
            std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().unwrap());
        crate::init_rt_handle(RUNTIME.handle().clone());
        &RUNTIME
    }

    pub(crate) fn timer_started() -> bool {
        CALL_TIMER_STOP.with(|cell| cell.borrow().is_some())
    }

    fn pump(window: &MainWindow, millis: u64) {
        Timer::single_shot(Duration::from_millis(millis), || {
            slint::quit_event_loop().unwrap();
        });
        window.show().unwrap();
        slint::run_event_loop().unwrap();
    }

    fn room_roster_restores_mount_overwrite_and_tracks_departures_and_rejoin() {
        use chatter_core::client::calls::CallRoomSnapshot;
        use chatter_core::ui::chat_data::CallParticipantInfo;
        use slint::{Model, ModelRc, VecModel};
        let window = MainWindow::new().unwrap();
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Group);
        call.start();
        let roster = |ids: &[&str]| {
            ids.iter()
                .map(|id| {
                    (
                        id.to_string(),
                        CallParticipantInfo {
                            display_name: id.to_string(),
                            ..Default::default()
                        },
                    )
                })
                .collect()
        };
        // Room joins already happened; mounting the outgoing view overwrites
        // them with its local-only placeholder. No more join events arrive.
        let mut data = window.get_active_call_data();
        data.call_id = call.id.to_string().into();
        data.channel_id = call.channel_id.to_string().into();
        data.participants = ModelRc::new(VecModel::from(vec![crate::CallParticipantData {
            user_id: "me".into(),
            is_muted: true,
            ..Default::default()
        }]));
        window.set_active_call_data(data.clone());
        let source = Arc::new(Mutex::new(Some(CallRoomSnapshot {
            call: call.clone(),
            verification:
                chatter_core::services::call_worker::CallEncryptionVerification::ExistingHandshake,
            remotes: Some(roster(&["b", "a"])),
        })));
        let read = source.clone();
        start_call_presentation_with(&window, move || {
            let snapshot = read.lock().unwrap().clone();
            async move { snapshot }
        });
        pump(&window, 100);
        let mounted = window.get_active_call_data();
        assert_eq!(mounted.participant_count, 3);
        assert_eq!(mounted.participants.row_data(0).unwrap().user_id, "a");
        assert_eq!(mounted.participants.row_data(2).unwrap().user_id, "me");
        assert!(mounted.participants.row_data(2).unwrap().is_muted);
        // Retain remote controls/media through reconciliation, remove a
        // departed peer and its screen tile without counting screens as peers.
        let mut a = mounted.participants.row_data(0).unwrap();
        a.is_video_enabled = true;
        a.is_audio_muted_locally = true;
        let mut with_media = mounted;
        with_media.participants = ModelRc::new(VecModel::from(vec![
            a,
            crate::CallParticipantData {
                user_id: "b".into(),
                ..Default::default()
            },
            crate::CallParticipantData {
                user_id: "b\u{1}screen".into(),
                is_screen_sharing: true,
                ..Default::default()
            },
            with_media.participants.row_data(2).unwrap(),
        ]));
        window.set_active_call_data(with_media);
        source.lock().unwrap().as_mut().unwrap().remotes = Some(roster(&["a"]));
        pump(&window, 1100);
        let departed = window.get_active_call_data();
        assert_eq!(departed.participant_count, 2);
        assert_eq!(departed.participants.row_count(), 2);
        assert!(departed.participants.row_data(0).unwrap().is_video_enabled);
        assert!(
            departed
                .participants
                .row_data(0)
                .unwrap()
                .is_audio_muted_locally
        );
        // Re-mount/rejoin repeats the overwrite; the immediate read repairs it.
        window.set_active_call_data(data);
        let read = source.clone();
        start_call_presentation_with(&window, move || {
            let snapshot = read.lock().unwrap().clone();
            async move { snapshot }
        });
        pump(&window, 100);
        assert_eq!(window.get_active_call_data().participant_count, 2);
        let mut other = call.clone();
        other.id = chatter_core::domain::ids::CallId::new();
        *source.lock().unwrap() = Some(CallRoomSnapshot {
            call: other,
            verification:
                chatter_core::services::call_worker::CallEncryptionVerification::ExistingHandshake,
            remotes: Some(roster(&["wrong-room"])),
        });
        pump(&window, 1100);
        assert_eq!(
            window
                .get_active_call_data()
                .participants
                .row_data(0)
                .unwrap()
                .user_id,
            "a"
        );
        stop_call_timer();
        // Anonymous pre-rendered names/avatars remain conversation-owned even
        // when the live identity adopts the empty remote slot.
        let mut anonymous = window.get_active_call_data();
        anonymous.is_anonymous_call = true;
        anonymous.participants = ModelRc::new(VecModel::from(vec![
            crate::CallParticipantData {
                display_name: "Private peer".into(),
                ..Default::default()
            },
            crate::CallParticipantData {
                user_id: "real-local-id\u{1}screen".into(),
                display_name: chatter_core::calls::tiles::LOCAL_SCREEN_TILE_NAME.into(),
                is_screen_sharing: true,
                is_video_enabled: true,
                ..Default::default()
            },
            crate::CallParticipantData {
                user_id: "You".into(),
                ..Default::default()
            },
        ]));
        super::super::room_events::reconcile_roster(&mut anonymous, roster(&["ephemeral"]));
        assert_eq!(
            anonymous.participants.row_data(0).unwrap().display_name,
            "Private peer"
        );
        assert_eq!(
            anonymous.participants.row_data(0).unwrap().user_id,
            "ephemeral"
        );
        assert_eq!(anonymous.participant_count, 2);
        assert_eq!(anonymous.participants.row_count(), 3);
        assert_eq!(
            anonymous.participants.row_data(1).unwrap().user_id,
            "real-local-id\u{1}screen"
        );
        assert_eq!(anonymous.participants.row_data(2).unwrap().user_id, "You");
        super::super::room_events::reconcile_roster(&mut anonymous, roster(&["ephemeral"]));
        assert_eq!(
            anonymous.participants.row_data(0).unwrap().display_name,
            "Private peer"
        );
    }

    fn verification_tracks_snapshot_with_identity_fences() {
        use chatter_core::client::calls::CallRoomSnapshot;
        use chatter_core::services::call_worker::CallEncryptionVerification as Verification;
        let window = MainWindow::new().unwrap();
        let call = Call::new(ChannelId::new(), UserId::new(), CallType::Group);
        let mut data = window.get_active_call_data();
        data.call_id = call.id.to_string().into();
        data.channel_id = call.channel_id.to_string().into();
        window.set_active_call_data(data);
        let code = |epoch| Verification::MediaKey {
            epoch,
            emojis: if epoch == u32::MAX {
                ["🌟", "🐶", "🚲", "🍎"].map(String::from)
            } else {
                ["🐱", "🌳", "🚀", "🍋"].map(String::from)
            },
        };
        let source = Arc::new(Mutex::new(Some(CallRoomSnapshot {
            call: call.clone(),
            remotes: None,
            verification: code(7),
        })));
        let read = source.clone();
        start_call_presentation_with(&window, move || {
            let snapshot = read.lock().unwrap().clone();
            async move { snapshot }
        });
        let assert_code = |version: &str, expected: [&str; 4]| {
            let data = window.get_active_call_data();
            assert_eq!(data.verification_key_version.as_str(), version);
            assert_eq!(
                [
                    data.verification_emoji_1.as_str(),
                    data.verification_emoji_2.as_str(),
                    data.verification_emoji_3.as_str(),
                    data.verification_emoji_4.as_str(),
                ],
                expected
            );
        };
        pump(&window, 100);
        assert_code("7", ["🐱", "🌳", "🚀", "🍋"]);
        source.lock().unwrap().as_mut().unwrap().verification = code(u32::MAX);
        pump(&window, 1100);
        assert_code("4294967295", ["🌟", "🐶", "🚲", "🍎"]);
        source.lock().unwrap().as_mut().unwrap().verification = Verification::WaitingForKey;
        pump(&window, 1100);
        assert_code("", ["", "", "", ""]);
        // A fresh key must replace every slot after pending, in the wire order.
        source.lock().unwrap().as_mut().unwrap().verification = code(9);
        pump(&window, 1100);
        assert_code("9", ["🐱", "🌳", "🚀", "🍋"]);
        // Handshake snapshots preserve both standard and anonymous SAS.
        for anonymous in [false, true] {
            let mut data = window.get_active_call_data();
            data.is_anonymous_call = anonymous;
            window.set_active_call_data(data);
            source.lock().unwrap().as_mut().unwrap().verification = Verification::ExistingHandshake;
            let read = source.clone();
            start_call_presentation_with(&window, move || {
                let snapshot = read.lock().unwrap().clone();
                async move { snapshot }
            });
            pump(&window, 100);
            assert_code("", ["🐱", "🌳", "🚀", "🍋"]);
        }
        // A pending result from another core call cannot erase this code.
        source.lock().unwrap().as_mut().unwrap().call.id = chatter_core::domain::ids::CallId::new();
        source.lock().unwrap().as_mut().unwrap().verification = Verification::WaitingForKey;
        pump(&window, 1100);
        assert_code("", ["🐱", "🌳", "🚀", "🍋"]);
        stop_call_timer();
        // Hold a real production snapshot in flight, replace UI, then release it.
        let mut original = window.get_active_call_data();
        original.is_anonymous_call = false;
        window.set_active_call_data(original);
        let (send, receive) = tokio::sync::oneshot::channel();
        let receive = Arc::new(Mutex::new(Some(receive)));
        start_call_presentation_with(&window, move || {
            let receive = receive.lock().unwrap().take();
            async move {
                match receive {
                    Some(receive) => receive.await.unwrap(),
                    None => None,
                }
            }
        });
        pump(&window, 100);
        let mut replacement = window.get_active_call_data();
        replacement.call_id = "replacement".into();
        replacement.verification_key_version = "123".into();
        window.set_active_call_data(replacement);
        send.send(Some(CallRoomSnapshot {
            call,
            remotes: None,
            verification: Verification::WaitingForKey,
        }))
        .unwrap();
        pump(&window, 100);
        assert_code("123", ["🐱", "🌳", "🚀", "🍋"]);
        stop_call_timer();
    }

    #[test]
    fn call_presentation_restores_roster_and_refreshes_timer_with_identity_and_cancellation_guards()
    {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let _runtime = runtime();
        verification_tracks_snapshot_with_identity_fences();
        room_roster_restores_mount_overwrite_and_tracks_departures_and_rejoin();
        let window = MainWindow::new().unwrap();
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Group);
        call.ring();
        let mut data = window.get_active_call_data();
        data.call_id = call.id.to_string().into();
        window.set_active_call_data(data);
        let snapshot = Arc::new(Mutex::new(None::<Call>));
        let source = snapshot.clone();
        start_call_timer_with(&window, move || {
            let call = source.lock().unwrap().clone();
            async move { call }
        });
        pump(&window, 100);
        assert!(!window.get_active_call_data().is_connected);
        *snapshot.lock().unwrap() = Some(call.clone());
        pump(&window, 1100);
        assert_eq!(window.get_active_call_data().duration_seconds, 0);
        assert!(!window.get_active_call_data().is_connected);
        call.start();
        call.connected_at = Some(chrono::Utc::now() - chrono::Duration::seconds(40));
        *snapshot.lock().unwrap() = Some(call.clone());
        pump(&window, 1100);
        let data = window.get_active_call_data();
        assert!(data.is_connected);
        assert!(
            data.duration_seconds >= 40,
            "late connection must replace the ringing snapshot"
        );
        let before = data.duration_seconds;
        pump(&window, 1100);
        assert!(window.get_active_call_data().duration_seconds > before);

        // Core may replace its call before the corresponding UI mount.
        let mut other = Call::new(ChannelId::new(), UserId::new(), CallType::Group);
        other.start();
        other.connected_at = Some(chrono::Utc::now() - chrono::Duration::seconds(500));
        *snapshot.lock().unwrap() = Some(other);
        let before = window.get_active_call_data().duration_seconds;
        pump(&window, 1100);
        assert_eq!(window.get_active_call_data().duration_seconds, before);
        *snapshot.lock().unwrap() = Some(call.clone());

        // A queued result for this call cannot paint a replacement UI.
        let mut replacement = window.get_active_call_data();
        replacement.call_id = "replacement".into();
        replacement.duration_seconds = 0;
        replacement.is_connected = false;
        window.set_active_call_data(replacement);
        pump(&window, 1100);
        assert_eq!(window.get_active_call_data().duration_seconds, 0);
        assert!(!window.get_active_call_data().is_connected);
        stop_call_timer();

        // Anonymous UI IDs are room names, not the core's random domain IDs.
        let mut anonymous = window.get_active_call_data();
        anonymous.call_id = "anon-room".into();
        anonymous.channel_id = call.channel_id.to_string().into();
        anonymous.is_anonymous_call = true;
        window.set_active_call_data(anonymous);
        let source = snapshot.clone();
        start_call_timer_with(&window, move || {
            let call = source.lock().unwrap().clone();
            async move { call }
        });
        pump(&window, 100);
        assert!(window.get_active_call_data().is_connected);
        assert!(window.get_active_call_data().duration_seconds >= 40);
        stop_call_timer();
        let mut data = window.get_active_call_data();
        data.duration_seconds = 0;
        data.is_connected = false;
        window.set_active_call_data(data);

        // Cancel while the core read is in flight, then let that read finish.
        let (send, receive) = tokio::sync::oneshot::channel();
        let receive = Arc::new(Mutex::new(Some(receive)));
        start_call_timer_with(&window, move || {
            let receive = receive.lock().unwrap().take().unwrap();
            async move { receive.await.unwrap() }
        });
        pump(&window, 100);
        stop_call_timer();
        send.send(Some(call)).unwrap();
        pump(&window, 100);
        assert_eq!(window.get_active_call_data().duration_seconds, 0);
        assert!(!window.get_active_call_data().is_connected);
    }
}
