//! Outgoing call initiation.
//!
//! Handles starting voice and video calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::call::CallType;
use chatter_core::services::CallWorkerHandle;
use chatter_core::state::ActiveConversation;
use crate::{ActiveCallData, CallParticipantData, MainWindow};

use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire start call callbacks.
pub fn wire_start_call(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ctx_call_service = ui_ctx.call_service().clone();
    let ctx_chat_state = ui_ctx.chat_state().clone();
    let ctx_ui = ui_ctx.clone();

    // Start voice call
    window.on_start_voice_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();

        move || {
            let weak = weak.clone();
            let conversation = chat_state.active_conversation();
            let ui_ctx = ui_ctx.clone();

            // Get the call service from OnceLock
            let call_service = match call_service.get() {
                Some(svc) => svc.clone(),
                None => {
                    tracing::error!("CallService not initialized");
                    return;
                }
            };

            slint::spawn_local(async move {
                if tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    start_call_impl(weak.clone(), call_service, CallType::Voice, conversation, ui_ctx),
                ).await.is_err() {
                    tracing::error!("Call setup timed out");
                    if let Some(w) = weak.upgrade() {
                        w.set_active_call_visible(false);
                    }
                }
            }).unwrap();
        }
    });

    // Start video call
    window.on_start_video_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();

        move || {
            let weak = weak.clone();
            let conversation = chat_state.active_conversation();
            let ui_ctx = ui_ctx.clone();

            // Get the call service from OnceLock
            let call_service = match call_service.get() {
                Some(svc) => svc.clone(),
                None => {
                    tracing::error!("CallService not initialized");
                    return;
                }
            };

            slint::spawn_local(async move {
                if tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    start_call_impl(weak.clone(), call_service, CallType::Video, conversation, ui_ctx),
                ).await.is_err() {
                    tracing::error!("Call setup timed out");
                    if let Some(w) = weak.upgrade() {
                        w.set_active_call_visible(false);
                    }
                }
            }).unwrap();
        }
    });
}

async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    call_service: CallWorkerHandle,
    call_type: CallType,
    conversation: ActiveConversation,
    ui_ctx: Arc<ChatterClient>,
) {
    // Get current user ID and display name
    let user_id = match ui_ctx.session_current_user_id() {
        Some(id) => id,
        None => {
            tracing::error!("No user ID available");
            return;
        }
    };

    let display_name = ui_ctx.my_display_name().unwrap_or_else(|| "Anonymous".to_string());

    let is_video_call = matches!(call_type, CallType::Video);
    let call_type_str = match call_type {
        CallType::Voice => "Voice Call",
        CallType::Video => "Video Call",
        CallType::Group => "Group Call",
    };

    // Dispatch directly on the typed enum — no string serialization
    match conversation {
        ActiveConversation::AnonGroup(id) => {
            start_private_group_call(
                weak,
                &id.to_string(),
                &display_name,
                user_id,
                is_video_call,
                call_type_str,
                ui_ctx,
            ).await;
            return;
        }
        ActiveConversation::None => {
            tracing::warn!("No channel, DM, or private group selected for call");
            return;
        }
        _ => {} // Channel, Dm, AnonDm — handled below
    }

    let channel_name = weak.upgrade()
        .map(|w| w.get_channel_name().to_string())
        .unwrap_or_default();

    let is_dm = matches!(conversation, ActiveConversation::MailboxDm { .. } | ActiveConversation::ServerDm { .. } | ActiveConversation::AnonDm(_));

    let channel_id = match conversation.channel_id() {
        Some(id) => id,
        None => {
            tracing::error!("No channel ID for conversation");
            return;
        }
    };

    tracing::info!("Starting {} in channel {}", call_type_str, channel_id);

    if is_dm {
        // =================================================================
        // DM Call with Ephemeral Key Exchange (PFS)
        // =================================================================
        // 1. Generate ephemeral X25519 keypair
        // 2. Create call on server (but don't connect yet)
        // 3. Send offer with our ephemeral public key
        // 4. Store pending state, wait for answer
        // 5. When answer arrives → derive key → connect
        // =================================================================

        // Get peer ID - different lookup for anon DMs vs regular DMs
        let peer_id = if let ActiveConversation::AnonDm(anon_dm_id) = conversation {
            // For anon DMs, look up the conversation to get peer_id
            match ui_ctx.get_anon_dm_conversation(&anon_dm_id) {
                Some(dm) => dm.peer_id,
                None => {
                    tracing::error!("Anon DM conversation not found: {}", anon_dm_id);
                    return;
                }
            }
        } else {
            // Regular DM - look up from channel membership
            match ui_ctx.get_dm_peer_local(&channel_id) {
                Some(id) => id,
                None => {
                    tracing::error!("Could not determine peer ID for DM call");
                    return;
                }
            }
        };

        // Handle anonymous DM calls differently
        if let ActiveConversation::AnonDm(anon_dm_id) = conversation {
            // Delegate to core — handles token acquisition, offer, pending state, and failed call history
            let start_result = {
                let ctx = ui_ctx.clone();
                let display_name = display_name.clone();
                tokio::spawn(async move {
                    ctx.start_anon_dm_call(&anon_dm_id, is_video_call, &display_name).await
                }).await.unwrap()
            };

            match start_result {
                Ok(offer_result) => {
                    tracing::info!(room_name = %offer_result.room_name, "Anon DM call started via core");

                    // Show "Calling..." UI
                    if let Some(w) = weak.upgrade() {
                        let local_participant = CallParticipantData {
                            user_id: user_id.to_string().into(),
                            display_name: display_name.into(),
                            avatar_url: "".into(),
                            is_muted: false,
                            is_video_enabled: is_video_call,
                            is_screen_sharing: false,
                            is_speaking: false,
                        };

                        let call_data = ActiveCallData {
                            call_id: offer_result.room_name.clone().into(),
                            channel_id: channel_id.to_string().into(),
                            title: format!("Calling...").into(),
                            is_video_call,
                            is_anonymous_call: true,
                            is_encrypted: true,
                            is_identity_verified: true,
                            identity_warning: "".into(),
                            participant_count: 1,
                            duration_seconds: 0,
                            participants: ModelRc::new(VecModel::from(vec![local_participant])),
                            verification_emoji_1: "\u{1F512}".into(),
                            verification_emoji_2: "".into(),
                            verification_emoji_3: "".into(),
                            verification_emoji_4: "".into(),
                        };

                        w.set_active_call_data(call_data);
                        w.set_call_is_muted(false);
                        w.set_call_is_video_enabled(is_video_call);
                        w.set_call_is_screen_sharing(false);
                        w.set_active_call_visible(true);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to start anon DM call: {}", e);
                    if let Some(w) = weak.upgrade() {
                        w.set_active_call_visible(false);
                    }
                }
            }
        } else {
            // Regular DM call — delegate to core orchestration
            let start_result = {
                let ctx = ui_ctx.clone();
                let peer = peer_id;
                tokio::spawn(async move {
                    ctx.start_dm_call(&peer, channel_id, call_type, user_id).await
                }).await.unwrap()
            };

            match start_result {
                Ok(call_id) => {
                    tracing::info!(call_id = %call_id, "DM call started via core, waiting for answer");

                    // Show "Calling..." UI (not connected yet)
                    if let Some(w) = weak.upgrade() {
                        let local_participant = CallParticipantData {
                            user_id: user_id.to_string().into(),
                            display_name: display_name.into(),
                            avatar_url: "".into(),
                            is_muted: false,
                            is_video_enabled: is_video_call,
                            is_screen_sharing: false,
                            is_speaking: false,
                        };

                        // Placeholder verification (will update when connected)
                        let call_data = ActiveCallData {
                            call_id: call_id.to_string().into(),
                            channel_id: channel_id.to_string().into(),
                            title: if channel_name.is_empty() { format!("Calling...").into() } else { channel_name.into() },
                            is_video_call,
                            is_anonymous_call: false,
                            is_encrypted: true,
                            is_identity_verified: false,
                            identity_warning: "".into(),
                            participant_count: 1,
                            duration_seconds: 0,
                            participants: ModelRc::new(VecModel::from(vec![local_participant])),
                            verification_emoji_1: "\u{23F3}".into(),
                            verification_emoji_2: "".into(),
                            verification_emoji_3: "".into(),
                            verification_emoji_4: "".into(),
                        };

                        w.set_active_call_data(call_data);
                        w.set_call_is_muted(false);
                        w.set_call_is_video_enabled(is_video_call);
                        w.set_call_is_screen_sharing(false);
                        w.set_active_call_visible(true);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to start DM call: {}", e);
                }
            }
        }
    } else {
        // =================================================================
        // Group Call — core handles key generation + initiation + distribution
        // =================================================================

        let start_result = {
            let ctx = ui_ctx.clone();
            tokio::spawn(async move {
                ctx.start_group_call(channel_id, call_type, user_id).await
            }).await.unwrap()
        };

        match start_result {
            Ok(setup) => {
                tracing::info!(call_id = %setup.call_id, "Started group call via core");

                if let Some(w) = weak.upgrade() {
                    let local_participant = CallParticipantData {
                        user_id: user_id.to_string().into(),
                        display_name: display_name.into(),
                        avatar_url: "".into(),
                        is_muted: false,
                        is_video_enabled: is_video_call,
                        is_screen_sharing: false,
                        is_speaking: false,
                    };

                    let call_data = ActiveCallData {
                        call_id: setup.call_id.to_string().into(),
                        channel_id: channel_id.to_string().into(),
                        title: if channel_name.is_empty() { call_type_str.into() } else { channel_name.into() },
                        is_video_call,
                        is_anonymous_call: false,
                        is_encrypted: true,
                        is_identity_verified: false,
                        identity_warning: "".into(),
                        participant_count: 1,
                        duration_seconds: 0,
                        participants: ModelRc::new(VecModel::from(vec![local_participant])),
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

                    start_call_timer(&w, None);
                    start_video_frame_timer(&w, &call_service);
                }
            }
            Err(e) => {
                tracing::error!("Failed to start group call: {}", e);
            }
        }
    }
}

/// Start a call in a private (anonymous) group.
///
/// Delegates to core for token acquisition, offer sending, and pending state management.
async fn start_private_group_call(
    weak: slint::Weak<MainWindow>,
    group_id_str: &str,
    display_name: &str,
    user_id: chatter_core::domain::ids::UserId,
    is_video_call: bool,
    call_type_str: &str,
    ui_ctx: Arc<ChatterClient>,
) {
    use chatter_core::domain::anon_group::AnonGroupId;

    let anon_group_id: AnonGroupId = match group_id_str.parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid private group ID: {}", e);
            return;
        }
    };

    tracing::info!("Starting {} in private group {}", call_type_str, anon_group_id);

    // Delegate to core — handles token acquisition, offer, pending state
    let start_result = {
        let ctx = ui_ctx.clone();
        let gid = anon_group_id;
        let dn = display_name.to_string();
        tokio::spawn(async move {
            ctx.start_anon_group_call(&gid, is_video_call, &dn).await
        }).await.unwrap()
    };

    match start_result {
        Ok(offer_result) => {
            tracing::info!(room_name = %offer_result.room_name, "Anon group call started via core");

            // Show "Calling..." UI
            if let Some(w) = weak.upgrade() {
                let local_participant = CallParticipantData {
                    user_id: user_id.to_string().into(),
                    display_name: display_name.to_string().into(),
                    avatar_url: "".into(),
                    is_muted: false,
                    is_video_enabled: is_video_call,
                    is_screen_sharing: false,
                    is_speaking: false,
                };

                let call_data = ActiveCallData {
                    call_id: offer_result.room_name.into(),
                    channel_id: group_id_str.to_string().into(),
                    title: format!("Calling...").into(),
                    is_video_call,
                    is_anonymous_call: true,
                    is_encrypted: true,
                    is_identity_verified: true,
                    identity_warning: "".into(),
                    participant_count: 1,
                    duration_seconds: 0,
                    participants: ModelRc::new(VecModel::from(vec![local_participant])),
                    verification_emoji_1: "\u{1F512}".into(),
                    verification_emoji_2: "".into(),
                    verification_emoji_3: "".into(),
                    verification_emoji_4: "".into(),
                };

                w.set_active_call_data(call_data);
                w.set_call_is_muted(false);
                w.set_call_is_video_enabled(is_video_call);
                w.set_call_is_screen_sharing(false);
                w.set_active_call_visible(true);
            }
        }
        Err(e) => {
            tracing::error!("Failed to start anon group call: {}", e);
            if let Some(w) = weak.upgrade() {
                w.set_active_call_visible(false);
            }
        }
    }
}
