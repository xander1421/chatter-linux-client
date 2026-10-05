//! Signal call event handlers (non-anonymous).

use std::sync::Arc;

use slint::Weak;

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

/// Handle incoming call key offer.
///
/// Deprecated inside this path: the CallOrchestrator bridge
/// ([`chatter_core::services::call_worker::spawn_call_orchestrator_bridge`])
/// now owns the decision to present the incoming-call modal, so we keep
/// this function as a trace-only stub until the UI dispatcher stops
/// forwarding `CallKeyOfferReceived` explicitly.
pub(super) fn handle_call_key_offer(
    _window: &Weak<MainWindow>,
    _ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    peer_id: UserId,
    _ephemeral_public_key: [u8; 32],
    call_type: String,
    _channel_id: String,
    is_identity_verified: bool,
    _identity_warning: String,
    caller_name: String,
    _timestamp: u64,
) {
    tracing::trace!(
        %call_id,
        %peer_id,
        %call_type,
        verified = is_identity_verified,
        caller = %caller_name,
        "CallKeyOfferReceived: orchestrator bridge drives the modal now"
    );
}

// NOTE: CallKeyAnswerReceived is handled by calls/event_handlers.rs
// (wire_call_key_answer_handler), which delegates to
// key_exchange::handle_call_key_answer for ECDH key derivation.
// GroupCallSignalReceived is handled in core (ChatterClient::handle_group_call_signal);
// the UI never sees group key bytes. The domain_handler arms for these
// events are intentional no-ops.

/// Handle call hangup received — hide call UI.
pub(super) fn handle_call_hangup(
    window: &Weak<MainWindow>,
    call_id: String,
    peer_id: UserId,
    reason: chatter_core::services::gateway::CallHangupReason,
) {
    let weak = window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // A hangup names its call: a late one for an older call must not
            // dismiss or tear down the call on screen now.
            let ringing = w.get_incoming_call_visible()
                && w.get_incoming_call_data().call_id == call_id.as_str();
            let active =
                w.get_active_call_visible() && w.get_active_call_data().call_id == call_id.as_str();
            if !ringing && !active {
                tracing::info!(call_id = %call_id, "Hangup for a call not on screen; ignored");
                return;
            }
            if ringing {
                super::super::super::initial_data::hide_incoming_call(&w);
            }
            tracing::info!(
                call_id = %call_id,
                peer_id = %peer_id,
                reason = %reason,
                "Call ended"
            );
        }
    });
}
