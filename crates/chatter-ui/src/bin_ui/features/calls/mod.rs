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
//! - `incoming` - Incoming call accept/decline + orchestrator event glue
//! - `active` - Active call controls (mute, video, screen share, end)
//! - `device_picker` - Audio and screen device selection
//! - `event_handlers` - Domain event handling for call keys

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use crate::MainWindow;

mod active;
mod device_picker;
mod event_handlers;
mod incoming;
pub mod key_exchange;
mod outgoing;
mod room_events;
pub mod timers;

use slint::{Model, ModelRc, VecModel, Weak};

use crate::bin_ui::converters::avatar_bytes_to_image;
use crate::CallParticipantData;

/// Patch the active call's participant row matching `user_id` with the given
/// avatar bytes. Must be called on the UI thread. No-op if the bytes can't be
/// decoded or no row matches.
pub(super) fn apply_remote_avatar(w: &MainWindow, user_id: &str, bytes: &[u8]) {
    let Some(img) = avatar_bytes_to_image(bytes) else { return };
    let mut data = w.get_active_call_data();
    let participants: Vec<CallParticipantData> = (0..data.participants.row_count())
        .map(|i| {
            let mut p = data.participants.row_data(i).unwrap_or_default();
            if p.user_id.as_str() == user_id {
                p.avatar_source = img.clone();
            }
            p
        })
        .collect();
    data.participants = ModelRc::new(VecModel::from(participants));
    w.set_active_call_data(data);
}

/// Background fetch of a remote user's avatar via the server profile URL,
/// then patch the matching participant row. Used for non-anon paths (regular
/// DM accept, LiveKit `ParticipantJoined`) where no avatar bytes are cached
/// locally. No-op if the user has no `avatar_url` or the download fails.
pub(super) fn spawn_fetch_remote_avatar(
    weak: Weak<MainWindow>,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    user_id: chatter_core::domain::ids::UserId,
) {
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let Some(bytes) = ui_ctx.get_user_avatar_bytes(&user_id).await else { return };
        let user_id_str = user_id.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            apply_remote_avatar(&w, &user_id_str, &bytes);
        });
    });
}

/// Wire all call-related callbacks.
///
/// Note: call_service is passed separately because it's initialized post-boot
/// via AppContext.init_call_service() and stored in a OnceLock.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    outgoing::wire_start_call(window, ui_ctx);
    incoming::wire_incoming_call(window, ui_ctx);
    incoming::wire_orchestrator_events(window, ui_ctx);
    active::wire_active_call(window, ui_ctx);
    room_events::wire_room_events(window, ui_ctx);
    device_picker::wire_device_picker(window, ui_ctx);
    active::wire_call_hangup_handler(window, ui_ctx);
    event_handlers::wire_call_key_answer_handler(window, ui_ctx);
}
