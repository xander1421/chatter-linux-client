//! Call UI callback wiring.
//!
//! Wires call-related Slint callbacks to CallService.

use std::str::FromStr;
use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use chatter_linux::domain::call::CallType;
use chatter_linux::domain::ids::{CallId, ChannelId, UserId};
use chatter_linux::services::CallService;
use crate::MainWindow;

/// Wire all call-related callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_start_call(window, ctx);
    wire_incoming_call(window, ctx);
    wire_active_call(window, ctx);
}

fn wire_start_call(window: &MainWindow, ctx: &AppContext) {
    let weak = window.as_weak();
    let ctx_grpc = ctx.grpc_pool.clone();
    let ctx_settings = ctx.settings_repo.clone();

    // Start voice call
    window.on_start_voice_call({
        let weak = weak.clone();
        let grpc = ctx_grpc.clone();
        let settings = ctx_settings.clone();

        move || {
            let weak = weak.clone();
            let grpc = grpc.clone();
            let settings = settings.clone();

            slint::spawn_local(async move {
                start_call_impl(weak, grpc, settings, CallType::Voice).await;
            }).unwrap();
        }
    });

    // Start video call
    window.on_start_video_call({
        let weak = weak.clone();
        let grpc = ctx_grpc.clone();
        let settings = ctx_settings.clone();

        move || {
            let weak = weak.clone();
            let grpc = grpc.clone();
            let settings = settings.clone();

            slint::spawn_local(async move {
                start_call_impl(weak, grpc, settings, CallType::Video).await;
            }).unwrap();
        }
    });
}

async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    grpc: std::sync::Arc<chatter_linux::network::grpc::GrpcClientPool>,
    settings: std::sync::Arc<chatter_linux::storage::SettingsRepository>,
    call_type: CallType,
) {
    // Get current channel ID from the window
    let channel_id_str = if let Some(w) = weak.upgrade() {
        w.get_channel_id().to_string()
    } else {
        return;
    };

    if channel_id_str.is_empty() {
        tracing::warn!("No channel selected for call");
        return;
    }

    let channel_id = match ChannelId::from_str(&channel_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid channel ID: {}", e);
            return;
        }
    };

    // Get current user ID
    let user_id_str = match settings.get_string(chatter_linux::storage::settings_keys::CURRENT_USER_ID) {
        Ok(Some(id)) => id,
        _ => {
            tracing::error!("No user ID available");
            return;
        }
    };

    let user_id = match UserId::from_str(&user_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid user ID: {}", e);
            return;
        }
    };

    // Create CallService
    let call_client = chatter_linux::network::grpc::CallClient::new(
        grpc.channel(),
        grpc.auth_clone(),
    );
    let call_service = CallService::new(call_client);

    // For now, use a zeroed key - proper key derivation requires Signal session lookup
    // TODO: Derive encryption key from Signal session
    let encryption_key = [0u8; 32];

    let call_type_str = match call_type {
        CallType::Voice => "Voice",
        CallType::Video => "Video",
        CallType::Group => "Group",
    };

    tracing::info!("Starting {} call in channel {}", call_type_str, channel_id);

    match call_service.initiate_call(
        channel_id,
        call_type_str,
        call_type,
        &encryption_key,
        user_id,
    ).await {
        Ok(call) => {
            tracing::info!("Started call: {:?}", call.id);
            if let Some(w) = weak.upgrade() {
                w.set_active_call_visible(true);
            }
        }
        Err(e) => {
            tracing::error!("Failed to start call: {}", e);
        }
    }
}

fn wire_incoming_call(window: &MainWindow, ctx: &AppContext) {
    let weak = window.as_weak();
    let ctx_grpc = ctx.grpc_pool.clone();
    let ctx_settings = ctx.settings_repo.clone();

    // Accept call callback
    window.on_accept_incoming_call(move |call_id| {
        let call_id_str = call_id.to_string();
        tracing::info!("Accept call: {}", call_id_str);

        let weak = weak.clone();
        let grpc = ctx_grpc.clone();
        let settings = ctx_settings.clone();

        slint::spawn_local(async move {
            // Parse call ID
            let call_id = match CallId::from_str(&call_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid call ID: {}", e);
                    return;
                }
            };

            // Get current user ID
            let user_id_str = match settings.get_string(chatter_linux::storage::settings_keys::CURRENT_USER_ID) {
                Ok(Some(id)) => id,
                _ => {
                    tracing::error!("No user ID available");
                    return;
                }
            };

            let user_id = match UserId::from_str(&user_id_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    return;
                }
            };

            // Create CallService (we need a fresh client for each operation)
            let call_client = chatter_linux::network::grpc::CallClient::new(
                grpc.channel(),
                grpc.auth_clone(),
            );
            let call_service = CallService::new(call_client);

            // For now, use a zeroed key - proper key derivation requires Signal session lookup
            // TODO: Derive encryption key from Signal session with call initiator
            let encryption_key = [0u8; 32];

            match call_service.join_call(call_id, &encryption_key, user_id).await {
                Ok(call) => {
                    tracing::info!("Joined call: {:?}", call.id);
                    if let Some(w) = weak.upgrade() {
                        w.set_incoming_call_visible(false);
                        w.set_active_call_visible(true);
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
    });

    let weak = window.as_weak();

    // Decline call callback
    window.on_decline_incoming_call(move |call_id| {
        let call_id_str = call_id.to_string();
        tracing::info!("Decline call: {}", call_id_str);

        // Just hide the modal - server will timeout the call
        // TODO: Notify server that call was declined
        if let Some(w) = weak.upgrade() {
            w.set_incoming_call_visible(false);
        }
    });
}

fn wire_active_call(window: &MainWindow, ctx: &AppContext) {
    let call_service = ctx.call_service.clone();

    // Toggle mute
    window.on_call_toggle_mute({
        let call_service = call_service.clone();
        move || {
            tracing::info!("Toggle mute");
            if let Some(service) = call_service.get() {
                // RoomManager handles audio track muting
                let room_state = service.room_state();
                tracing::debug!("Room state: {:?}", room_state);
                // TODO: Wire to RoomManager.toggle_audio_mute()
            }
        }
    });

    // Toggle video
    window.on_call_toggle_video({
        let call_service = call_service.clone();
        move || {
            tracing::info!("Toggle video");
            if let Some(service) = call_service.get() {
                let room_state = service.room_state();
                tracing::debug!("Room state: {:?}", room_state);
                // TODO: Wire to RoomManager.toggle_video()
            }
        }
    });

    // Toggle screen share
    window.on_call_toggle_screen_share({
        let call_service = call_service.clone();
        move || {
            tracing::info!("Toggle screen share");
            if let Some(service) = call_service.get() {
                let room_state = service.room_state();
                tracing::debug!("Room state: {:?}", room_state);
                // TODO: Wire to RoomManager.toggle_screen_share()
            }
        }
    });

    let weak = window.as_weak();

    // End call
    window.on_call_end({
        let call_service = call_service.clone();
        move || {
            tracing::info!("End call");

            let weak = weak.clone();
            let call_service = call_service.clone();

            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    match service.leave_call().await {
                        Ok(()) => {
                            tracing::info!("Left call successfully");
                        }
                        Err(e) => {
                            tracing::error!("Failed to leave call: {}", e);
                        }
                    }
                }

                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                }
            }).unwrap();
        }
    });
}
