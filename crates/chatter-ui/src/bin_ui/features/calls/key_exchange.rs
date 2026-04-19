//! Ephemeral key exchange for call encryption.
//!
//! DM call key exchange state and crypto are fully managed by core
//! (CallSignalingService). This module provides the UI-side handler
//! that delegates to core and updates Slint properties from the result.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use crate::MainWindow;

use super::timers::{start_call_timer, start_video_frame_timer};

/// Handle receiving a call key answer (peer's ephemeral key).
///
/// Delegates to core's complete_dm_call() which handles:
/// 1. Taking pending call state
/// 2. Verifying call ID match
/// 3. ECDH + identity lookup + HKDF (via Signal actor)
/// 4. Updating LiveKit encryption key
/// 5. Deriving verification emojis
///
/// This function only handles UI updates (Slint properties, timers, media).
/// Must be called on the UI thread.
pub async fn handle_call_key_answer(
    weak_window: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    peer_ephemeral_pk: [u8; 32],
    call_id: String,
    peer_id: UserId,
) {
    tracing::info!(
        call_id = %call_id,
        peer_id = %peer_id,
        "Processing call key answer via core"
    );

    // Delegate all crypto and state management to core (gRPC - run on tokio)
    let result = {
        let ctx = ui_ctx.clone();
        let cid = call_id.clone();
        let pid = peer_id;
        tokio::spawn(async move {
            ctx.complete_dm_call(peer_ephemeral_pk, &cid, &pid).await
        }).await.unwrap()
    };

    let setup = match result {
        Ok(setup) => setup,
        Err(e) => {
            tracing::error!(call_id = %call_id, error = %e, "Failed to complete DM call");
            return;
        }
    };

    tracing::info!(
        call_id = %call_id,
        "DM call completed, updating UI"
    );

    // Media is auto-published by the call worker after connect — no UI-side
    // publish needed.

    // Update UI with verification emojis from core result
    let shared_frames = ui_ctx.call_video_frames();
    if let Some(w) = weak_window.upgrade() {
        let mut call_data = w.get_active_call_data();
        call_data.verification_emoji_1 = setup.verification_emojis[0].clone().into();
        call_data.verification_emoji_2 = setup.verification_emojis[1].clone().into();
        call_data.verification_emoji_3 = setup.verification_emojis[2].clone().into();
        call_data.verification_emoji_4 = setup.verification_emojis[3].clone().into();
        let current_title = call_data.title.to_string();
        if current_title == "Calling..." {
            call_data.title = "Call".into();
        }
        w.set_active_call_data(call_data);

        start_call_timer(&w, None);
        if let Some(frames) = shared_frames {
            start_video_frame_timer(&w, frames);
        }
    }
}
