//! Signal call event handlers (non-anonymous).

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use crate::MainWindow;

/// Handle incoming call key offer — show the incoming call modal.
pub(super) fn handle_call_key_offer(
    window: &Weak<MainWindow>,
    _ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    peer_id: UserId,
    _ephemeral_public_key: [u8; 32],
    call_type: String,
    _channel_id: String,
    is_identity_verified: bool,
    identity_warning: String,
    caller_name: String,
    _timestamp: u64,
) {
    let weak = window.clone();

    // The ephemeral key is passed to the UI via show_incoming_call() -> IncomingCallData.
    // When the user accepts, incoming.rs reads it from IncomingCallData and passes to core.
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::show_incoming_call(
                &w,
                &call_id,
                &peer_id.to_string(),
                &caller_name,
                &call_type,
                is_identity_verified,
                &identity_warning,
                false, // not anonymous
            );

            tracing::info!(
                peer_id = %peer_id,
                call_type = %call_type,
                verified = is_identity_verified,
                "Showing incoming call modal"
            );
        }
    });
}

// NOTE: CallKeyAnswerReceived and GroupCallKeyReceived are handled by
// calls/event_handlers.rs (wire_call_key_answer_handler), which delegates
// to key_exchange::handle_call_key_answer for ECDH key derivation and
// call_service.set_encryption_key for group keys. The domain_handler
// arms for these events are intentional no-ops.

/// Handle call hangup received — hide call UI.
pub(super) fn handle_call_hangup(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    peer_id: UserId,
    reason: String,
) {
    let weak = window.clone();

    // Clear call state in core
    ui_ctx.chat_state().clear_call_state();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            super::super::super::initial_data::hide_incoming_call(&w);
            tracing::info!(
                call_id = %call_id,
                peer_id = %peer_id,
                reason = %reason,
                "Call ended"
            );
        }
    });
}
