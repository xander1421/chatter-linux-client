//! Incoming call handling.
//!
//! Handles accepting and declining incoming calls.

use std::str::FromStr;
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use crate::{ActiveCallData, CallParticipantData, MainWindow};

use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire incoming call accept/decline callbacks.
pub fn wire_incoming_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    let ctx_call_service = ui_ctx.call_service().clone();

    // Accept call callback
    window.on_accept_incoming_call({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let call_service = ctx_call_service.clone();

        move |call_id| {
            let call_id_str = call_id.to_string();
            tracing::info!("Accept call: {}", call_id_str);

            // Read the is_anonymous_call flag from the incoming call data
            let is_anonymous = weak.upgrade()
                .map(|w| w.get_incoming_call_data().is_anonymous_call)
                .unwrap_or(false);

            if is_anonymous {
                // Delegate to core — handles token, accept, room join, LiveKit connect
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                let call_service = call_service.clone();

                slint::spawn_local(async move {
                    tracing::info!("Accepting anonymous call: {}", call_id_str);

                    // Hide UI immediately to prevent double-click
                    if let Some(w) = weak.upgrade() {
                        w.set_incoming_call_visible(false);
                    }

                    let result = {
                        let ctx = ui_ctx.clone();
                        let room = call_id_str.clone();
                        tokio::spawn(async move {
                            ctx.accept_anon_call(&room).await
                        }).await.unwrap()
                    };

                    match result {
                        Ok(setup) => {
                            tracing::info!(room_name = %setup.room_name, "Anonymous call accepted via core");

                            // Get call service for media publishing
                            let service = match call_service.get() {
                                Some(svc) => svc.clone(),
                                None => {
                                    tracing::error!("CallService not initialized");
                                    return;
                                }
                            };

                            // Publish microphone immediately
                            if let Err(e) = service.publish_microphone(None).await {
                                tracing::error!("Failed to publish microphone: {}", e);
                            }

                            // Update UI
                            if let Some(w) = weak.upgrade() {
                                w.set_incoming_call_visible(false);

                                let local_participant = CallParticipantData {
                                    user_id: "You".into(),
                                    display_name: "You".into(),
                                    avatar_url: "".into(),
                                    is_muted: false,
                                    is_video_enabled: setup.is_video,
                                    is_screen_sharing: false,
                                    is_speaking: false,
                                };

                                let remote_participant = CallParticipantData {
                                    user_id: "Anonymous".into(),
                                    display_name: setup.peer_display_name.clone().into(),
                                    avatar_url: "".into(),
                                    is_muted: false,
                                    is_video_enabled: setup.is_video,
                                    is_screen_sharing: false,
                                    is_speaking: false,
                                };

                                let call_data = ActiveCallData {
                                    call_id: setup.room_name.clone().into(),
                                    channel_id: setup.channel_id.to_string().into(),
                                    title: "Anonymous Call".into(),
                                    is_video_call: setup.is_video,
                                    is_anonymous_call: true,
                                    is_encrypted: true,
                                    is_identity_verified: true,
                                    identity_warning: "".into(),
                                    participant_count: 2,
                                    duration_seconds: 0,
                                    participants: ModelRc::new(VecModel::from(vec![
                                        remote_participant,
                                        local_participant,
                                    ])),
                                    verification_emoji_1: setup.verification_emojis[0].clone().into(),
                                    verification_emoji_2: setup.verification_emojis[1].clone().into(),
                                    verification_emoji_3: setup.verification_emojis[2].clone().into(),
                                    verification_emoji_4: setup.verification_emojis[3].clone().into(),
                                };

                                w.set_active_call_data(call_data);
                                w.set_call_is_muted(false);
                                w.set_call_is_video_enabled(setup.is_video);
                                w.set_call_is_screen_sharing(false);
                                w.set_active_call_visible(true);

                                super::timers::start_call_timer(&w, None);
                                super::timers::start_video_frame_timer(&w, &service);

                                if setup.is_video {
                                    let service = service.clone();
                                    slint::spawn_local(async move {
                                        if let Err(e) = service.publish_camera(None).await {
                                            tracing::error!("Failed to publish camera: {}", e);
                                        }
                                    }).unwrap();
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to accept anonymous call: {}", e);
                            if let Some(w) = weak.upgrade() {
                                w.set_incoming_call_visible(false);
                            }
                        }
                    }
                }).unwrap();
                return;
            }

            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            let call_service = call_service.clone();

            slint::spawn_local(async move {
                // Get incoming call data to extract caller's ephemeral key and caller ID
                let (caller_ephemeral_b64, is_video_call, caller_name, call_start_timestamp_ns, caller_id_str) = if let Some(w) = weak.upgrade() {
                    let data = w.get_incoming_call_data();
                    // Parse timestamp from string (nanoseconds since epoch)
                    let timestamp_ns = data.call_start_timestamp_ns.to_string()
                        .parse::<u64>()
                        .ok();
                    (
                        data.encryption_key.to_string(), // Contains caller's ephemeral public key
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

                // Get current user ID
                let user_id = match ui_ctx.session_current_user_id() {
                    Some(id) => id,
                    None => {
                        tracing::error!("No user ID available");
                        return;
                    }
                };

                // Delegate all crypto + signaling + joining to core
                let result = {
                    let ctx = ui_ctx.clone();
                    let cid = call_id_str.clone();
                    tokio::spawn(async move {
                        ctx.accept_dm_call(caller_ephemeral_pk, &cid, &caller_id, user_id).await
                    }).await.unwrap()
                };

                match result {
                    Ok(setup) => {
                        tracing::info!(call_id = %call_id_str, "DM call accepted via core");

                        // Get the call service for media publishing
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
                                user_id: caller_id.to_string().into(),
                                display_name: caller_name.into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: is_video_call,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            // Use verification emojis from core result
                            let call_data = ActiveCallData {
                                call_id: setup.call_id.to_string().into(),
                                channel_id: setup.channel_id.to_string().into(),
                                title: "Call".into(),
                                is_video_call,
                                is_anonymous_call: false,
                                is_encrypted: true,
                                is_identity_verified: false,
                                identity_warning: "".into(),
                                participant_count: 2, // We + caller
                                duration_seconds: 0,
                                participants: ModelRc::new(VecModel::from(vec![caller_participant, local_participant])),
                                verification_emoji_1: setup.verification_emojis[0].clone().into(),
                                verification_emoji_2: setup.verification_emojis[1].clone().into(),
                                verification_emoji_3: setup.verification_emojis[2].clone().into(),
                                verification_emoji_4: setup.verification_emojis[3].clone().into(),
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
                        tracing::error!("Failed to accept DM call: {}", e);
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

        let is_anonymous = weak.upgrade()
            .map(|w| w.get_incoming_call_data().is_anonymous_call)
            .unwrap_or(false);

        if is_anonymous {
            let ui_ctx = ui_ctx_decline.clone();

            // Hide the modal immediately
            if let Some(w) = weak.upgrade() {
                w.set_incoming_call_visible(false);
            }

            // Delegate decline to core — handles pending state, send decline, call history
            tokio::spawn(async move {
                if let Err(e) = ui_ctx.decline_anon_call(&call_id_str).await {
                    tracing::error!("Failed to decline anonymous call: {}", e);
                } else {
                    tracing::info!("Anonymous call declined for {}", call_id_str);
                }
            });
            return;
        }

        let weak = weak.clone();
        let ui_ctx = ui_ctx_decline.clone();

        // Hide the modal immediately
        if let Some(w) = weak.upgrade() {
            w.set_incoming_call_visible(false);
        }

        // Notify server that call was declined (gRPC - run on tokio)
        tokio::spawn(async move {
            // Leave call signals decline (we never joined, so this just marks our intent)
            if let Err(e) = ui_ctx.decline_call(&call_id_str).await {
                // This error is expected if we never joined - just log at debug level
                tracing::debug!("Decline call notification: {}", e);
            } else {
                tracing::info!("Call decline notification sent for {}", call_id_str);
            }
        });
    });
}
