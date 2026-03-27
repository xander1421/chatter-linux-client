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
    _ui_ctx: &Arc<ChatterClient>,
    conversation_id: AnonDmId,
    sender_id: UserId,
    _call_room_id: Vec<u8>,
    room_name: String,
    call_type: i32,
    caller_display_name: String,
    _call_encryption_key: Vec<u8>,
) {
    // Pending call state is stored by core EventHandlers on AnonCallOfferReceived.
    // UI only shows the incoming call modal.

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
        if weak.upgrade().is_some() {
            // Delegate to core — handles token, room join, LiveKit connect, call history
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            let call_service = ui_ctx.call_service().clone();

            slint::spawn_local(async move {
                let result = {
                    let ctx = ui_ctx.clone();
                    let crid = call_room_id.clone();
                    let dn = callee_display_name.clone();
                    tokio::spawn(async move {
                        ctx.handle_anon_call_answer(&crid, accepted, &dn).await
                    }).await.unwrap()
                };

                match result {
                    Ok(Some(setup)) => {
                        // Call accepted — show active call UI
                        tracing::info!(room_name = %setup.room_name, "Anonymous call answer handled via core");

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

                        if let Some(w) = weak.upgrade() {
                            super::super::super::initial_data::hide_incoming_call(&w);

                            let local_participant = crate::CallParticipantData {
                                user_id: "You".into(),
                                display_name: "You".into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: setup.is_video,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            let remote_participant = crate::CallParticipantData {
                                user_id: "Anonymous".into(),
                                display_name: setup.peer_display_name.clone().into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: setup.is_video,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            let call_data = crate::ActiveCallData {
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
                                participants: slint::ModelRc::new(slint::VecModel::from(vec![
                                    local_participant,
                                    remote_participant,
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

                            crate::bin_ui::features::calls::timers::start_call_timer(&w, None);
                            crate::bin_ui::features::calls::timers::start_video_frame_timer(&w, &service);

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
                    Ok(None) => {
                        // Call was declined by peer — core already stored call history
                        tracing::info!(conversation_id = %conversation_id, "Anonymous call declined by peer");
                        if let Some(w) = weak.upgrade() {
                            w.set_active_call_visible(false);
                            super::super::super::initial_data::hide_incoming_call(&w);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to handle anonymous call answer: {}", e);
                        if let Some(w) = weak.upgrade() {
                            super::super::super::initial_data::hide_incoming_call(&w);
                        }
                    }
                }
            }).unwrap();
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
    // Reuse the same core delegation as anon DM call answers
    // (pending call state is stored in the same infrastructure)
    handle_anon_call_answer(
        window,
        ui_ctx,
        // Convert group_id to AnonDmId (same bytes, different type — stored that way in pending)
        chatter_core::domain::anon_dm::AnonDmId::try_from_slice(
            group_id.as_bytes()
        ).expect("AnonGroupId is 16 bytes"),
        accepted,
        call_room_id,
        callee_display_name,
    );
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
