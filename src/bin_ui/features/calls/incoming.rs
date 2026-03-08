//! Incoming call handling.
//!
//! Handles accepting and declining incoming calls.

use std::str::FromStr;
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};
use libsignal_protocol::IdentityKey;

use chatter_linux::app::UIContext;
use chatter_linux::crypto::{EphemeralCallKey, derive_call_key_ephemeral};
use chatter_linux::domain::ids::{CallId, UserId};
use crate::{ActiveCallData, CallParticipantData, MainWindow};

use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire incoming call accept/decline callbacks.
pub fn wire_incoming_call(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak = window.as_weak();
    let ctx_call_service = ui_ctx.call_service.clone();
    let ctx_session_service = ui_ctx.session_service.clone();
    let ctx_message_service = ui_ctx.message_service.clone();
    let ctx_mailbox_manager = ui_ctx.mailbox_manager.clone();
    let ctx_identity_manager = ui_ctx.identity_manager.clone();
    let ctx_key_store = ui_ctx.key_store.clone();

    // Accept call callback
    window.on_accept_incoming_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let session_service = ctx_session_service.clone();
        let message_service = ctx_message_service.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let identity_manager = ctx_identity_manager.clone();
        let key_store = ctx_key_store.clone();

        move |call_id| {
            let call_id_str = call_id.to_string();
            tracing::info!("Accept call: {}", call_id_str);

            let weak = weak.clone();
            let call_service = call_service.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let identity_manager = identity_manager.clone();
            let key_store = key_store.clone();

            slint::spawn_local(async move {
                // Get incoming call data to extract caller's ephemeral key and caller ID
                let (caller_ephemeral_b64, is_video_call, caller_name, call_start_timestamp_ns, caller_id_str) = if let Some(w) = weak.upgrade() {
                    let data = w.get_incoming_call_data();
                    // Parse timestamp from string (nanoseconds since epoch)
                    let timestamp_ns = data.call_start_timestamp_ns.to_string()
                        .parse::<u64>()
                        .ok();
                    (
                        data.encryption_key.to_string(), // Now contains caller's ephemeral public key
                        data.is_video_call,
                        data.caller_name.to_string(),
                        timestamp_ns,
                        data.caller_id.to_string(),
                    )
                } else {
                    return;
                };

                // Parse caller ID
                let caller_id = match UserId::from_str(&caller_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid caller ID: {}", e);
                        return;
                    }
                };

                // Decode caller's ephemeral public key from base64
                let caller_ephemeral_pk: [u8; 32] = match base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &caller_ephemeral_b64,
                ) {
                    Ok(bytes) if bytes.len() == 32 => {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&bytes);
                        key
                    }
                    Ok(bytes) => {
                        tracing::error!("Invalid caller ephemeral key length: {} (expected 32)", bytes.len());
                        return;
                    }
                    Err(e) => {
                        tracing::error!("Failed to decode caller ephemeral key: {}", e);
                        return;
                    }
                };

                // =============================================================
                // Ephemeral Key Exchange (Callee Side)
                // =============================================================
                // 1. Generate our ephemeral keypair
                // 2. Compute ECDH(our_ephemeral, caller_ephemeral)
                // 3. Derive call key with identity binding
                // 4. Send answer with our ephemeral public key
                // 5. Join the call with derived key
                // =============================================================

                // Generate our ephemeral keypair
                let our_ephemeral = EphemeralCallKey::generate();
                let our_ephemeral_pk = our_ephemeral.public_key_bytes();

                // Compute ECDH
                let ephemeral_shared = our_ephemeral.diffie_hellman(&caller_ephemeral_pk);

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

                // Get caller's identity key from storage (address format: "user_id:device_id")
                let caller_address = format!("{}:1", caller_id);
                let caller_identity = match key_store.get_identity(&caller_address) {
                    Ok(Some(bytes)) => {
                        match IdentityKey::decode(&bytes) {
                            Ok(key) => key,
                            Err(e) => {
                                tracing::error!("Failed to decode caller identity key: {}", e);
                                return;
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::error!("No identity key stored for caller {}", caller_id);
                        return;
                    }
                    Err(e) => {
                        tracing::error!("Failed to get caller identity key: {}", e);
                        return;
                    }
                };

                // Derive call key (we are callee, so we_are_caller = false)
                let encryption_key = match derive_call_key_ephemeral(
                    &ephemeral_shared,
                    &our_identity,
                    &caller_identity,
                    &call_id_str,
                    false, // we_are_caller = false (we're the callee)
                ) {
                    Ok(key) => key,
                    Err(e) => {
                        tracing::error!("Failed to derive call key: {}", e);
                        return;
                    }
                };

                tracing::info!(
                    call_id = %call_id_str,
                    caller_id = %caller_id,
                    "Derived call key, sending answer"
                );

                // Send answer with our ephemeral public key
                let answer_content = chatter_linux::services::build_call_key_answer(
                    &call_id_str,
                    our_ephemeral_pk,
                );

                if let Err(e) = message_service
                    .send_dm(&caller_id, answer_content, &mailbox_manager)
                    .await
                {
                    tracing::error!(
                        caller_id = %caller_id,
                        error = %e,
                        "Failed to send call key answer"
                    );
                    // Continue anyway - caller might timeout, but we can still try to connect
                }

                // our_ephemeral is dropped here, zeroizing the private key
                drop(our_ephemeral);
                tracing::debug!("Ephemeral private key zeroized");

                // Parse call ID
                let call_id = match CallId::from_str(&call_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid call ID: {}", e);
                        return;
                    }
                };

                // Get current user ID
                let user_id = match session_service.current_user_id() {
                    Some(id) => id,
                    None => {
                        tracing::error!("No user ID available");
                        return;
                    }
                };

                // Get the CallService from context (same instance used for room events)
                let service = match call_service.get() {
                    Some(svc) => svc.clone(),
                    None => {
                        tracing::error!("CallService not initialized");
                        if let Some(w) = weak.upgrade() {
                            w.set_incoming_call_visible(false);
                        }
                        return;
                    }
                };

                match service.join_call(call_id, &encryption_key, user_id).await {
                    Ok(call) => {
                        tracing::info!("Joined call: {:?}", call.id);

                        // Publish microphone immediately after joining
                        if let Err(e) = service.publish_microphone(None).await {
                            tracing::error!("Failed to publish microphone: {}", e);
                        } else {
                            tracing::info!("Microphone published successfully");
                        }

                        // Update UI
                        if let Some(w) = weak.upgrade() {
                            w.set_incoming_call_visible(false);

                            // Create local participant
                            let local_participant = CallParticipantData {
                                user_id: user_id.to_string().into(),
                                display_name: "You".into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: is_video_call,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            // Create caller participant
                            let caller_participant = CallParticipantData {
                                user_id: call.initiator_id.to_string().into(),
                                display_name: caller_name.into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: is_video_call,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            // Derive verification emojis for secure call verification
                            let verification = chatter_linux::services::CallVerification::derive_verification_emojis(
                                &encryption_key,
                                &call.id.to_string(),
                            );
                            let (e1, e2, e3, e4) = chatter_linux::services::CallVerification::to_slint_tuple(&verification);

                            // Create call data for UI
                            let call_data = ActiveCallData {
                                call_id: call.id.to_string().into(),
                                channel_id: call.channel_id.to_string().into(),
                                title: "Call".into(),
                                is_video_call,
                                is_encrypted: true,
                                is_identity_verified: false,
                                identity_warning: "".into(),
                                participant_count: 2, // We + caller
                                duration_seconds: 0,
                                participants: ModelRc::new(VecModel::from(vec![caller_participant, local_participant])),
                                verification_emoji_1: e1.into(),
                                verification_emoji_2: e2.into(),
                                verification_emoji_3: e3.into(),
                                verification_emoji_4: e4.into(),
                            };

                            w.set_active_call_data(call_data);
                            w.set_call_is_muted(false);
                            w.set_call_is_video_enabled(is_video_call);
                            w.set_call_is_screen_sharing(false);
                            w.set_active_call_visible(true);

                            // Start call duration timer (receiver uses caller's timestamp for sync)
                            start_call_timer(&w, call_start_timestamp_ns);

                            // Start video frame polling timer
                            start_video_frame_timer(&w, &service);

                            // Publish camera if video call
                            if is_video_call {
                                let service = service.clone();
                                slint::spawn_local(async move {
                                    if let Err(e) = service.publish_camera(None).await {
                                        tracing::error!("Failed to publish camera: {}", e);
                                    } else {
                                        tracing::info!("Camera published successfully");
                                    }
                                }).unwrap();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to join call: {}", e);
                        if let Some(w) = weak.upgrade() {
                            w.set_incoming_call_visible(false);
                        }
                    }
                }
            }).unwrap();
        }
    });

    let weak = window.as_weak();
    let ui_ctx_decline = ui_ctx.clone();

    // Decline call callback
    window.on_decline_incoming_call(move |call_id| {
        let call_id_str = call_id.to_string();
        tracing::info!("Decline call: {}", call_id_str);

        let weak = weak.clone();
        let ui_ctx = ui_ctx_decline.clone();

        // Hide the modal immediately
        if let Some(w) = weak.upgrade() {
            w.set_incoming_call_visible(false);
        }

        // Notify server that call was declined by attempting to leave
        slint::spawn_local(async move {
            // Leave call signals decline (we never joined, so this just marks our intent)
            if let Err(e) = ui_ctx.decline_call(&call_id_str).await {
                // This error is expected if we never joined - just log at debug level
                tracing::debug!("Decline call notification: {}", e);
            } else {
                tracing::info!("Call decline notification sent for {}", call_id_str);
            }
        }).unwrap();
    });
}
