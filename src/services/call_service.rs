//! Call service.
//!
//! Orchestrates call lifecycle: server API + LiveKit + E2EE.

// LiveKit types don't implement Send+Sync, and MutexGuard across await is intentional
#![allow(clippy::arc_with_non_send_sync)]
#![allow(clippy::await_holding_lock)]

use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use parking_lot::RwLock;

use crate::calls::{RoomManager, RoomState, LiveKitRoomEvent};
use crate::crypto::call_key_provider::CallKeyState;
use crate::domain::call::{Call, CallType, Participant};
use crate::domain::ids::{CallId, ChannelId, UserId};
use crate::error::CallError;
use crate::network::grpc::CallClient;
use crate::proto::chat_v1::{self, CallType as ProtoCallType};

/// Call service for managing voice/video calls.
pub struct CallService {
    /// gRPC client for server communication.
    call_client: Arc<RwLock<CallClient>>,
    /// LiveKit room manager.
    room_manager: Arc<RoomManager>,
    /// Current active call (if any).
    active_call: Arc<RwLock<Option<ActiveCall>>>,
    /// Room event receiver.
    room_events_rx: Arc<RwLock<Option<mpsc::UnboundedReceiver<LiveKitRoomEvent>>>>,
}

/// State of the currently active call.
#[allow(dead_code)] // Fields stored for key ratcheting and reconnection
struct ActiveCall {
    /// Call domain object.
    call: Call,
    /// E2EE key state (for key ratcheting during long calls).
    key_state: CallKeyState,
    /// LiveKit room name (for reconnection).
    room_name: String,
    /// LiveKit URL (for reconnection).
    livekit_url: String,
}

impl CallService {
    /// Create a new CallService.
    pub fn new(call_client: CallClient) -> Self {
        let (room_manager, room_events_rx) = RoomManager::new();
        Self {
            call_client: Arc::new(RwLock::new(call_client)),
            room_manager: Arc::new(room_manager),
            active_call: Arc::new(RwLock::new(None)),
            room_events_rx: Arc::new(RwLock::new(Some(room_events_rx))),
        }
    }

    /// Take the room events receiver (can only be called once).
    pub fn take_room_events(&self) -> Option<mpsc::UnboundedReceiver<LiveKitRoomEvent>> {
        self.room_events_rx.write().take()
    }

    /// Initiate a new call in a channel.
    ///
    /// # Arguments
    /// * `channel_id` - Channel to start call in
    /// * `title` - Call title
    /// * `call_type` - Voice or Video
    /// * `encryption_root_key` - Root key from Signal session for E2EE
    /// * `initiator_id` - User ID of the caller
    pub async fn initiate_call(
        &self,
        channel_id: ChannelId,
        title: &str,
        call_type: CallType,
        encryption_root_key: &[u8; 32],
        initiator_id: UserId,
    ) -> Result<Call, CallError> {
        // Check if already in a call
        if self.active_call.read().is_some() {
            return Err(CallError::AlreadyInCall);
        }

        // Convert call type to proto
        let proto_type = match call_type {
            CallType::Voice => ProtoCallType::Voice,
            CallType::Video => ProtoCallType::Video,
            CallType::Group => ProtoCallType::Video, // Groups use video capability
        };

        // Create call on server
        let response = {
            let mut client = self.call_client.write();
            client
                .create_call(&channel_id.to_string(), title, proto_type, None)
                .await
                .map_err(|e| CallError::ServerError(e.to_string()))?
        };

        // Derive E2EE key
        let key_state = CallKeyState::new(encryption_root_key, &response.call_id)
            .map_err(|e| CallError::CryptoError(e.to_string()))?;

        // Connect to LiveKit room with encryption
        self.room_manager
            .connect(&response.livekit_url, &response.livekit_token, key_state.current_key_vec())
            .await?;

        // Create domain call object
        let call_id = CallId::from_str(&response.call_id)
            .map_err(|_| CallError::InvalidCallId)?;

        let mut call = Call::new(channel_id, initiator_id, call_type);
        call.id = call_id;
        call.start();

        // Add initiator as participant
        call.add_participant(Participant::new(initiator_id));

        // Store active call
        {
            let mut active = self.active_call.write();
            *active = Some(ActiveCall {
                call: call.clone(),
                key_state,
                room_name: response.room_name,
                livekit_url: response.livekit_url,
            });
        }

        Ok(call)
    }

    /// Join an existing call.
    ///
    /// # Arguments
    /// * `call_id` - Call to join
    /// * `encryption_root_key` - Root key from Signal session for E2EE
    /// * `joiner_id` - User ID of the person joining
    pub async fn join_call(
        &self,
        call_id: CallId,
        encryption_root_key: &[u8; 32],
        joiner_id: UserId,
    ) -> Result<Call, CallError> {
        // Check if already in a call
        if self.active_call.read().is_some() {
            return Err(CallError::AlreadyInCall);
        }

        // Join call on server
        let response = {
            let mut client = self.call_client.write();
            client
                .join_call(&call_id.to_string())
                .await
                .map_err(|e| CallError::ServerError(e.to_string()))?
        };

        // Derive E2EE key (must match initiator's key)
        let key_state = CallKeyState::new(encryption_root_key, &call_id.to_string())
            .map_err(|e| CallError::CryptoError(e.to_string()))?;

        // Connect to LiveKit room
        self.room_manager
            .connect(&response.livekit_url, &response.livekit_token, key_state.current_key_vec())
            .await?;

        // Convert proto CallInfo to domain Call
        let call_info = response.call_info.ok_or(CallError::NotFound)?;
        let call = self.proto_call_info_to_domain(&call_info, joiner_id)?;

        // Store active call
        {
            let mut active = self.active_call.write();
            *active = Some(ActiveCall {
                call: call.clone(),
                key_state,
                room_name: call_info.room_name.clone(),
                livekit_url: response.livekit_url,
            });
        }

        Ok(call)
    }

    /// Leave the current call.
    pub async fn leave_call(&self) -> Result<(), CallError> {
        let call_id = {
            let active = self.active_call.read();
            active.as_ref()
                .map(|a| a.call.id.to_string())
                .ok_or(CallError::NotInCall)?
        };

        // Leave on server with timeout to prevent blocking
        // If server is slow/unreachable, we still want to disconnect locally
        {
            let mut client = self.call_client.write();
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(3),
                client.leave_call(&call_id)
            ).await {
                Ok(Ok(())) => {
                    tracing::debug!("Server acknowledged call leave");
                }
                Ok(Err(e)) => {
                    // Server error - log but continue with local disconnect
                    tracing::warn!("Server leave_call error (continuing): {}", e);
                }
                Err(_) => {
                    // Timeout - log but continue with local disconnect
                    tracing::warn!("Server leave_call timed out (continuing with local disconnect)");
                }
            }
        }

        // Disconnect from LiveKit (already has internal timeout)
        self.room_manager.disconnect().await;

        // Clear active call
        *self.active_call.write() = None;

        Ok(())
    }

    /// End the current call (creator/admin only).
    pub async fn end_call(&self, reason: Option<&str>) -> Result<(), CallError> {
        let call_id = {
            let active = self.active_call.read();
            active.as_ref()
                .map(|a| a.call.id.to_string())
                .ok_or(CallError::NotInCall)?
        };

        // End on server
        {
            let mut client = self.call_client.write();
            client.end_call(&call_id, reason.unwrap_or(""))
                .await
                .map_err(|e| CallError::ServerError(e.to_string()))?;
        }

        // Disconnect from LiveKit
        self.room_manager.disconnect().await;

        // Clear active call
        *self.active_call.write() = None;

        Ok(())
    }

    /// Disconnect locally from LiveKit without notifying the server.
    ///
    /// Use this when receiving a remote hangup notification - the peer already
    /// told us they're ending the call, so we just need to disconnect locally.
    /// Calling the server would be redundant and could block/timeout.
    pub async fn disconnect_local(&self) {
        // Disconnect from LiveKit
        self.room_manager.disconnect().await;

        // Clear active call
        *self.active_call.write() = None;

        tracing::info!("Disconnected locally from call (no server notification)");
    }

    /// Get the currently active call.
    pub fn active_call(&self) -> Option<Call> {
        self.active_call.read().as_ref().map(|a| a.call.clone())
    }

    /// Check if currently in a call.
    pub fn is_in_call(&self) -> bool {
        self.active_call.read().is_some()
    }

    /// Get room connection state.
    pub fn room_state(&self) -> RoomState {
        self.room_manager.state()
    }

    /// Get the room manager for track publishing.
    pub fn room_manager(&self) -> Arc<RoomManager> {
        self.room_manager.clone()
    }

    /// Publish microphone audio to the current call.
    pub async fn publish_microphone(&self, device_id: Option<&str>) -> Result<(), CallError> {
        self.room_manager.publish_microphone(device_id).await
    }

    /// Publish camera video to the current call.
    pub async fn publish_camera(&self, device_path: Option<&str>) -> Result<(), CallError> {
        self.room_manager.publish_camera(device_path, None).await
    }

    /// Stop microphone capture.
    pub fn stop_microphone(&self) {
        self.room_manager.stop_microphone();
    }

    /// Stop camera capture.
    pub fn stop_camera(&self) {
        self.room_manager.stop_camera();
    }

    /// Toggle microphone mute.
    pub fn set_microphone_muted(&self, muted: bool) {
        self.room_manager.set_microphone_muted(muted);
    }

    /// Check if microphone is muted.
    pub fn is_microphone_muted(&self) -> bool {
        self.room_manager.is_microphone_muted()
    }

    /// Toggle camera enabled.
    pub fn set_camera_enabled(&self, enabled: bool) {
        self.room_manager.set_camera_enabled(enabled);
    }

    /// Check if camera is enabled.
    pub fn is_camera_enabled(&self) -> bool {
        self.room_manager.is_camera_enabled()
    }

    fn proto_call_info_to_domain(&self, info: &chat_v1::CallInfo, joiner_id: UserId) -> Result<Call, CallError> {
        let call_id = CallId::from_str(&info.id)
            .map_err(|_| CallError::InvalidCallId)?;
        let channel_id = ChannelId::from_str(&info.channel_id)
            .map_err(|_| CallError::InvalidChannelId)?;
        let initiator_id = UserId::from_str(&info.created_by)
            .map_err(|_| CallError::InvalidUserId)?;

        let call_type = match ProtoCallType::try_from(info.r#type).unwrap_or(ProtoCallType::Unspecified) {
            ProtoCallType::Voice => CallType::Voice,
            ProtoCallType::Video | ProtoCallType::ScreenShare => CallType::Video,
            _ => CallType::Voice,
        };

        let mut call = Call::new(channel_id, initiator_id, call_type);
        call.id = call_id;
        call.start();

        // Add the joiner as a participant
        call.add_participant(Participant::new(joiner_id));

        Ok(call)
    }
}
