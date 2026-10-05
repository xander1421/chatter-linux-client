//! Anonymous call event handlers (DM and group calls).

use std::sync::Arc;

use slint::Weak;

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::UserId;

/// Handle anonymous call offer received.
///
/// Deprecated inside this path: the CallOrchestrator bridge in core drives
/// incoming-call presentation now. Pending-call state storage still happens
/// in core `EventHandlers`; this stub exists only until the dispatcher stops
/// forwarding `AnonCallOfferReceived` to the UI handlers explicitly.
pub(super) fn handle_anon_call_offer(
    _window: &Weak<MainWindow>,
    _ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    sender_id: UserId,
    _call_room_id: Vec<u8>,
    room_name: String,
    call_type: i32,
    caller_display_name: String,
    _call_encryption_key: Vec<u8>,
) {
    tracing::trace!(
        %conversation_id,
        %sender_id,
        %room_name,
        call_type,
        caller = %caller_display_name,
        "AnonCallOfferReceived: orchestrator bridge drives the modal now"
    );
}

// NOTE: anon call answer handling now lives entirely inside the call
// orchestrator bridge (see `chatter_core::services::call_worker::
// spawn_anon_call_answer_task`). The UI receives the resulting state
// transitions as `AnonCallConnected` / `ActiveEnded` orchestrator events,
// handled in `features::calls::incoming`.

/// Handle anonymous group call offer received.
pub(super) fn handle_anon_group_call_offer(
    window: &Weak<MainWindow>,
    _ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    sender_id: UserId,
    _call_room_id: Vec<u8>,
    room_name: String,
    call_type: i32,
    caller_display_name: String,
    _call_encryption_key: Vec<u8>,
) {
    // Pending call state is stored by core EventHandlers on AnonGroupCallOfferReceived.
    // UI only shows the incoming call modal.

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let display_name = if caller_display_name.is_empty() {
                "Anonymous"
            } else {
                &caller_display_name
            };

            let call_type_str = if call_type == 2 { "video" } else { "voice" };

            super::super::super::initial_data::show_incoming_call(
                &w,
                &room_name,
                &sender_id.to_string(),
                display_name,
                call_type_str,
                true,
                "",
                true,
                None, // anonymous caller — no avatar, show initials
            );

            tracing::info!(
                group_id = %group_id,
                room_name = %room_name,
                call_type = call_type,
                "Showing incoming anonymous group call UI"
            );
        }
    });
}
