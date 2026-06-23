//! Active call controls.
//!
//! Handles mute, video toggle, screen share, and call end.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
#[cfg(not(target_os = "android"))]
use chatter_core::calls::ScreenCapture;
use slint::{Model, ModelRc, SharedString, VecModel};
use crate::{CallParticipantData, MainWindow};

use super::timers::{get_call_duration_secs, stop_call_timer, stop_video_frame_timer};

/// Toggle the local participant's `is_screen_sharing` flag in the active-call
/// model so their own tile picks up the screen-share badge. Remote peers'
/// flags are driven by `LiveKitRoomEvent::TrackSubscribed` in `room_events.rs`;
/// the local user never receives that event for their own publish, so we
/// flip it here.
#[cfg(not(target_os = "android"))]
fn set_local_participant_screen_sharing(window: &MainWindow, local_user_id: &str, sharing: bool) {
    let mut data = window.get_active_call_data();
    let updated: Vec<CallParticipantData> = (0..data.participants.row_count())
        .map(|i| {
            let mut p = data.participants.row_data(i).unwrap_or_default();
            if p.user_id.to_string() == local_user_id {
                p.is_screen_sharing = sharing;
            }
            p
        })
        .collect();
    data.participants = ModelRc::new(VecModel::from(updated));
    window.set_active_call_data(data);
}

/// Mutate the participant entry matching `target_user_id` via `f`, leaving
/// every other entry untouched, and push the updated model back onto the
/// window. Returns the (post-mutation) participant if found.
fn update_participant<F>(
    window: &MainWindow,
    target_user_id: &str,
    mut f: F,
) -> Option<CallParticipantData>
where
    F: FnMut(&mut CallParticipantData),
{
    let mut data = window.get_active_call_data();
    let mut found: Option<CallParticipantData> = None;
    let updated: Vec<CallParticipantData> = (0..data.participants.row_count())
        .map(|i| {
            let mut p = data.participants.row_data(i).unwrap_or_default();
            if p.user_id.to_string() == target_user_id {
                f(&mut p);
                found = Some(p.clone());
            }
            p
        })
        .collect();
    data.participants = ModelRc::new(VecModel::from(updated));
    window.set_active_call_data(data);
    found
}

/// Wire active call control callbacks.
pub fn wire_active_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Toggle mute
    window.on_call_toggle_mute({
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                let new_state = !ui_ctx.call_is_microphone_muted().await;
                ui_ctx.call_set_microphone_muted(new_state);
                tracing::info!("Microphone muted: {}", new_state);
                if let Some(w) = weak.upgrade() {
                    w.set_call_is_muted(new_state);
                }
            })
            .unwrap();
        }
    });

    // Toggle video. Works for any call — including a call that started as
    // voice — so the user can escalate to video mid-call. Turning the camera on
    // the first time publishes the camera track (needs the connected room, like
    // screen share); after that, toggling just pauses/resumes capture.
    window.on_call_toggle_video({
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                let new_state = !ui_ctx.call_is_camera_enabled().await;
                if new_state {
                    // Camera publish needs the connected room — during the
                    // outgoing "Calling…" state there's no room yet.
                    if !ui_ctx.call_is_connected().await {
                        tracing::info!("Video unavailable — call not connected yet");
                        return;
                    }
                    // Android: a call that started as voice never requested the
                    // CAMERA permission. Request on demand; if not yet granted,
                    // bail so CameraX doesn't fail silently — the user grants via
                    // the system dialog and taps the video button again.
                    #[cfg(target_os = "android")]
                    {
                        use crate::platform::android::permissions;
                        if !permissions::has(permissions::CAMERA) {
                            permissions::request(&[permissions::CAMERA]);
                            tracing::info!("Requested CAMERA permission — tap video again once granted");
                            return;
                        }
                    }
                    // Idempotent in core: publishes the track the first time,
                    // resumes capture on later enables.
                    if let Err(e) = ui_ctx.call_publish_camera(None).await {
                        tracing::error!("Failed to enable camera: {}", e);
                        return;
                    }
                    tracing::info!("Camera enabled");
                } else {
                    ui_ctx.call_set_camera_enabled(false);
                    tracing::info!("Camera disabled");
                }
                let local_id = ui_ctx.session_current_user_id().map(|u| u.to_string());
                if let Some(w) = weak.upgrade() {
                    w.set_call_is_video_enabled(new_state);
                    if let Some(id) = local_id {
                        update_participant(&w, &id, |p| p.is_video_enabled = new_state);
                    }
                }
            })
            .unwrap();
        }
    });

    // Toggle screen share (desktop only)
    window.on_call_toggle_screen_share({
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        move || {
            let _ui_ctx = ui_ctx.clone();
            let _weak = weak.clone();
            #[cfg(not(target_os = "android"))]
            slint::spawn_local(async move {
                let new_state = !_ui_ctx.call_is_screen_sharing().await;
                if new_state {
                    // No room during the outgoing "Calling…" state — starting a
                    // share now fails NotInCall. Gate on the connected room.
                    if !_ui_ctx.call_is_connected().await {
                        tracing::info!("Screen share unavailable — call not connected yet");
                        return;
                    }
                    match ScreenCapture::primary_source() {
                        Ok(source) => {
                            if let Err(e) = _ui_ctx.call_publish_screen_share(source).await {
                                tracing::error!("Failed to start screen share: {}", e);
                            } else {
                                tracing::info!("Screen share started");
                                let local_id = _ui_ctx.session_current_user_id().map(|u| u.to_string());
                                if let Some(w) = _weak.upgrade() {
                                    w.set_call_is_screen_sharing(true);
                                    if let Some(id) = local_id {
                                        set_local_participant_screen_sharing(&w, &id, true);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("No screen source available: {}", e);
                        }
                    }
                } else {
                    _ui_ctx.call_stop_screen_share();
                    tracing::info!("Screen share stopped");
                    let local_id = _ui_ctx.session_current_user_id().map(|u| u.to_string());
                    if let Some(w) = _weak.upgrade() {
                        w.set_call_is_screen_sharing(false);
                        if let Some(id) = local_id {
                            set_local_participant_screen_sharing(&w, &id, false);
                        }
                    }
                }
            })
            .unwrap();
            #[cfg(target_os = "android")]
            tracing::warn!("Screen sharing not available on Android");
        }
    });

    // Per-peer local audio mute toggle.
    window.on_call_peer_audio_mute_toggle({
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        move |user_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let uid = user_id.to_string();
            let updated = update_participant(&w, &uid, |p| {
                p.is_audio_muted_locally = !p.is_audio_muted_locally;
            });
            let Some(p) = updated else { return };
            ui_ctx.call_set_peer_audio_muted(uid.clone(), p.is_audio_muted_locally);
            tracing::info!(
                peer = %uid,
                muted = p.is_audio_muted_locally,
                "Per-peer audio mute toggled (local)"
            );
        }
    });

    // Per-peer local video pause toggle. Flipping `is_video_enabled` makes
    // the tile fall back to the avatar while paused, since the core layer
    // also drops the cached frame from `shared_frames`.
    window.on_call_peer_video_pause_toggle({
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        move |user_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let uid = user_id.to_string();
            let updated = update_participant(&w, &uid, |p| {
                p.is_video_paused_locally = !p.is_video_paused_locally;
                if p.is_video_paused_locally {
                    p.is_video_enabled = false;
                } else {
                    // Resuming — let the next decoded frame flip the tile
                    // back to video by re-marking video as enabled. If no
                    // frames arrive, the TrackUnsubscribed handler will
                    // clear it again.
                    p.is_video_enabled = true;
                }
            });
            let Some(p) = updated else { return };
            ui_ctx.call_set_peer_video_paused(uid.clone(), p.is_video_paused_locally);
            tracing::info!(
                peer = %uid,
                paused = p.is_video_paused_locally,
                "Per-peer video pause toggled (local)"
            );
        }
    });

    // End call
    window.on_call_end({
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();

        move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tracing::info!("End call");

            slint::spawn_local(async move {
                // Read duration BEFORE stopping timer — stop_call_timer clears CALL_START_NS
                let duration = get_call_duration_secs() as i64;

                // Update UI immediately BEFORE async operations so the modal
                // doesn't linger if network calls block.
                stop_call_timer();
                stop_video_frame_timer();
                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                }

                // Core resolves call kind (anon/DM/group) from orchestrator state.
                let ctx = ui_ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = ctx.end_active_call(duration).await {
                        tracing::error!(error = %e, "Failed to end active call");
                    }
                });
            }).unwrap();
        }
    });
}

/// Wire call hangup handler to process remote hangup events.
///
/// Subscribes to the typed remote-hangup stream on [`ChatterClient`] and
/// calls `leave_call()` on the CallService to disconnect from LiveKit.
pub fn wire_call_hangup_handler(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let mut event_rx = ui_ctx.subscribe_remote_call_hangups();
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    // Spawn a task to handle incoming hangup events
    slint::spawn_local(async move {
        loop {
            let hangup = match event_rx.recv().await {
                Ok(hangup) => hangup,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Remote call hangup receiver lagged, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let chatter_core::client::events::RemoteCallHangup { call_id, peer_id, reason } = hangup;
            tracing::info!(
                call_id = %call_id,
                peer_id = %peer_id,
                reason = %reason,
                "Processing remote call hangup"
            );

            // Core gates on active-call match internally — UI just forwards.
            let duration_secs = get_call_duration_secs();
            let ctx = ui_ctx.clone();
            let call_id_owned = call_id.clone();
            tokio::spawn(async move {
                if let Err(e) = ctx.handle_remote_hangup(&call_id_owned, duration_secs).await {
                    tracing::error!(error = %e, "Failed to handle remote hangup");
                }
            });

            stop_call_timer();
            stop_video_frame_timer();

            if let Some(w) = weak.upgrade() {
                w.set_active_call_visible(false);
                w.set_incoming_call_visible(false);
            }
        }
    })
    .unwrap();
}
