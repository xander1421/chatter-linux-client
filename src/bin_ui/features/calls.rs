//! Call UI callback wiring.
//!
//! Wires call-related Slint callbacks to service layer.
//!
//! Note: Full integration requires adding CallService to AppContext.
//! Currently provides callback structure with logging for future integration.

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use crate::MainWindow;

/// Wire all call-related callbacks.
pub fn wire(window: &MainWindow, _ctx: &AppContext) {
    wire_incoming_call(window);
    wire_active_call(window);
}

fn wire_incoming_call(window: &MainWindow) {
    let weak = window.as_weak();

    // Accept call callback
    window.on_accept_incoming_call(move |call_id| {
        let call_id_str = call_id.to_string();
        tracing::info!("Accept call: {}", call_id_str);

        // TODO: Wire to CallService.join_call() when integrated into AppContext
        // For now, just hide the incoming call modal
        if let Some(w) = weak.upgrade() {
            w.set_incoming_call_visible(false);
            w.set_active_call_visible(true);
        }
    });

    let weak = window.as_weak();

    // Decline call callback
    window.on_decline_incoming_call(move |call_id| {
        let call_id_str = call_id.to_string();
        tracing::info!("Decline call: {}", call_id_str);

        // TODO: Notify server that call was declined
        if let Some(w) = weak.upgrade() {
            w.set_incoming_call_visible(false);
        }
    });
}

fn wire_active_call(window: &MainWindow) {
    // Toggle mute
    window.on_call_toggle_mute(|| {
        tracing::info!("Toggle mute");
        // TODO: Wire to RoomManager audio track mute
    });

    // Toggle video
    window.on_call_toggle_video(|| {
        tracing::info!("Toggle video");
        // TODO: Wire to RoomManager video track toggle
    });

    // Toggle screen share
    window.on_call_toggle_screen_share(|| {
        tracing::info!("Toggle screen share");
        // TODO: Wire to RoomManager screen share track
    });

    let weak = window.as_weak();

    // End call
    window.on_call_end(move || {
        tracing::info!("End call");

        // TODO: Wire to CallService.leave_call()
        if let Some(w) = weak.upgrade() {
            w.set_active_call_visible(false);
        }
    });
}
