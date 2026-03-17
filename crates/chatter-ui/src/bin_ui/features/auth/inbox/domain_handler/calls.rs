//! Signal call event handlers (non-anonymous).

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use crate::MainWindow;

/// Handle incoming call key offer.
pub(super) fn handle_call_key_offer(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    peer_id: UserId,
    ephemeral_public_key: [u8; 32],
    call_type: String,
    _channel_id: String,
    is_identity_verified: bool,
    identity_warning: String,
    caller_name: String,
    _timestamp: u64,
) {
    let weak = window.clone();

    // Store ephemeral key for later use
    ui_ctx.chat_state().set_incoming_call_ephemeral_pk(Some(ephemeral_public_key.to_vec()));

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Show incoming call modal
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

/// Handle call key answer received.
pub(super) fn handle_call_key_answer(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    peer_id: UserId,
    _ephemeral_public_key: [u8; 32],
) {
    let weak = window.clone();

    // Complete the call key exchange
    if let Some(_our_eph_sk) = ui_ctx.chat_state().take_outgoing_call_ephemeral_sk() {
        // Derive call key from ECDH
        // TODO: Implement x25519_ecdh or use call_key_provider
        // let shared = chatter_core::crypto::x25519_ecdh(&our_eph_sk, &ephemeral_public_key);
        let shared = [0u8; 32]; // Placeholder
        ui_ctx.chat_state().set_current_call_key(Some(shared.to_vec()));

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Update call state - callee answered
                // w.set_call_state("connecting".into());
                tracing::info!(
                    call_id = %call_id,
                    peer_id = %peer_id,
                    "Call key exchange complete - connecting"
                );
            }
        });
    }
}

/// Handle call hangup received.
pub(super) fn handle_call_hangup(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    peer_id: UserId,
    reason: String,
) {
    let weak = window.clone();

    // Clear call state
    ui_ctx.chat_state().clear_call_state();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Hide call modals
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

/// Handle group call key received.
pub(super) fn handle_group_call_key(
    ui_ctx: &Arc<ChatterClient>,
    call_id: String,
    sender_id: UserId,
    key_material: [u8; 32],
    key_epoch: u32,
    channel_id: ChannelId,
) {
    // Update group call key
    ui_ctx.chat_state().set_group_call_key(Some(key_material.to_vec()));

    tracing::debug!(
        call_id = %call_id,
        sender_id = %sender_id,
        channel_id = %channel_id,
        key_epoch,
        "Updated group call key"
    );
}
