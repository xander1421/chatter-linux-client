//! Active call controls.
//!
//! Handles mute, video toggle, screen share, and call end.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
#[cfg(not(target_os = "android"))]
use chatter_core::calls::ScreenCapture;
use chatter_core::inbox::handlers::DomainEvent;
use crate::MainWindow;

use super::timers::{get_call_duration_secs, stop_call_timer, stop_video_frame_timer};

/// Wire active call control callbacks.
pub fn wire_active_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let call_service = ui_ctx.call_service().clone();

    // Toggle mute
    window.on_call_toggle_mute({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_muted = service.is_microphone_muted().await;
                    let new_state = !is_muted;
                    service.set_microphone_muted(new_state);
                    tracing::info!("Microphone muted: {}", new_state);

                    if let Some(w) = weak.upgrade() {
                        w.set_call_is_muted(new_state);
                    }
                }
            }).unwrap();
        }
    });

    // Toggle video
    window.on_call_toggle_video({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_enabled = service.is_camera_enabled().await;
                    let new_state = !is_enabled;
                    service.set_camera_enabled(new_state);
                    tracing::info!("Camera enabled: {}", new_state);

                    if let Some(w) = weak.upgrade() {
                        w.set_call_is_video_enabled(new_state);
                    }
                }
            }).unwrap();
        }
    });

    // Toggle screen share (desktop only)
    window.on_call_toggle_screen_share({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let _call_service = call_service.clone();
            let _weak = weak.clone();
            #[cfg(not(target_os = "android"))]
            slint::spawn_local(async move {
                if let Some(service) = _call_service.get() {
                    let is_sharing = service.is_screen_sharing().await;
                    let new_state = !is_sharing;

                    if new_state {
                        // Start screen share - use primary display
                        match ScreenCapture::primary_source() {
                            Ok(source) => {
                                if let Err(e) = service.publish_screen_share(source).await {
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
                        // Stop screen share
                        service.stop_screen_share();
                        tracing::info!("Screen share stopped");
                        if let Some(w) = _weak.upgrade() {
                            w.set_call_is_screen_sharing(false);
                        }
                    }
                }
            }).unwrap();
            #[cfg(target_os = "android")]
            tracing::warn!("Screen sharing not available on Android");
        }
    });

    // End call
    window.on_call_end({
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        let call_service = call_service.clone();

        move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            let _call_service = call_service.clone();
            tracing::info!("End call");

            // Get call info before the async block
            let (_call_id_str, is_anonymous) = if let Some(w) = weak.upgrade() {
                let data = w.get_active_call_data();
                (data.call_id.to_string(), data.is_anonymous_call)
            } else {
                (String::new(), false)
            };

            slint::spawn_local(async move {
                // IMPORTANT: Update UI immediately BEFORE async operations
                // This prevents the modal from staying visible if network calls block
                stop_call_timer();
                stop_video_frame_timer();
                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                }

                // End call via core (handles hangup signal, history storage, leave)
                if is_anonymous {
                    let duration = get_call_duration_secs() as i64;
                    let ctx = ui_ctx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = ctx.end_anon_call(duration).await {
                            tracing::error!(error = %e, "Failed to end anon call");
                        }
                    });
                } else {
                    let duration = get_call_duration_secs() as i64;
                    let ctx = ui_ctx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = ctx.end_dm_call(duration).await {
                            tracing::error!(error = %e, "Failed to end DM call");
                        }
                    });
                }
            }).unwrap();
        }
    });
}

/// Wire call hangup handler to process remote hangup events.
///
/// Subscribes to `DomainEvent::CallHangupReceived` from the event bus
/// and calls `leave_call()` on the CallService to disconnect from LiveKit.
pub fn wire_call_hangup_handler(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let call_service = ui_ctx.call_service().clone();
    let mut event_rx = ui_ctx.event_bus().subscribe_domain();
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    // Spawn a task to handle incoming hangup events
    // Note: This runs in a slint::spawn_local context which keeps CallService on UI thread
    slint::spawn_local(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Domain event receiver lagged, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if let DomainEvent::CallHangupReceived { call_id, peer_id, reason } = event {
                tracing::info!(
                    call_id = %call_id,
                    peer_id = %peer_id,
                    reason = %reason,
                    "Processing remote call hangup"
                );

                // Get call duration BEFORE stopping the timer
                let duration_secs = get_call_duration_secs();

                // Get CallService and disconnect locally
                // IMPORTANT: Don't call leave_call() here - that would make a server
                // request which can block/timeout since the peer already ended the call.
                // Just disconnect locally from LiveKit.
                if let Some(service) = call_service.get() {
                    // Verify this hangup is for our active call
                    let is_our_call = service.active_call().await
                        .as_ref()
                        .map(|c| c.id.to_string() == call_id)
                        .unwrap_or(false);

                    if is_our_call {
                        // Core handles: store history + disconnect locally
                        let ctx = ui_ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ctx.handle_remote_hangup(duration_secs).await {
                                tracing::error!(error = %e, "Failed to handle remote hangup");
                            }
                        });
                    } else {
                        tracing::debug!(
                            call_id = %call_id,
                            "Ignoring hangup for non-active call"
                        );
                    }
                }

                // Stop the call timer and video frame timer, hide the call UI
                // IMPORTANT: Must stop timers to prevent calloop event loop issues
                stop_call_timer();
                stop_video_frame_timer();

                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                    w.set_incoming_call_visible(false);
                }
            }
        }
    }).unwrap();
}
