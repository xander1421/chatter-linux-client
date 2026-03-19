//! Incoming call handling.
//!
//! Handles accepting and declining incoming calls.

use std::str::FromStr;
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::crypto::EphemeralCallKey;
use chatter_core::domain::ids::{CallId, UserId};
use crate::{ActiveCallData, CallParticipantData, MainWindow};

use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire incoming call accept/decline callbacks.
pub fn wire_incoming_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    let ctx_call_service = ui_ctx.call_service().clone();
    let ctx_session_service = ui_ctx.sessions().clone();
    let ctx_signal_service = ui_ctx.signal().clone();
    let ctx_anon_dm_service = ui_ctx.anon_dms().clone();
    let ctx_anon_group_service = ui_ctx.anon_groups().clone();

    // Accept call callback
    window.on_accept_incoming_call({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let call_service = ctx_call_service.clone();
        let session_service = ctx_session_service.clone();
        let signal_service = ctx_signal_service.clone();
        let anon_dm_service = ctx_anon_dm_service.clone();
        let anon_group_service = ctx_anon_group_service.clone();

        move |call_id| {
            let call_id_str = call_id.to_string();
            tracing::info!("Accept call: {}", call_id_str);

            // Read the is_anonymous_call flag from the incoming call data
            let is_anonymous = weak.upgrade()
                .map(|w| w.get_incoming_call_data().is_anonymous_call)
                .unwrap_or(false);

            if is_anonymous {
                // Handle anonymous call acceptance
                let weak = weak.clone();
                let anon_dm_service = anon_dm_service.clone();
                let anon_group_service = anon_group_service.clone();
                let call_service = call_service.clone();
                
                slint::spawn_local(async move {
                    tracing::info!("Accepting anonymous call: {}", call_id_str);

                    // Hide UI immediately to prevent double-click
                    if let Some(w) = weak.upgrade() {
                        w.set_incoming_call_visible(false);
                    }

                    tracing::info!("Accept: UI hidden, getting pending call state");
                    // Get stored pending call details from the service (consume to prevent re-accept)
                    let pending = match anon_dm_service.take_pending_anon_call(&call_id_str) {
                        Some(p) => p,
                        None => {
                            tracing::error!("No pending anonymous call state found for {}", call_id_str);
                            if let Some(w) = weak.upgrade() {
                                w.set_incoming_call_visible(false);
                            }
                            return;
                        }
                    };
                    
                    // Get a token for accepting the call
                    tracing::info!("Getting call room token for accept");
                    let token = match anon_dm_service.get_available_call_room_token() {
                        Ok(Some(t)) => {
                            tracing::info!("Got existing call room token, marking used");
                            let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                            t
                        }
                        Ok(None) => {
                            tracing::info!("No call room tokens available, requesting more");
                            // gRPC - run on tokio
                            let token_req = {
                                let svc = anon_dm_service.clone();
                                tokio::spawn(async move {
                                    tokio::time::timeout(
                                        std::time::Duration::from_secs(5),
                                        svc.request_call_room_tokens(5),
                                    ).await
                                }).await.unwrap()
                            };
                            match token_req {
                                Ok(Ok(count)) => {
                                    tracing::info!("Requested {} call room tokens", count);
                                    match anon_dm_service.get_available_call_room_token() {
                                        Ok(Some(t)) => {
                                            let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                            t
                                        }
                                        _ => {
                                            tracing::error!("No tokens available after request");
                                            return;
                                        }
                                    }
                                }
                                Ok(Err(e)) => {
                                    tracing::error!("Failed to request tokens: {}", e);
                                    return;
                                }
                                Err(_) => {
                                    tracing::error!("Timed out requesting call room tokens");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to get token: {}", e);
                            return;
                        }
                    };
                    
                    // Use our own display name (not the caller's)
                    let my_display_name = anon_dm_service.get_my_display_name().unwrap_or_else(|| "Anonymous".to_string());

                    // Send accept answer - route through DM or group service
                    // Check if conversation_id is an actual anon DM or a synthetic ID from a group
                    let is_group_call = anon_dm_service.get_conversation(&pending.conversation_id)
                        .ok().flatten().is_none();

                    if is_group_call {
                        // Route through anon group service
                        let group_id = match chatter_core::domain::anon_group::AnonGroupId::try_from_slice(
                            pending.conversation_id.as_bytes()
                        ) {
                            Some(id) => id,
                            None => {
                                tracing::error!("Invalid group conversation_id bytes (expected 16 bytes, got {})",
                                    pending.conversation_id.as_bytes().len());
                                if let Some(w) = weak.upgrade() {
                                    w.set_incoming_call_visible(false);
                                }
                                return;
                            }
                        };
                        // gRPC - run on tokio
                        let group_answer_result = {
                            let svc = anon_group_service.clone();
                            let gid = group_id;
                            let crid = pending.call_room_id;
                            let tok = token.clone();
                            let dn = my_display_name.clone();
                            tokio::spawn(async move {
                                svc.send_call_answer(&gid, &crid, true, Some(&tok), Some(&dn)).await
                            }).await.unwrap()
                        };
                        if let Err(e) = group_answer_result {
                            tracing::error!("Failed to send group call accept: {}", e);
                            if let Some(w) = weak.upgrade() {
                                w.set_incoming_call_visible(false);
                            }
                            return;
                        }
                    } else {
                        // Route through anon DM service (gRPC - run on tokio)
                        let dm_answer_result = {
                            let svc = anon_dm_service.clone();
                            let cid = pending.conversation_id;
                            let crid = pending.call_room_id;
                            let tok = token.clone();
                            let dn = my_display_name.clone();
                            tokio::spawn(async move {
                                svc.send_call_answer(&cid, &crid, true, Some(&tok), None, Some(&dn)).await
                            }).await.unwrap()
                        };
                        if let Err(e) = dm_answer_result {
                            tracing::error!("Failed to send call accept: {}", e);
                            if let Some(w) = weak.upgrade() {
                                w.set_incoming_call_visible(false);
                            }
                            return;
                        }
                    }

                    tracing::info!("Anonymous call accept sent, joining LiveKit room");

                    // Join the LiveKit room (gRPC - run on tokio)
                    let join_result = {
                        let svc = anon_dm_service.clone();
                        let rn = pending.room_name.clone();
                        let tok = token.clone();
                        let dn = my_display_name.clone();
                        let is_video = pending.is_video_call;
                        tokio::spawn(async move {
                            svc.join_anon_call_room(&rn, &tok, &dn, is_video).await
                        }).await.unwrap()
                    };
                    match join_result {
                        Ok(room_info) => {
                            tracing::info!(
                                "Joined anonymous call room with LiveKit token"
                            );
                            
                            // Connect to LiveKit using the room info
                            let service = match call_service.get() {
                                Some(svc) => svc.clone(),
                                None => {
                                    tracing::error!("CallService not initialized");
                                    if let Some(w) = weak.upgrade() {
                                        w.set_incoming_call_visible(false);
                                    }
                                    // Clear pending state
                                    anon_dm_service.clear_pending_anon_call(&call_id_str);
                                    return;
                                }
                            };
                            
                            // Create a dummy call ID and channel ID for anonymous calls
                            let call_id = chatter_core::domain::ids::CallId::new();
                            let channel_id = chatter_core::domain::ids::ChannelId::new();
                            
                            // Connect to LiveKit with the access token and E2EE key (network - run on tokio)
                            let connect_result = {
                                let svc = service.clone();
                                let lk_token = room_info.livekit_token.clone();
                                let lk_url = room_info.livekit_url.clone();
                                let enc_key = pending.encryption_key.clone();
                                let rn = pending.room_name.clone();
                                tokio::spawn(async move {
                                    svc.connect_with_token(&lk_token, &lk_url, enc_key, &rn, call_id, channel_id).await
                                }).await.unwrap()
                            };
                            match connect_result {
                                Ok(()) => {
                                    tracing::info!("Connected to LiveKit for anonymous call");
                                    
                                    // Publish microphone immediately after joining
                                    if let Err(e) = service.publish_microphone(None).await {
                                        tracing::error!("Failed to publish microphone: {}", e);
                                    } else {
                                        tracing::info!("Microphone published successfully");
                                    }
                                    
                                    // Update UI
                                    if let Some(w) = weak.upgrade() {
                                        w.set_incoming_call_visible(false);
                                        
                                        // Create participant data for UI
                                        let local_participant = CallParticipantData {
                                            user_id: "You".into(),
                                            display_name: "You".into(),
                                            avatar_url: "".into(),
                                            is_muted: false,
                                            is_video_enabled: pending.is_video_call,
                                            is_screen_sharing: false,
                                            is_speaking: false,
                                        };
                                        
                                        let remote_participant = CallParticipantData {
                                            user_id: "Anonymous".into(),
                                            display_name: pending.display_name.clone().into(),
                                            avatar_url: "".into(),
                                            is_muted: false,
                                            is_video_enabled: pending.is_video_call,
                                            is_screen_sharing: false,
                                            is_speaking: false,
                                        };
                                        
                                        // Derive verification emojis from shared E2EE key + room name
                                        let mut e2ee_key = [0u8; 32];
                                        if pending.encryption_key.len() >= 32 { e2ee_key.copy_from_slice(&pending.encryption_key[..32]); }
                                        let verification = chatter_core::services::CallVerification::derive_verification_emojis(
                                            &e2ee_key,
                                            &pending.room_name,
                                        );
                                        let (e1, e2, e3, e4) = chatter_core::services::CallVerification::to_slint_tuple(&verification);

                                        // Create call data for UI (use room_name as call_id so "anon-" prefix is preserved for cleanup)
                                        let call_data = ActiveCallData {
                                            call_id: pending.room_name.clone().into(),
                                            channel_id: channel_id.to_string().into(),
                                            title: "Anonymous Call".into(),
                                            is_video_call: pending.is_video_call,
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
                                            verification_emoji_1: e1.into(),
                                            verification_emoji_2: e2.into(),
                                            verification_emoji_3: e3.into(),
                                            verification_emoji_4: e4.into(),
                                        };
                                        
                                        w.set_active_call_data(call_data);
                                        w.set_call_is_muted(false);
                                        w.set_call_is_video_enabled(pending.is_video_call);
                                        w.set_call_is_screen_sharing(false);
                                        w.set_active_call_visible(true);
                                        
                                        // Start call duration timer
                                        super::timers::start_call_timer(&w, None);
                                        
                                        // Start video frame polling timer
                                        super::timers::start_video_frame_timer(&w, &service);
                                        
                                        // Publish camera if video call
                                        if pending.is_video_call {
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
                                    
                                    // Clear pending state on successful connection
                                    anon_dm_service.clear_pending_anon_call(&call_id_str);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to connect to LiveKit: {}", e);
                                    if let Some(w) = weak.upgrade() {
                                        w.set_incoming_call_visible(false);
                                    }
                                    // Clear pending state
                                    anon_dm_service.clear_pending_anon_call(&call_id_str);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to join anonymous call room: {}", e);
                            if let Some(w) = weak.upgrade() {
                                w.set_incoming_call_visible(false);
                            }
                            // Clear pending state
                            anon_dm_service.clear_pending_anon_call(&call_id_str);
                        }
                    }
                }).unwrap();
                return;
            }

            let weak = weak.clone();
            let call_service = call_service.clone();
            let session_service = session_service.clone();
            let signal_service = signal_service.clone();
            let ui_ctx = ui_ctx.clone();

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
                // 3. Delegate identity lookup + key derivation to Signal service
                // 4. Send answer with our ephemeral public key
                // 5. Join the call with derived key
                // =============================================================

                // Generate our ephemeral keypair
                let our_ephemeral = EphemeralCallKey::generate();
                let our_ephemeral_pk = our_ephemeral.public_key_bytes();

                // Delegate all crypto to the Signal service (actor call - run on tokio)
                let encryption_key = {
                    let actor = signal_service.actor.clone();
                    let priv_key = our_ephemeral.private_key_bytes();
                    let cid_str = call_id_str.clone();
                    match tokio::spawn(async move {
                        actor.complete_call_key_exchange(
                            priv_key,
                            caller_ephemeral_pk,
                            caller_id,
                            cid_str,
                            false, // we_are_caller = false (we're the callee)
                        ).await
                    }).await.unwrap() {
                        Ok(key) => key,
                        Err(e) => {
                            tracing::error!("Failed to derive call key: {}", e);
                            return;
                        }
                    }
                };

                tracing::info!(
                    call_id = %call_id_str,
                    caller_id = %caller_id,
                    "Derived call key, sending answer"
                );

                // Send answer with our ephemeral public key via CallSignalingService (gRPC - run on tokio)
                let answer_result = {
                    let ctx = ui_ctx.clone();
                    let cid = caller_id;
                    let call_str = call_id_str.clone();
                    let eph_pk = our_ephemeral_pk;
                    tokio::spawn(async move {
                        ctx.call_signaling().send_call_answer(&cid, &call_str, eph_pk).await
                    }).await.unwrap()
                };
                if let Err(e) = answer_result {
                    tracing::error!(caller_id = %caller_id, error = %e, "Failed to send call key answer");
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
                            let verification = chatter_core::services::CallVerification::derive_verification_emojis(
                                &encryption_key,
                                &call.id.to_string(),
                            );
                            let (e1, e2, e3, e4) = chatter_core::services::CallVerification::to_slint_tuple(&verification);

                            // Create call data for UI
                            let call_data = ActiveCallData {
                                call_id: call.id.to_string().into(),
                                channel_id: call.channel_id.to_string().into(),
                                title: "Call".into(),
                                is_video_call,
                                is_anonymous_call: false,
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

        let is_anonymous = weak.upgrade()
            .map(|w| w.get_incoming_call_data().is_anonymous_call)
            .unwrap_or(false);

        if is_anonymous {
            let weak = weak.clone();
            let ui_ctx = ui_ctx_decline.clone();
            
            // Hide the modal immediately
            if let Some(w) = weak.upgrade() {
                w.set_incoming_call_visible(false);
            }
            
            // gRPC calls only, no UI updates needed - run entirely on tokio
            tokio::spawn(async move {
                // Get stored offer details from pending state (consume it since we're declining)
                if let Some(pending) = ui_ctx.anon_dms().take_pending_anon_call(&call_id_str) {
                    // Route decline through DM or group service
                    let is_group_call = ui_ctx.anon_dms().get_conversation(&pending.conversation_id)
                        .ok().flatten().is_none();

                    if is_group_call {
                        let group_id = chatter_core::domain::anon_group::AnonGroupId::try_from_slice(
                            pending.conversation_id.as_bytes()
                        ).expect("conversation_id bytes are valid");
                        if let Err(e) = ui_ctx.anon_groups().send_call_answer(
                            &group_id,
                            &pending.call_room_id,
                            false,
                            None,
                            Some(&pending.display_name),
                        ).await {
                            tracing::error!("Failed to send group call decline: {}", e);
                        } else {
                            tracing::info!("Group call decline sent for {}", call_id_str);
                        }
                    } else {
                        if let Err(e) = ui_ctx.anon_dms().send_call_answer(
                            &pending.conversation_id,
                            &pending.call_room_id,
                            false,
                            None,
                            None,
                            Some(&pending.display_name),
                        ).await {
                            tracing::error!("Failed to send anonymous call decline: {}", e);
                        } else {
                            tracing::info!("Anonymous call decline sent for {}", call_id_str);
                        }
                    }
                } else {
                    tracing::warn!("No pending anonymous call state found for decline: {}", call_id_str);
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
