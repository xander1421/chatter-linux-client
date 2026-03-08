//! Outgoing call initiation.
//!
//! Handles starting voice and video calls.

use std::str::FromStr;
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::crypto::EphemeralCallKey;
use chatter_linux::domain::call::CallType;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::services::CallWorkerHandle;
use crate::{ActiveCallData, CallParticipantData, MainWindow};

use super::key_exchange::{set_pending_call, PendingOutgoingCall};
use super::timers::{start_call_timer, start_video_frame_timer};

/// Wire start call callbacks.
pub fn wire_start_call(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let weak = window.as_weak();
    let ctx_call_service = ui_ctx.call_service.clone();
    let ctx_session_service = ui_ctx.session_service.clone();
    let ctx_message_service = ui_ctx.message_service.clone();
    let ctx_mailbox_manager = ui_ctx.mailbox_manager.clone();
    let ctx_channel_service = ui_ctx.channel_service.clone();

    // Start voice call
    window.on_start_voice_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let session_service = ctx_session_service.clone();
        let message_service = ctx_message_service.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let channel_service = ctx_channel_service.clone();

        move || {
            let weak = weak.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_service = channel_service.clone();

            // Get the call service from OnceLock
            let call_service = match call_service.get() {
                Some(svc) => svc.clone(),
                None => {
                    tracing::error!("CallService not initialized");
                    return;
                }
            };

            slint::spawn_local(async move {
                start_call_impl(
                    weak,
                    call_service,
                    session_service,
                    CallType::Voice,
                    message_service,
                    mailbox_manager,
                    channel_service,
                ).await;
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

        move || {
            let weak = weak.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_service = channel_service.clone();

            // Get the call service from OnceLock
            let call_service = match call_service.get() {
                Some(svc) => svc.clone(),
                None => {
                    tracing::error!("CallService not initialized");
                    return;
                }
            };

            slint::spawn_local(async move {
                start_call_impl(
                    weak,
                    call_service,
                    session_service,
                    CallType::Video,
                    message_service,
                    mailbox_manager,
                    channel_service,
                ).await;
            }).unwrap();
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    call_service: CallWorkerHandle,
    session_service: Arc<chatter_linux::services::session_service::SessionService>,
    call_type: CallType,
    message_service: Arc<chatter_linux::services::message::MessageService>,
    mailbox_manager: Arc<chatter_linux::services::mailbox_manager::MailboxManager>,
    channel_service: Arc<chatter_linux::services::channel::ChannelService>,
) {
    // Get current channel ID and name from the window
    // Check both selected-channel-id (for channels) and selected-dm-id (for DMs)
    let (channel_id_str, channel_name, is_dm) = if let Some(w) = weak.upgrade() {
        let channel_id = w.get_selected_channel_id().to_string();
        let dm_id = w.get_selected_dm_id().to_string();

        if !channel_id.is_empty() {
            (channel_id, w.get_channel_name().to_string(), false)
        } else if !dm_id.is_empty() {
            // For DMs, the dm_id IS the channel_id
            (dm_id, w.get_channel_name().to_string(), true)
        } else {
            (String::new(), String::new(), false)
        }
    } else {
        return;
    };

    if channel_id_str.is_empty() {
        tracing::warn!("No channel or DM selected for call");
        return;
    }

    let channel_id = match ChannelId::from_str(&channel_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid channel ID: {}", e);
            return;
        }
    };

    // Get current user ID and display name
    let user_id = match session_service.current_user_id() {
        Some(id) => id,
        None => {
            tracing::error!("No user ID available");
            return;
        }
    };

    // Display name shown as "You" for local participant
    let display_name = "You".to_string();

    let is_video_call = matches!(call_type, CallType::Video);
    let call_type_str = match call_type {
        CallType::Voice => "Voice Call",
        CallType::Video => "Video Call",
        CallType::Group => "Group Call",
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

        // Get peer ID first
        let peer_id = match channel_service.get_dm_peer_local(&channel_id) {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::error!("Could not determine peer ID for DM call");
                return;
            }
            Err(e) => {
                tracing::error!("Failed to get peer ID: {}", e);
                return;
            }
        };

        // Generate ephemeral keypair for PFS
        let ephemeral_key = EphemeralCallKey::generate();
        let ephemeral_public = ephemeral_key.public_key_bytes();

        tracing::info!(
            peer_id = %peer_id,
            "Generated ephemeral key for DM call, sending offer"
        );

        // Create call on server (temporary placeholder key - will update when answer arrives)
        let placeholder_key = [0u8; 32];
        match call_service.initiate_call(
            channel_id,
            call_type_str,
            call_type,
            &placeholder_key,
            user_id,
        ).await {
            Ok(call) => {
                tracing::info!("Created call on server: {:?}, waiting for peer's ephemeral key", call.id);

                // Build and send offer with our ephemeral public key
                let content = chatter_linux::services::build_call_key_offer(
                    &call.id.to_string(),
                    ephemeral_public,
                    call_type_str,
                    &channel_id_str,
                );

                if let Err(e) = message_service
                    .send_dm(&peer_id, content, &mailbox_manager)
                    .await
                {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to send call offer"
                    );
                    // Clean up the call on server
                    let _ = call_service.leave_call().await;
                    return;
                }

                tracing::info!(
                    peer_id = %peer_id,
                    call_id = %call.id,
                    "Call offer sent, storing pending state"
                );

                // Store pending call state (ephemeral key kept until answer)
                set_pending_call(PendingOutgoingCall {
                    call_id: call.id,
                    channel_id,
                    ephemeral_key,
                    peer_id,
                    call_type: call_type_str.to_string(),
                    is_video_call,
                });

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
                        call_id: call.id.to_string().into(),
                        channel_id: channel_id_str.clone().into(),
                        title: if channel_name.is_empty() { format!("Calling...").into() } else { channel_name.into() },
                        is_video_call,
                        is_encrypted: true,
                        is_identity_verified: false,
                        identity_warning: "".into(),
                        participant_count: 1,
                        duration_seconds: 0,
                        participants: ModelRc::new(VecModel::from(vec![local_participant])),
                        verification_emoji_1: "⏳".into(),
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
                tracing::error!("Failed to create call on server: {}", e);
            }
        }
    } else {
        // =================================================================
        // Group Call with Distributed Key
        // =================================================================
        // Generate random key and distribute via pairwise sealed sender
        // =================================================================

        let group_key = chatter_linux::crypto::generate_group_call_key();

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
                let channel_id_clone = channel_id.clone();
                let message_service_clone = message_service.clone();
                let channel_service_clone = channel_service.clone();
                let mailbox_manager_clone = mailbox_manager.clone();
                let call_id_str = call.id.to_string();
                let channel_id_str = channel_id_clone.to_string();
                let group_key_arr = group_key.clone();

                let channel_id_str_for_spawn = channel_id_str.clone();
                let _ = slint::spawn_local(async move {
                    match channel_service_clone.get_channel_members(&channel_id_clone, 100, 0).await {
                        Ok(members) => {
                            tracing::info!(
                                call_id = %call_id_str,
                                member_count = %members.len(),
                                "Distributing group call key to members"
                            );

                            let channel_id_str_inner = channel_id_str_for_spawn.clone();
                            for member in members {
                                // Don't send the key to ourselves
                                if member.user_id == user_id {
                                    continue;
                                }

                                // Build GroupCallKey content
                                let content = chatter_linux::services::build_group_call_key_content(
                                    &call_id_str,
                                    group_key_arr,
                                    0, // Initial epoch
                                    &channel_id_str_inner,
                                );


                                // Send via pairwise DM (uses sealed sender)
                                if let Err(e) = message_service_clone
                                    .send_dm(&member.user_id, content, &mailbox_manager_clone)
                                    .await
                                {
                                    tracing::warn!(
                                        call_id = %call_id_str,
                                        peer_id = %member.user_id,
                                        error = %e,
                                        "Failed to distribute group call key to member"
                                    );
                                }
                            }
                            tracing::info!(call_id = %call_id_str, "Completed group call key distribution");
                        }
                        Err(e) => {
                            tracing::error!(
                                call_id = %call_id_str,
                                error = %e,
                                "Failed to fetch channel members for key distribution"
                            );
                        }
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

                    let verification = chatter_linux::services::CallVerification::derive_verification_emojis(
                        &group_key,
                        &call.id.to_string(),
                    );
                    let (e1, e2, e3, e4) = chatter_linux::services::CallVerification::to_slint_tuple(&verification);

                    let call_data = ActiveCallData {
                        call_id: call.id.to_string().into(),
                        channel_id: channel_id_str.into(),
                        title: if channel_name.is_empty() { call_type_str.into() } else { channel_name.into() },
                        is_video_call,
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
