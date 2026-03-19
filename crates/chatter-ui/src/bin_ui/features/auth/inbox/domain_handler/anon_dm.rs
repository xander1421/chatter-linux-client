//! Anonymous DM event handlers.

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::ids::UserId;
use crate::MainWindow;

/// Handle anonymous DM request received.
pub(super) fn handle_request_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anon DM list to show the request
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);

            // Update pending message request count
            if let Ok(requests) = ui_ctx.anon_dms().list_message_requests() {
                w.set_pending_message_requests(requests.len() as i32);
            }
        }
    });
}

/// Handle anonymous DM request accepted.
pub(super) fn handle_request_accepted(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    peer_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anon DM list
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
            tracing::info!(
                conversation_id = %conversation_id,
                peer_id = %peer_id,
                "Anon DM request accepted"
            );
        }
    });
}

/// Handle anonymous DM message received.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    // Message display handled by message state
    // But we need to refresh the sidebar to update latest message preview
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
        }
    });
}

/// Handle anonymous DM init received.
pub(super) fn handle_init_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Refresh anon DM list for new init
            super::super::super::initial_data::refresh_anon_dms(&w, &ui_ctx);
        }
    });
}
