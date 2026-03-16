//! Outgoing call initiation.
//!
//! Handles starting voice and video calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::app::UIContext;
use chatter_core::crypto::EphemeralCallKey;
use chatter_core::domain::call::CallType;
use chatter_core::domain::ids::ChannelId;
use chatter_core::services::CallWorkerHandle;
use chatter_core::state::ActiveConversation;
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
    let ctx_anon_dm_service = ui_ctx.anon_dm_service.clone();
    let ctx_anon_group_service = ui_ctx.anon_group_service.clone();
    let ctx_chat_state = ui_ctx.chat_state.clone();

    // Start voice call
    window.on_start_voice_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let session_service = ctx_session_service.clone();
        let message_service = ctx_message_service.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let channel_service = ctx_channel_service.clone();
        let anon_dm_service = ctx_anon_dm_service.clone();
        let anon_group_service = ctx_anon_group_service.clone();
        let chat_state = ctx_chat_state.clone();

        move || {
            let weak = weak.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_service = channel_service.clone();
            let anon_dm_service = anon_dm_service.clone();
            let anon_group_service = anon_group_service.clone();
            let conversation = chat_state.active_conversation();

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
                        anon_dm_service,
                        anon_group_service,
                        conversation,
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
        let anon_dm_service = ctx_anon_dm_service.clone();
        let anon_group_service = ctx_anon_group_service.clone();
        let chat_state = ctx_chat_state.clone();

        move || {
            let weak = weak.clone();
            let session_service = session_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_service = channel_service.clone();
            let anon_dm_service = anon_dm_service.clone();
            let anon_group_service = anon_group_service.clone();
            let conversation = chat_state.active_conversation();

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
                        anon_dm_service,
                        anon_group_service,
                        conversation,
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
    message_service: Arc<chatter_core::services::message::MessageService>,
    mailbox_manager: Arc<chatter_core::services::mailbox_manager::MailboxManager>,
    channel_service: Arc<chatter_core::services::channel::ChannelService>,
    anon_dm_service: Arc<chatter_core::services::anon_dm::AnonDmService>,
    anon_group_service: Arc<chatter_core::services::anon_group::AnonGroupService>,
    conversation: ActiveConversation,
) {
    // Get current user ID and display name
    let user_id = match session_service.current_user_id() {
        Some(id) => id,
        None => {
            tracing::error!("No user ID available");
            return;
        }
    };

    let display_name = anon_dm_service.get_my_display_name().unwrap_or_else(|| "Anonymous".to_string());

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
                anon_dm_service,
                anon_group_service,
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

    let is_dm = matches!(conversation, ActiveConversation::Dm { .. } | ActiveConversation::AnonDm(_));

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
            match anon_dm_service.get_conversation(&anon_dm_id) {
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
            
            // Get a blind signature token for the call (request more if needed)
            let token = match anon_dm_service.get_available_call_room_token() {
                Ok(Some(token)) => token,
                Ok(None) => {
                    tracing::info!("No call room tokens available - requesting more");
                    
                    // Request 5 call room tokens
                    match anon_dm_service.request_call_room_tokens(5).await {
                        Ok(count) => {
                            tracing::info!("Successfully requested {} call room tokens", count);
                            
                            // Try again to get a token
                            match anon_dm_service.get_available_call_room_token() {
                                Ok(Some(token)) => token,
                                _ => {
                                    tracing::error!("Still no tokens available after request");
                                    
                                    // Create failed call history entry
                                    let channel_id_str = format!("anon-dm-{}", anon_dm_id);
                                    if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                                        use crate::bin_ui::features::calls::call_history::create_call_history_message;
                                        let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                                        create_call_history_message(
                                            message_service.clone(),
                                            &channel_id,
                                            "no-tokens-available",
                                            &anonymous_user_id,
                                            &display_name,
                                            true, // This was an outgoing call
                                            0, // Failed call, no duration
                                            true, // Mark as missed/failed
                                        );
                                    }
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to request call room tokens: {}", e);
                            
                            // Create failed call history entry
                            let channel_id_str = format!("anon-dm-{}", anon_dm_id);
                            if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                                use crate::bin_ui::features::calls::call_history::create_call_history_message;
                                let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                                create_call_history_message(
                                    message_service.clone(),
                                    &channel_id,
                                    "token-request-failed",
                                    &anonymous_user_id,
                                    &display_name,
                                    true, // This was an outgoing call
                                    0, // Failed call, no duration
                                    true, // Mark as missed/failed
                                );
                            }
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to get call room token: {}", e);
                    
                    // Create failed call history entry
                    let channel_id_str = format!("anon-dm-{}", anon_dm_id);
                    if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                        use crate::bin_ui::features::calls::call_history::create_call_history_message;
                        let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                        create_call_history_message(
                            message_service.clone(),
                            &channel_id,
                            "get-token-failed",
                            &anonymous_user_id,
                            &display_name,
                            true, // This was an outgoing call
                            0, // Failed call, no duration
                            true, // Mark as missed/failed
                        );
                    }
                    return;
                }
            };
            
            // Send anonymous call offer
            match anon_dm_service.send_call_offer(
                &anon_dm_id,
                &token,
                is_video_call,
                Some(&display_name),
            ).await {
                Ok(offer_result) => {
                    tracing::info!(
                        room_name = %offer_result.room_name,
                        "Sent anonymous call offer"
                    );
                    
                    // Mark token as used
                    let _ = anon_dm_service.mark_call_room_token_used(&token.id);
                    
                    // Store pending anonymous call state in the service
                    anon_dm_service.store_pending_anon_call(
                        offer_result.room_name.clone(),
                        chatter_core::services::anon_dm::PendingAnonCall {
                            call_room_id: offer_result.call_room_id,
                            room_name: offer_result.room_name.clone(),
                            conversation_id: anon_dm_id,
                            token,
                            is_video_call,
                            is_outgoing: true,
                            display_name: display_name.clone(),
                            encryption_key: offer_result.call_encryption_key,
                            sender_id: None,
                            started_at: None, // Will be set when call is accepted
                        },
                    );
                    
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
                            is_identity_verified: true, // Anonymous calls are always verified
                            identity_warning: "".into(),
                            participant_count: 1,
                            duration_seconds: 0,
                            participants: ModelRc::new(VecModel::from(vec![local_participant])),
                            verification_emoji_1: "🔒".into(),
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
                    tracing::error!("Failed to send anonymous call offer: {}", e);
                    
                    // Create failed call history entry
                    let channel_id_str = format!("anon-dm-{}", anon_dm_id);
                    if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                        use crate::bin_ui::features::calls::call_history::create_call_history_message;
                        let anonymous_user_id = chatter_core::domain::ids::UserId::new();
                        create_call_history_message(
                            message_service.clone(),
                            &channel_id,
                            "failed-call",
                            &anonymous_user_id,
                            &display_name,
                            true, // This was an outgoing call
                            0, // Failed call, no duration
                            true, // Mark as missed/failed
                        );
                    }
                    
                    // No pending state to clear - we only store it after successful offer
                    if let Some(w) = weak.upgrade() {
                        w.set_active_call_visible(false);
                    }
                }
            }
        } else {
            // Regular DM call with ephemeral key exchange
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
                    let content = chatter_core::services::build_call_key_offer(
                        &call.id.to_string(),
                        ephemeral_public,
                        call_type_str,
                        &channel_id.to_string(),
                    );

                    // Send via regular DM
                    let send_result = message_service.send_dm(&peer_id, content, &mailbox_manager).await
                        .map_err(|e| format!("{}", e));

                    if let Err(e) = send_result {
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
                let message_service_clone = message_service.clone();
                let channel_service_clone = channel_service.clone();
                let mailbox_manager_clone = mailbox_manager.clone();
                let call_id_str = call.id.to_string();
                let channel_id_str = channel_id_clone.to_string();
                let group_key_arr = group_key;

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
                                let content = chatter_core::services::build_group_call_key_content(
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

                    let verification = chatter_core::services::CallVerification::derive_verification_emojis(
                        &group_key,
                        &call.id.to_string(),
                    );
                    let (e1, e2, e3, e4) = chatter_core::services::CallVerification::to_slint_tuple(&verification);

                    let call_data = ActiveCallData {
                        call_id: call.id.to_string().into(),
                        channel_id: channel_id_str.into(),
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
/// Uses blind signature tokens for anonymous call rooms, same as anon DM calls.
/// The offer is sent to all group members via the group messaging channel.
#[allow(clippy::too_many_arguments)]
async fn start_private_group_call(
    weak: slint::Weak<MainWindow>,
    group_id_str: &str,
    display_name: &str,
    user_id: chatter_core::domain::ids::UserId,
    is_video_call: bool,
    call_type_str: &str,
    anon_dm_service: Arc<chatter_core::services::anon_dm::AnonDmService>,
    anon_group_service: Arc<chatter_core::services::anon_group::AnonGroupService>,
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

    // Get a blind signature token for the call
    let token = match anon_group_service.get_available_call_room_token() {
        Ok(Some(token)) => token,
        Ok(None) => {
            tracing::info!("No call room tokens available - requesting more");
            match anon_group_service.request_call_room_tokens(5).await {
                Ok(count) => {
                    tracing::info!("Successfully requested {} call room tokens", count);
                    match anon_group_service.get_available_call_room_token() {
                        Ok(Some(token)) => token,
                        _ => {
                            tracing::error!("Still no tokens available after request");
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to request call room tokens: {}", e);
                    return;
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to get call room token: {}", e);
            return;
        }
    };

    // Send call offer to all group members
    match anon_group_service.send_call_offer(
        &anon_group_id,
        &token,
        is_video_call,
        Some(display_name),
    ).await {
        Ok(offer_result) => {
            tracing::info!(
                room_name = %offer_result.room_name,
                "Sent anonymous group call offer"
            );

            // Mark token as used
            let _ = anon_group_service.mark_call_room_token_used(&token.id);

            // Store pending call state (reuse anon DM pending call storage)
            anon_dm_service.store_pending_anon_call(
                offer_result.room_name.clone(),
                chatter_core::services::anon_dm::PendingAnonCall {
                    call_room_id: offer_result.call_room_id,
                    room_name: offer_result.room_name.clone(),
                    // Use a synthetic AnonDmId from the group ID bytes for storage
                    conversation_id: chatter_core::domain::anon_dm::AnonDmId::try_from_slice(
                        anon_group_id.as_bytes()
                    ).expect("AnonGroupId is 16 bytes like AnonDmId"),
                    token,
                    is_video_call,
                    is_outgoing: true,
                    display_name: display_name.to_string(),
                    encryption_key: offer_result.call_encryption_key.clone(),
                    sender_id: None,
                    started_at: None,
                },
            );

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
                    verification_emoji_1: "🔒".into(),
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
            tracing::error!("Failed to send anonymous group call offer: {}", e);
            if let Some(w) = weak.upgrade() {
                w.set_active_call_visible(false);
            }
        }
    }
}
