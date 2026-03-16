//! Call UI callback wiring.
//!
//! Wires call-related Slint callbacks to CallService.
//!
//! This module is organized into:
//! - `timers` - Call duration and video frame polling timers
//! - `key_exchange` - Ephemeral key exchange for E2EE calls
//! - `call_history` - Call history message creation
//! - `room_events` - LiveKit room event handling
//! - `outgoing` - Outgoing call initiation
//! - `incoming` - Incoming call accept/decline
//! - `active` - Active call controls (mute, video, screen share, end)
//! - `device_picker` - Audio and screen device selection
//! - `event_handlers` - Domain event handling for call keys

use std::sync::Arc;

use chatter_linux::app::UIContext;
use crate::MainWindow;

mod active;
pub mod call_history;
mod device_picker;
mod event_handlers;
mod incoming;
pub mod key_exchange;
mod outgoing;
mod room_events;
pub mod timers;

// Re-export for external use (cleanup_call_ui_state is used by auth/inbox)

/// Clean up call UI state when a call ends remotely (via hangup notification).
///
/// This must be called from the main UI thread via `invoke_from_event_loop`.
/// Use this from inbox_events.rs when receiving CallHangup.
pub fn cleanup_call_ui_state() {
    timers::stop_call_timer();
    timers::stop_video_frame_timer();
    // Clear any pending outgoing call state
    key_exchange::clear_pending_call();
    tracing::debug!("Call UI state cleaned up (remote hangup)");
}

/// Wire all call-related callbacks.
///
/// Note: call_service is passed separately because it's initialized post-boot
/// via AppContext.init_call_service() and stored in a OnceLock.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    outgoing::wire_start_call(window, ui_ctx);
    incoming::wire_incoming_call(window, ui_ctx);
    active::wire_active_call(window, ui_ctx);
    room_events::wire_room_events(window, ui_ctx);
    device_picker::wire_device_picker(window, ui_ctx);
    active::wire_call_hangup_handler(window, ui_ctx);
    event_handlers::wire_call_key_answer_handler(window, ui_ctx);
}
