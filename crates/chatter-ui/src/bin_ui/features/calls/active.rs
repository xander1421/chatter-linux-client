//! Active call controls.
//!
//! Handles mute, video toggle, screen share, and call end.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_linux::app::UIContext;
#[cfg(not(target_os = "android"))]
use chatter_linux::calls::ScreenCapture;
use chatter_linux::inbox::handlers::DomainEvent;
use chatter_linux::state::ActiveConversation;
use crate::MainWindow;

use super::call_history::create_call_history_message;
use super::timers::{get_call_duration_secs, stop_call_timer, stop_video_frame_timer};

/// Wire active call control callbacks.
pub fn wire_active_call(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let call_service = ui_ctx.call_service.clone();

    // Toggle mute
    window.on_call_toggle_mute({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_muted = service.is_microphone_muted().await;
                    let new_state = !is_muted;
                    service.set_microphone_muted(new_state);
                    tracing::info!("Microphone muted: {}", new_state);

                    if let Some(w) = weak.upgrade() {
                        w.set_call_is_muted(new_state);
                    }
                }
            }).unwrap();
        }
    });

    // Toggle video
    window.on_call_toggle_video({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_enabled = service.is_camera_enabled().await;
                    let new_state = !is_enabled;
                    service.set_camera_enabled(new_state);
                    tracing::info!("Camera enabled: {}", new_state);

                    if let Some(w) = weak.upgrade() {
                        w.set_call_is_video_enabled(new_state);
                    }
                }
            }).unwrap();
        }
    });

    // Toggle screen share (desktop only)
    window.on_call_toggle_screen_share({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let _call_service = call_service.clone();
            let _weak = weak.clone();
            #[cfg(not(target_os = "android"))]
            slint::spawn_local(async move {
                if let Some(service) = _call_service.get() {
                    let is_sharing = service.is_screen_sharing().await;
                    let new_state = !is_sharing;

                    if new_state {
                        // Start screen share - use primary display
                        match ScreenCapture::primary_source() {
                            Ok(source) => {
                                if let Err(e) = service.publish_screen_share(source).await {
                                    tracing::error!("Failed to start screen share: {}", e);
                                } else {
                                    tracing::info!("Screen share started");
                                    if let Some(w) = _weak.upgrade() {
                                        w.set_call_is_screen_sharing(true);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("No screen source available: {}", e);
                            }
                        }
                    } else {
                        // Stop screen share
                        service.stop_screen_share();
                        tracing::info!("Screen share stopped");
                        if let Some(w) = _weak.upgrade() {
                            w.set_call_is_screen_sharing(false);
                        }
                    }
                }
            }).unwrap();
            #[cfg(target_os = "android")]
            tracing::warn!("Screen sharing not available on Android");
        }
    });

    let weak = window.as_weak();
    let message_service = ui_ctx.message_service.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let session_service = ui_ctx.session_service.clone();
    let channel_service = ui_ctx.channel_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let chat_state = ui_ctx.chat_state.clone();

    // End call
    window.on_call_end({
        let call_service = call_service.clone();
        let message_service = message_service.clone();
        let mailbox_manager = mailbox_manager.clone();
        let session_service = session_service.clone();
        let channel_service = channel_service.clone();
        let user_service = user_service.clone();
        let anon_dm_service = anon_dm_service.clone();
        let chat_state = chat_state.clone();

        move || {
            tracing::info!("End call");

            let weak = weak.clone();
            let call_service = call_service.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let session_service = session_service.clone();
            let channel_service = channel_service.clone();
            let user_service = user_service.clone();
            let anon_dm_service_clone = anon_dm_service.clone();
            let conversation = chat_state.active_conversation();
            
            // Get call info before the async block
            let (call_id_str, is_anonymous) = if let Some(w) = weak.upgrade() {
                let data = w.get_active_call_data();
                (data.call_id.to_string(), data.is_anonymous_call)
            } else {
                (String::new(), false)
            };

            slint::spawn_local(async move {
                // IMPORTANT: Update UI immediately BEFORE async operations
                // This prevents the modal from staying visible if network calls block
                stop_call_timer();
                stop_video_frame_timer();
                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                }

                // Check if this is an anonymous call
                if is_anonymous {
                    // Try to get the pending call from service
                    if let Some(pending_anon) = anon_dm_service_clone.take_pending_anon_call(&call_id_str) {
                    // Send anonymous call hangup
                    tracing::info!(
                        conversation_id = %pending_anon.conversation_id,
                        room_name = %pending_anon.room_name,
                        "Sending anonymous call hangup"
                    );
                    
                    if let Err(e) = anon_dm_service_clone.send_call_hangup(
                        &pending_anon.conversation_id,
                        &pending_anon.call_room_id,
                        chatter_linux::proto::content_v1::anon_call_hangup::Reason::Normal,
                    ).await {
                        tracing::error!(
                            "Failed to send anonymous call hangup: {}", e
                        );
                    }
                    
                    // Store call history in anon DM conversation
                    let call_duration = if let Some(started_at) = pending_anon.started_at {
                        (chrono::Utc::now() - started_at).num_seconds()
                    } else {
                        get_call_duration_secs() as i64
                    };
                    let duration_str = if call_duration > 0 {
                        format!("{}:{:02}", call_duration / 60, call_duration % 60)
                    } else {
                        "0:00".to_string()
                    };
                    let call_type_label = if pending_anon.is_outgoing { "Outgoing call" } else { "Incoming call" };
                    let call_text = format!("📞 {} · {}", call_type_label, duration_str);

                    // Store in anon DM message store
                    let sent_at = super::timers::current_time_ns();
                    let our_user_id = session_service.current_user_id().unwrap_or_else(chatter_linux::domain::ids::UserId::new);
                    if let Err(e) = anon_dm_service_clone.store_message(
                        &pending_anon.conversation_id,
                        &our_user_id,
                        0, // text content type
                        &call_text,
                        sent_at,
                    ) {
                        tracing::error!("Failed to store anon call history: {}", e);
                    }

                    // Push call bubble to UI if viewing this conversation
                    let is_viewing = matches!(conversation, ActiveConversation::AnonDm(id) if id == pending_anon.conversation_id);
                    if is_viewing {
                        if let Some(w) = weak.upgrade() {
                            let messages = w.get_messages();
                            if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
                                let msg_item = crate::MessageItemData {
                                    message_id: uuid::Uuid::new_v4().to_string().into(),
                                    sender_id: our_user_id.to_string().into(),
                                    sender_name: "".into(),
                                    sender_initials: "".into(),
                                    sender_avatar_url: "".into(),
                                    sender_avatar_source: slint::Image::default(),
                                    has_avatar: false,
                                    content: call_text.into(),
                                    timestamp: chrono::Utc::now().format("%H:%M").to_string().into(),
                                    timestamp_ns: sent_at.to_string().into(),
                                    is_own: true,
                                    is_edited: false,
                                    is_deleted: false,
                                    is_system: true,
                                    is_ai: false,
                                    has_attachments: false,
                                    attachment_count: 0,
                                    attachments: slint::ModelRc::default(),
                                    has_reactions: false,
                                    reaction_summary: "".into(),
                                    reactions: ModelRc::default(),
                                    reply_count: 0,
                                    reply_count_text: "".into(),
                                    is_thread_reply: false,
                                    is_reply: false,
                                    reply_to_sender_name: "".into(),
                                    reply_to_preview: "".into(),
                                    show_sender: false,
                                    delivery_status: "".into(),
                                    is_pinned: false,
                                    is_call: true,
                                    call_type: call_type_label.into(),
                                    call_duration: duration_str.into(),
                                };
                                model.push(msg_item);
                            }
                        }
                    }

                    // State already consumed by take_pending_anon_call
                    return; // Don't process regular call logic
                    }
                }
                
                if let Some(service) = call_service.get() {
                    // Get call info before leaving
                    let call_info = service.active_call().await;

                    // Send hangup notification for DM calls
                    if let Some(ref call) = call_info {
                        let channel_id = &call.channel_id;

                        // Get current user ID
                        if let Some(user_id) = session_service.current_user_id() {
                            // Check if this is a DM call (use channel service)
                            if let Ok(Some(peer_id)) = channel_service.get_dm_peer_local(channel_id) {
                                tracing::info!(
                                    peer_id = %peer_id,
                                    call_id = %call.id,
                                    "Sending call hangup notification"
                                );

                                // Build hangup content
                                let content = chatter_linux::services::build_call_hangup_content(
                                    &call.id.to_string(),
                                    chatter_linux::services::CallHangupReason::Normal,
                                );

                                // Send via sealed sender
                                if let Err(e) = message_service
                                    .send_dm(&peer_id, content, &mailbox_manager)
                                    .await
                                {
                                    tracing::error!(
                                        peer_id = %peer_id,
                                        error = %e,
                                        "Failed to send hangup notification"
                                    );
                                } else {
                                    tracing::info!(
                                        peer_id = %peer_id,
                                        call_id = %call.id,
                                        "Hangup notification sent successfully"
                                    );
                                }
                            }

                            // Create call history message
                            let is_outgoing = call.initiator_id == user_id;
                            let duration_secs = get_call_duration_secs();

                            // Get initiator display name (via user service)
                            let initiator_name = user_service
                                .get_user_local(&call.initiator_id)
                                .ok()
                                .flatten()
                                .map(|u| u.username.clone())
                                .unwrap_or_else(|| call.initiator_id.to_string());

                            create_call_history_message(
                                message_service.clone(),
                                channel_id,
                                &call.id.to_string(),
                                &call.initiator_id,
                                &initiator_name,
                                is_outgoing,
                                duration_secs,
                                false, // not missed - call was answered and ended normally
                            );
                        }
                    }

                    // Leave the call (async cleanup - UI already updated above)
                    match service.leave_call().await {
                        Ok(()) => {
                            tracing::info!("Left call successfully");
                        }
                        Err(e) => {
                            tracing::error!("Failed to leave call: {}", e);
                        }
                    }
                }
            }).unwrap();
        }
    });
}

/// Wire call hangup handler to process remote hangup events.
///
/// Subscribes to `DomainEvent::CallHangupReceived` from the event bus
/// and calls `leave_call()` on the CallService to disconnect from LiveKit.
pub fn wire_call_hangup_handler(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let call_service = ui_ctx.call_service.clone();
    let mut event_rx = ui_ctx.event_bus.subscribe_domain();
    let weak = window.as_weak();
    let message_service = ui_ctx.message_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let session_service = ui_ctx.session_service.clone();

    // Spawn a task to handle incoming hangup events
    // Note: This runs in a slint::spawn_local context which keeps CallService on UI thread
    slint::spawn_local(async move {
        while let Ok(event) = event_rx.recv().await {
            if let DomainEvent::CallHangupReceived { call_id, peer_id, reason } = event {
                tracing::info!(
                    call_id = %call_id,
                    peer_id = %peer_id,
                    reason = %reason,
                    "Processing remote call hangup"
                );

                // Get call duration BEFORE stopping the timer
                let duration_secs = get_call_duration_secs();

                // Get CallService and disconnect locally
                // IMPORTANT: Don't call leave_call() here - that would make a server
                // request which can block/timeout since the peer already ended the call.
                // Just disconnect locally from LiveKit.
                if let Some(service) = call_service.get() {
                    // Get call info BEFORE disconnecting (for call history message)
                    let call_info = service.active_call().await;

                    // Verify this hangup is for our active call
                    let is_our_call = call_info.as_ref()
                        .map(|c| c.id.to_string() == call_id)
                        .unwrap_or(false);

                    if is_our_call {
                        // Create call history message before disconnecting
                        if let Some(ref call) = call_info {
                            // Get current user ID to determine if this was incoming
                            if let Some(current_user_id) = session_service.current_user_id() {
                                // Incoming call = we didn't initiate it
                                let is_outgoing = call.initiator_id == current_user_id;

                                // Get initiator display name (via user service)
                                let initiator_name = user_service
                                    .get_user_local(&call.initiator_id)
                                    .ok()
                                    .flatten()
                                    .map(|u| u.username.clone())
                                    .unwrap_or_else(|| call.initiator_id.to_string());

                                create_call_history_message(
                                    message_service.clone(),
                                    &call.channel_id,
                                    &call.id.to_string(),
                                    &call.initiator_id,
                                    &initiator_name,
                                    is_outgoing,
                                    duration_secs,
                                    false, // not missed - peer hung up after call was connected
                                );
                            }
                        }

                        // Disconnect locally without calling server (peer already ended)
                        service.disconnect_local().await;
                        tracing::info!("Disconnected locally after peer hangup");
                    } else {
                        tracing::debug!(
                            call_id = %call_id,
                            "Ignoring hangup for non-active call"
                        );
                    }
                }

                // Stop the call timer and video frame timer, hide the call UI
                // IMPORTANT: Must stop timers to prevent calloop event loop issues
                stop_call_timer();
                stop_video_frame_timer();

                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                    w.set_incoming_call_visible(false);
                }
            }
        }
    }).unwrap();
}
