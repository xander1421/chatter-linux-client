//! Ephemeral key exchange for call encryption.
//!
//! Manages outgoing call state and key derivation for E2EE calls.

use std::cell::RefCell;
use std::sync::Arc;

use libsignal_protocol::IdentityKey;

use chatter_linux::crypto::{EphemeralCallKey, derive_call_key_ephemeral};
use chatter_linux::domain::ids::{CallId, ChannelId, UserId};
use chatter_linux::services::CallWorkerHandle;
use crate::MainWindow;

use super::timers::{start_call_timer, start_video_frame_timer};

thread_local! {
    /// Pending outgoing call state (waiting for answer with peer's ephemeral key).
    pub(super) static PENDING_OUTGOING_CALL: RefCell<Option<PendingOutgoingCall>> = const { RefCell::new(None) };
}

/// State for an outgoing call waiting for the peer's ephemeral key answer.
pub struct PendingOutgoingCall {
    /// Call ID.
    pub call_id: CallId,
    /// Channel ID (stored for potential future use, e.g., call history).
    #[allow(dead_code)]
    pub channel_id: ChannelId,
    /// Our ephemeral X25519 keypair (private key zeroized on drop).
    pub ephemeral_key: EphemeralCallKey,
    /// Peer's user ID (stored for verification, used via call_id matching).
    #[allow(dead_code)]
    pub peer_id: UserId,
    /// Call type string.
    pub call_type: String,
    /// Whether this is a video call.
    pub is_video_call: bool,
}

/// Store a pending outgoing call.
pub fn set_pending_call(call: PendingOutgoingCall) {
    PENDING_OUTGOING_CALL.with(|pending| {
        *pending.borrow_mut() = Some(call);
    });
}

/// Clear any pending outgoing call state.
pub fn clear_pending_call() {
    PENDING_OUTGOING_CALL.with(|pending| {
        *pending.borrow_mut() = None;
    });
}

/// Handle receiving a call key answer (peer's ephemeral key).
///
/// This completes the caller's side of the ephemeral key exchange:
/// 1. Compute ECDH(our_ephemeral, peer_ephemeral)
/// 2. Derive call key with identity binding
/// 3. Update LiveKit encryption key
/// 4. Start call timers and publish media
///
/// Must be called on the UI thread.
pub fn handle_call_key_answer(
    weak_window: &slint::Weak<MainWindow>,
    call_service: &CallWorkerHandle,
    identity_manager: &Arc<chatter_linux::crypto::IdentityKeyManager>,
    key_store: &Arc<chatter_linux::storage::KeyStore>,
    peer_ephemeral_pk: [u8; 32],
    call_id: String,
    peer_id: UserId,
) {
    // Take the pending call state
    let pending = PENDING_OUTGOING_CALL.with(|pending| pending.borrow_mut().take());

    let pending = match pending {
        Some(p) => p,
        None => {
            tracing::warn!(
                call_id = %call_id,
                "Received call answer but no pending outgoing call"
            );
            return;
        }
    };

    // Verify this answer is for our pending call
    if pending.call_id.to_string() != call_id {
        tracing::warn!(
            expected = %pending.call_id,
            received = %call_id,
            "Call ID mismatch in answer"
        );
        // Put it back
        PENDING_OUTGOING_CALL.with(|p| {
            *p.borrow_mut() = Some(pending);
        });
        return;
    }

    tracing::info!(
        call_id = %call_id,
        peer_id = %peer_id,
        "Received call answer, deriving call key"
    );

    // Compute ECDH(our_ephemeral, peer_ephemeral)
    let ephemeral_shared = pending.ephemeral_key.diffie_hellman(&peer_ephemeral_pk);

    // Get identity keys for HKDF binding
    let our_identity = match identity_manager.get_identity() {
        Ok(Some((_reg_id, identity))) => identity,
        Ok(None) => {
            tracing::error!("No local identity key available");
            return;
        }
        Err(e) => {
            tracing::error!("Failed to get our identity key: {}", e);
            return;
        }
    };

    // Get peer's identity key from storage (address format: "user_id:device_id")
    let peer_address = format!("{}:1", peer_id);
    let peer_identity = match key_store.get_identity(&peer_address) {
        Ok(Some(bytes)) => {
            match IdentityKey::decode(&bytes) {
                Ok(key) => key,
                Err(e) => {
                    tracing::error!("Failed to decode peer identity key: {}", e);
                    return;
                }
            }
        }
        Ok(None) => {
            tracing::error!("No identity key stored for peer {}", peer_id);
            return;
        }
        Err(e) => {
            tracing::error!("Failed to get peer identity key: {}", e);
            return;
        }
    };

    // Derive call key with identity binding (we are caller)
    let call_key = match derive_call_key_ephemeral(
        &ephemeral_shared,
        &our_identity,
        &peer_identity,
        &call_id,
        true, // we_are_caller
    ) {
        Ok(key) => key,
        Err(e) => {
            tracing::error!("Failed to derive call key: {}", e);
            return;
        }
    };

    tracing::info!(
        call_id = %call_id,
        "Call key derived successfully, updating LiveKit encryption"
    );

    // Update LiveKit encryption key and publish media
    let call_service_clone = call_service.clone();
    let call_key_clone = call_key;
    let is_video_call = pending.is_video_call;
    tokio::spawn(async move {
        let success = call_service_clone.set_encryption_key(call_key_clone.to_vec()).await;
        if success {
            tracing::info!("Call encryption key updated");
        } else {
            tracing::error!("Failed to set call encryption key");
        }

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

    // Update UI with verification emojis
    if let Some(w) = weak_window.upgrade() {
        let verification = chatter_linux::services::CallVerification::derive_verification_emojis(
            &call_key,
            &call_id,
        );
        let (e1, e2, e3, e4) = chatter_linux::services::CallVerification::to_slint_tuple(&verification);

        // Update verification emojis in active call data
        let mut call_data = w.get_active_call_data();
        call_data.verification_emoji_1 = e1.into();
        call_data.verification_emoji_2 = e2.into();
        call_data.verification_emoji_3 = e3.into();
        call_data.verification_emoji_4 = e4.into();
        call_data.title = pending.call_type.clone().into();
        w.set_active_call_data(call_data);

        // Start call timer now that we're connected
        start_call_timer(&w, None);
        start_video_frame_timer(&w, call_service);
    }

    // ephemeral_key is dropped here, zeroizing the private key
    tracing::debug!("Ephemeral private key zeroized");
}
