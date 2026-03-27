//! Ephemeral key exchange for call encryption.
//!
//! DM call key exchange state and crypto are fully managed by core
//! (CallSignalingService). This module provides the UI-side handler
//! that delegates to core and updates Slint properties from the result.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use chatter_core::services::CallWorkerHandle;
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
    call_service: &CallWorkerHandle,
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

    // Publish media (gRPC - run on tokio)
    let call_service_clone = call_service.clone();
    let is_video_call = weak_window.upgrade()
        .map(|w| w.get_active_call_data().is_video_call)
        .unwrap_or(false);
    tokio::spawn(async move {
        // Publish microphone
        if let Err(e) = call_service_clone.publish_microphone(None).await {
            tracing::error!("Failed to publish microphone: {}", e);
        }

        // Publish camera if video call
        if is_video_call {
            if let Err(e) = call_service_clone.publish_camera(None).await {
                tracing::error!("Failed to publish camera: {}", e);
            }
        }
    });

    // Update UI with verification emojis from core result
    if let Some(w) = weak_window.upgrade() {
        let mut call_data = w.get_active_call_data();
        call_data.verification_emoji_1 = setup.verification_emojis[0].clone().into();
        call_data.verification_emoji_2 = setup.verification_emojis[1].clone().into();
        call_data.verification_emoji_3 = setup.verification_emojis[2].clone().into();
        call_data.verification_emoji_4 = setup.verification_emojis[3].clone().into();
        // Update title from "Calling..." to the call type
        // (get from the active call data since core doesn't track UI title)
        let current_title = call_data.title.to_string();
        if current_title == "Calling..." {
            call_data.title = "Call".into();
        }
        w.set_active_call_data(call_data);

        // Start call timer now that we're connected
        start_call_timer(&w, None);
        start_video_frame_timer(&w, call_service);
    }
}
