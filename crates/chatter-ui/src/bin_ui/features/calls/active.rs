//! Active call controls.
//!
//! Handles mute, video toggle, screen share, and call end.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
#[cfg(not(target_os = "android"))]
use chatter_core::calls::ScreenCapture;
use crate::MainWindow;

use super::timers::{get_call_duration_secs, stop_call_timer, stop_video_frame_timer};

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

    // Toggle video
    window.on_call_toggle_video({
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                let new_state = !ui_ctx.call_is_camera_enabled().await;
                ui_ctx.call_set_camera_enabled(new_state);
                tracing::info!("Camera enabled: {}", new_state);
                if let Some(w) = weak.upgrade() {
                    w.set_call_is_video_enabled(new_state);
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
                    match ScreenCapture::primary_source() {
                        Ok(source) => {
                            if let Err(e) = _ui_ctx.call_publish_screen_share(source).await {
                                tracing::error!("Failed to start screen share: {}", e);
                            } else {
                                tracing::info!("Screen share started");
                                if let Some(w) = _weak.upgrade() {
                                    w.set_call_is_screen_sharing(true);
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
                    if let Some(w) = _weak.upgrade() {
                        w.set_call_is_screen_sharing(false);
                    }
                }
            })
            .unwrap();
            #[cfg(target_os = "android")]
            tracing::warn!("Screen sharing not available on Android");
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
