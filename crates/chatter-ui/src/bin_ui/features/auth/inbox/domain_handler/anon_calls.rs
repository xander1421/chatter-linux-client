//! Anonymous call event handlers (DM and group calls).

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::UserId;
use crate::MainWindow;

/// Handle anonymous call offer received.
pub(super) fn handle_anon_call_offer(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    sender_id: UserId,
    call_room_id: Vec<u8>,
    room_name: String,
    call_type: i32,
    caller_display_name: String,
    call_encryption_key: Vec<u8>,
) {
    // Store pending call state for later use when accepting/declining
    // We'll get a real token when accepting
    let dummy_token = chatter_core::mailbox::CallRoomToken {
        id: String::new(),
        public_key: vec![],
        private_key: vec![],
        signature: vec![],
        randomizer: None,
    };

    // Convert call_room_id from Vec<u8> to [u8; 16]
    let mut call_room_id_array = [0u8; 16];
    if call_room_id.len() >= 16 {
        call_room_id_array.copy_from_slice(&call_room_id[..16]);
    } else {
        call_room_id_array[..call_room_id.len()].copy_from_slice(&call_room_id);
    }

    let pending = chatter_core::services::anon_dm::PendingAnonCall {
        call_room_id: call_room_id_array,
        room_name: room_name.clone(),
        conversation_id,
        token: dummy_token, // Will be replaced when accepting
        is_video_call: call_type == 2,
        is_outgoing: false,  // This is an incoming call
        display_name: caller_display_name.clone(),
        encryption_key: call_encryption_key,
        sender_id: Some(sender_id),
        started_at: None, // Will be set when call is accepted
    };
    ui_ctx.anon_dms().store_pending_anon_call(room_name.clone(), pending);

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Show incoming anonymous call UI
            let display_name = if caller_display_name.is_empty() {
                "Anonymous"
            } else {
                &caller_display_name
            };

            let call_type_str = if call_type == 2 { "video" } else { "voice" };

            // Show incoming call modal
            super::super::super::initial_data::show_incoming_call(
                &w,
                &room_name,  // Use room name as call ID
                &sender_id.to_string(),
                display_name,
                call_type_str,
                true,  // Anonymous calls are always verified
                "",    // No identity warning for anonymous calls
                true,  // anonymous call
            );

            // Store the offer details for when user answers
            // We'll get a token and send the answer when they click accept
            tracing::info!(
                conversation_id = %conversation_id,
                room_name = %room_name,
                call_type = call_type,
                "Showing incoming anonymous call UI"
            );
        }
    });
}

/// Handle anonymous call answer received.
pub(super) fn handle_anon_call_answer(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    accepted: bool,
    call_room_id: Vec<u8>,
    callee_display_name: String,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if accepted {
                // Call was accepted - connect to LiveKit
                tracing::info!(
                    conversation_id = %conversation_id,
                    "Anonymous call accepted, connecting to LiveKit"
                );

                // Get the pending call info from the service
                let room_name = format!("anon-{}", uuid::Uuid::from_bytes(call_room_id.as_slice().try_into().unwrap_or_default()));
                if let Some(pending) = ui_ctx.anon_dms().take_pending_anon_call(&room_name) {
                    // Validate that this matches our pending call
                    if call_room_id.as_slice() != pending.call_room_id.as_slice() {
                        tracing::warn!(
                            "Call answer received for different room ID: expected {:?}, got {:?}",
                            pending.call_room_id,
                            call_room_id
                        );
                        return;
                    }

                    // Mark call as started for duration tracking
                    ui_ctx.anon_dms().mark_anon_call_started(&pending.room_name);

                    // Join the call room using our token
                    let anon_dm_service = ui_ctx.anon_dms().clone();
                    let weak = weak.clone();

                    let call_service = ui_ctx.call_service().clone();
                    slint::spawn_local(async move {
                        // Get a fresh token for joining (the stored one may have been used for signaling)
                        let join_token = match anon_dm_service.get_available_call_room_token() {
                            Ok(Some(t)) => {
                                let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                t
                            }
                            _ => {
                                // Try requesting more tokens
                                match anon_dm_service.request_call_room_tokens(5).await {
                                    Ok(_) => match anon_dm_service.get_available_call_room_token() {
                                        Ok(Some(t)) => {
                                            let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                            t
                                        }
                                        _ => {
                                            tracing::error!("No call room tokens available for join");
                                            return;
                                        }
                                    },
                                    Err(e) => {
                                        tracing::error!("Failed to request call room tokens: {}", e);
                                        return;
                                    }
                                }
                            }
                        };
                        // Use fresh token for joining the call room
                        match anon_dm_service.join_anon_call_room(
                            &pending.room_name,
                            &join_token,
                            &pending.display_name,
                            pending.is_video_call,
                        ).await {
                            Ok(room_info) => {
                                tracing::info!(
                                    "Got LiveKit credentials for anonymous call with token"
                                );

                                // Get CallService and connect to LiveKit
                                let service = match call_service.get() {
                                    Some(svc) => svc.clone(),
                                    None => {
                                        tracing::error!("CallService not initialized");
                                        if let Some(w) = weak.upgrade() {
                                            super::super::super::initial_data::hide_incoming_call(&w);
                                        }
                                        ui_ctx.anon_dms().clear_pending_anon_call(&pending.room_name);
                                        return;
                                    }
                                };

                                // Create dummy IDs for anonymous calls
                                let call_id = chatter_core::domain::ids::CallId::new();
                                let channel_id = chatter_core::domain::ids::ChannelId::new();

                                // Connect to LiveKit with the access token and E2EE key
                                match service.connect_with_token(
                                    &room_info.livekit_token,
                                    &room_info.livekit_url,
                                    pending.encryption_key.clone(),
                                    &pending.room_name,
                                    call_id,
                                    channel_id,
                                ).await {
                                    Ok(()) => {
                                        tracing::info!("Connected to LiveKit for anonymous call (outgoing)");

                                        // Publish microphone immediately
                                        if let Err(e) = service.publish_microphone(None).await {
                                            tracing::error!("Failed to publish microphone: {}", e);
                                        } else {
                                            tracing::info!("Microphone published successfully");
                                        }

                                        // Update UI
                                        if let Some(w) = weak.upgrade() {
                                            super::super::super::initial_data::hide_incoming_call(&w);

                                            // Create participants for UI
                                            let local_participant = crate::CallParticipantData {
                                                user_id: "You".into(),
                                                display_name: "You".into(),
                                                avatar_url: "".into(),
                                                is_muted: false,
                                                is_video_enabled: pending.is_video_call,
                                                is_screen_sharing: false,
                                                is_speaking: false,
                                            };

                                            let remote_participant = crate::CallParticipantData {
                                                user_id: "Anonymous".into(),
                                                display_name: callee_display_name.into(),
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
                                            let call_data = crate::ActiveCallData {
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
                                                participants: slint::ModelRc::new(slint::VecModel::from(vec![
                                                    local_participant,
                                                    remote_participant,
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

                                            // Start timers
                                            crate::bin_ui::features::calls::timers::start_call_timer(&w, None);
                                            crate::bin_ui::features::calls::timers::start_video_frame_timer(&w, &service);

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
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to connect to LiveKit: {}", e);

                                        // Create failed call history entry
                                        let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                                        if let Ok(channel_id) = channel_id_str.parse::<chatter_core::domain::ids::ChannelId>() {
                                            let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                                            crate::bin_ui::features::calls::call_history::create_call_history_message(
                                                ui_ctx.messages().clone(),
                                                &channel_id,
                                                &pending.room_name,
                                                &anonymous_user_id,
                                                &pending.display_name,
                                                pending.is_outgoing,
                                                0, // Failed call, no duration
                                                true, // Mark as missed/failed
                                            );
                                        }

                                        // Show error to user
                                        if let Some(w) = weak.upgrade() {
                                            super::super::super::initial_data::hide_incoming_call(&w);
                                            // TODO: Show toast notification about call failure
                                        }
                                        ui_ctx.anon_dms().clear_pending_anon_call(&pending.room_name);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to join anonymous call room: {}", e
                                );

                                // Create failed call history entry
                                let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                                if let Ok(channel_id) = channel_id_str.parse::<chatter_core::domain::ids::ChannelId>() {
                                    let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                                    crate::bin_ui::features::calls::call_history::create_call_history_message(
                                        ui_ctx.messages().clone(),
                                        &channel_id,
                                        &pending.room_name,
                                        &anonymous_user_id,
                                        &pending.display_name,
                                        pending.is_outgoing,
                                        0, // Failed call, no duration
                                        true, // Mark as missed/failed
                                    );
                                }

                                // Hide call UI on error
                                if let Some(w) = weak.upgrade() {
                                    super::super::super::initial_data::hide_incoming_call(&w);
                                    // TODO: Show toast notification about room join failure
                                }
                                ui_ctx.anon_dms().clear_pending_anon_call(&pending.room_name);
                            }
                        }
                    }).unwrap();
                }
            } else {
                // Call was declined
                tracing::info!(
                    conversation_id = %conversation_id,
                    "Anonymous call declined by peer"
                );

                // Create declined call history entry
                // Construct room name from call_room_id
                let room_name = format!("anon-{}", uuid::Uuid::from_bytes(call_room_id.as_slice().try_into().unwrap_or_default()));
                if let Some(pending) = ui_ctx.anon_dms().take_pending_anon_call(&room_name) {
                    let channel_id_str = format!("anon-dm-{}", pending.conversation_id);
                    if let Ok(channel_id) = channel_id_str.parse::<chatter_core::domain::ids::ChannelId>() {
                        let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                        crate::bin_ui::features::calls::call_history::create_call_history_message(
                            ui_ctx.messages().clone(),
                            &channel_id,
                            &pending.room_name,
                            &anonymous_user_id,
                            &callee_display_name,
                            pending.is_outgoing,
                            0, // Declined call, no duration
                            true, // Mark as missed/declined
                        );
                    }
                    // Already consumed by take_pending_anon_call
                }

                w.set_active_call_visible(false);
                super::super::super::initial_data::hide_incoming_call(&w);
            }
        }
    });
}

/// Handle anonymous call hangup received.
pub(super) fn handle_anon_call_hangup(
    window: &Weak<MainWindow>,
    conversation_id: AnonDmId,
) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            // Hide any call-related UI elements
            super::super::super::initial_data::hide_incoming_call(&window);
            tracing::info!(conversation_id = %conversation_id, "Anon call ended");
        }
    });
}

/// Handle anonymous group call offer received.
pub(super) fn handle_anon_group_call_offer(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    sender_id: UserId,
    call_room_id: Vec<u8>,
    room_name: String,
    call_type: i32,
    caller_display_name: String,
    call_encryption_key: Vec<u8>,
) {
    // Store pending call state
    let dummy_token = chatter_core::mailbox::CallRoomToken {
        id: String::new(),
        public_key: vec![],
        private_key: vec![],
        signature: vec![],
        randomizer: None,
    };

    let mut call_room_id_array = [0u8; 16];
    if call_room_id.len() >= 16 {
        call_room_id_array.copy_from_slice(&call_room_id[..16]);
    } else {
        call_room_id_array[..call_room_id.len()].copy_from_slice(&call_room_id);
    }

    // Store as pending anon call (reuse the DM pending call infrastructure)
    let conversation_id = chatter_core::domain::anon_dm::AnonDmId::try_from_slice(
        group_id.as_bytes()
    ).expect("AnonGroupId is 16 bytes");

    let pending = chatter_core::services::anon_dm::PendingAnonCall {
        call_room_id: call_room_id_array,
        room_name: room_name.clone(),
        conversation_id,
        token: dummy_token,
        is_video_call: call_type == 2,
        is_outgoing: false,
        display_name: caller_display_name.clone(),
        encryption_key: call_encryption_key,
        sender_id: Some(sender_id),
        started_at: None,
    };
    ui_ctx.anon_dms().store_pending_anon_call(room_name.clone(), pending);

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

/// Handle anonymous group call answer received.
pub(super) fn handle_anon_group_call_answer(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    group_id: AnonGroupId,
    accepted: bool,
    call_room_id: Vec<u8>,
    callee_display_name: String,
) {
    // Reuse the same answer handling as anon DM calls
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(_w) = weak.upgrade() {
            if accepted {
                tracing::info!(
                    group_id = %group_id,
                    "Anonymous group call accepted, connecting to LiveKit"
                );

                let room_name = format!("anon-{}", uuid::Uuid::from_bytes(
                    call_room_id.as_slice().try_into().unwrap_or_default()
                ));
                if let Some(pending) = ui_ctx.anon_dms().take_pending_anon_call(&room_name) {
                    ui_ctx.anon_dms().mark_anon_call_started(&pending.room_name);

                    let anon_dm_service = ui_ctx.anon_dms().clone();
                    let call_service = ui_ctx.call_service().clone();
                    let weak = weak.clone();

                    slint::spawn_local(async move {
                        let join_token = match anon_dm_service.get_available_call_room_token() {
                            Ok(Some(t)) => {
                                let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                t
                            }
                            _ => {
                                match anon_dm_service.request_call_room_tokens(5).await {
                                    Ok(_) => match anon_dm_service.get_available_call_room_token() {
                                        Ok(Some(t)) => {
                                            let _ = anon_dm_service.mark_call_room_token_used(&t.id);
                                            t
                                        }
                                        _ => {
                                            tracing::error!("No call room tokens for group call join");
                                            return;
                                        }
                                    },
                                    Err(e) => {
                                        tracing::error!("Failed to request tokens: {}", e);
                                        return;
                                    }
                                }
                            }
                        };

                        match anon_dm_service.join_anon_call_room(
                            &pending.room_name,
                            &join_token,
                            &pending.display_name,
                            pending.is_video_call,
                        ).await {
                            Ok(room_info) => {
                                tracing::info!("Got LiveKit credentials for anonymous group call");

                                let service = match call_service.get() {
                                    Some(svc) => svc.clone(),
                                    None => {
                                        tracing::error!("CallService not initialized");
                                        return;
                                    }
                                };

                                let call_id = chatter_core::domain::ids::CallId::new();
                                let channel_id = chatter_core::domain::ids::ChannelId::new();

                                match service.connect_with_token(
                                    &room_info.livekit_token,
                                    &room_info.livekit_url,
                                    pending.encryption_key.clone(),
                                    &pending.room_name,
                                    call_id,
                                    channel_id,
                                ).await {
                                    Ok(_) => {
                                        tracing::info!("Connected to anonymous group call room");
                                        if let Err(e) = service.publish_microphone(None).await {
                                            tracing::error!("Failed to publish mic: {}", e);
                                        }
                                        if pending.is_video_call {
                                            if let Err(e) = service.publish_camera(None).await {
                                                tracing::error!("Failed to publish camera: {}", e);
                                            }
                                        }
                                        if let Some(w) = weak.upgrade() {
                                            let mut call_data = w.get_active_call_data();
                                            call_data.title = format!("Group Call").into();
                                            w.set_active_call_data(call_data);
                                            crate::bin_ui::features::calls::timers::start_call_timer(&w, None);
                                            if let Some(svc) = call_service.get() {
                                                crate::bin_ui::features::calls::timers::start_video_frame_timer(&w, svc);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to connect to group call: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to join group call room: {}", e);
                            }
                        }
                    }).unwrap();
                }
            } else {
                tracing::info!(
                    group_id = %group_id,
                    callee = %callee_display_name,
                    "Group call member declined"
                );
            }
        }
    });
}

/// Handle anonymous group call hangup received.
pub(super) fn handle_anon_group_call_hangup(
    window: &Weak<MainWindow>,
    group_id: AnonGroupId,
) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            super::super::super::initial_data::hide_incoming_call(&window);
            tracing::info!(group_id = %group_id, "Anon group call ended");
        }
    });
}
