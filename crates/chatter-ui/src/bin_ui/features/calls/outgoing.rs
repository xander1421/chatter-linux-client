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
    let ctx_session_service = ui_ctx.sessions().clone();
    let ctx_message_service = ui_ctx.messages().clone();
    let ctx_mailbox_manager = ui_ctx.mailbox_manager().clone();
    let ctx_channel_service = ui_ctx.channels().clone();
    let ctx_chat_state = ui_ctx.chat_state().clone();
    let ctx_ui = ui_ctx.clone();

    // Start voice call
    window.on_start_voice_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let session_service = ctx_session_service.clone();
        let message_service = ctx_message_service.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let channel_service = ctx_channel_service.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();

        move || {
            let weak = weak.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_service = channel_service.clone();
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
                    start_call_impl(
                        weak.clone(),
                        call_service,
                        session_service,
                        CallType::Voice,
                        message_service,
                        mailbox_manager,
                        channel_service,
                        conversation,
                        ui_ctx,
                    ),
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
        let session_service = ctx_session_service.clone();
        let message_service = ctx_message_service.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let channel_service = ctx_channel_service.clone();
        let chat_state = ctx_chat_state.clone();
        let ui_ctx = ctx_ui.clone();

        move || {
            let weak = weak.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_service = channel_service.clone();
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
                    start_call_impl(
                        weak.clone(),
                        call_service,
                        session_service,
                        CallType::Video,
                        message_service,
                        mailbox_manager,
                        channel_service,
                        conversation,
                        ui_ctx,
                    ),
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

#[allow(clippy::too_many_arguments)]
async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    call_service: CallWorkerHandle,
    session_service: Arc<chatter_core::services::session_service::SessionService>,
    call_type: CallType,
    _message_service: Arc<chatter_core::services::message::MessageService>,
    _mailbox_manager: Arc<chatter_core::services::mailbox_manager::MailboxManager>,
    channel_service: Arc<chatter_core::services::channel::ChannelService>,
    conversation: ActiveConversation,
    ui_ctx: Arc<ChatterClient>,
) {
    // Get current user ID and display name
    let user_id = match session_service.current_user_id() {
        Some(id) => id,
        None => {
            tracing::error!("No user ID available");
            return;
        }
    };

    let display_name = ui_ctx.anon_dms().get_my_display_name().unwrap_or_else(|| "Anonymous".to_string());

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
            match ui_ctx.anon_dms().get_conversation(&anon_dm_id) {
                Ok(Some(dm)) => dm.peer_id,
                Ok(None) => {
                    tracing::error!("Anon DM conversation not found: {}", anon_dm_id);
                    return;
                }
                Err(e) => {
                    tracing::error!("Failed to get anon DM conversation: {}", e);
                    return;
                }
            }
        } else {
            // Regular DM - look up from channel membership
            match channel_service.get_dm_peer_local(&channel_id) {
                Ok(Some(id)) => id,
                Ok(None) => {
                    tracing::error!("Could not determine peer ID for DM call");
                    return;
                }
                Err(e) => {
                    tracing::error!("Failed to get peer ID: {}", e);
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
        // Group Call with Distributed Key
        // =================================================================
        // Generate random key and distribute via pairwise sealed sender
        // =================================================================

        let group_key = chatter_core::crypto::generate_group_call_key();

        match call_service.initiate_call(
            channel_id,
            call_type_str,
            call_type,
            &group_key,
            user_id,
        ).await {
            Ok(call) => {
                tracing::info!("Started group call: {:?}", call.id);

                // Publish microphone immediately after joining
                if let Err(e) = call_service.publish_microphone(None).await {
                    tracing::error!("Failed to publish microphone: {}", e);
                }

                // =============================================================
                // Group Call Key Distribution (Standard Channel)
                // =============================================================
                // Distribute group_key to all channel members via pairwise sealed sender.
                // This ensures all members have the same AES-256 key for E2EE media.
                // =============================================================
                let channel_id_clone = channel_id;
                let call_id_str = call.id.to_string();
                let signaling = ui_ctx.call_signaling().clone();

                // gRPC - run on tokio (fire-and-forget, no UI update needed)
                tokio::spawn(async move {
                    match signaling.distribute_group_call_key(
                        &channel_id_clone, &call_id_str, group_key, 0,
                    ).await {
                        Ok(count) => tracing::info!(call_id = %call_id_str, sent = count, "Group call key distributed"),
                        Err(e) => tracing::error!(call_id = %call_id_str, error = %e, "Failed to distribute group call key"),
                    }
                });

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

                    let verification = chatter_core::services::CallVerification::derive_verification_emojis(
                        &group_key,
                        &call.id.to_string(),
                    );
                    let (e1, e2, e3, e4) = chatter_core::services::CallVerification::to_slint_tuple(&verification);

                    let call_data = ActiveCallData {
                        call_id: call.id.to_string().into(),
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
