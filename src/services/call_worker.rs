//! Call worker - dedicated thread for non-Send LiveKit operations.
//!
//! LiveKit types don't implement Send/Sync, so all call operations must run
//! on a single dedicated thread. This module provides a Send-safe handle that
//! communicates with the worker via channels.

use std::str::FromStr;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot};

use crate::calls::{LiveKitRoomEvent, RoomManager, RoomState, ScreenSource, VideoCaptureConfig};
use crate::crypto::call_key_provider::CallKeyState;
use crate::domain::call::{Call, CallType, Participant};
use crate::domain::ids::{CallId, ChannelId, UserId};
use crate::error::CallError;
use crate::network::grpc::CallClient;
use crate::proto::chat_v1::CallType as ProtoCallType;

/// Commands sent to the call worker thread.
enum CallCommand {
    /// Initiate a new call.
    InitiateCall {
        channel_id: ChannelId,
        title: String,
        call_type: CallType,
        encryption_root_key: [u8; 32],
        initiator_id: UserId,
        response: oneshot::Sender<Result<Call, CallError>>,
    },
    /// Join an existing call.
    JoinCall {
        call_id: CallId,
        encryption_root_key: [u8; 32],
        joiner_id: UserId,
        response: oneshot::Sender<Result<Call, CallError>>,
    },
    /// Leave the current call.
    LeaveCall {
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// End the current call (creator only).
    EndCall {
        reason: Option<String>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Disconnect locally without server notification.
    DisconnectLocal {
        response: oneshot::Sender<()>,
    },
    /// Get active call info.
    GetActiveCall {
        response: oneshot::Sender<Option<Call>>,
    },
    /// Check if in a call.
    IsInCall {
        response: oneshot::Sender<bool>,
    },
    /// Get room state.
    GetRoomState {
        response: oneshot::Sender<RoomState>,
    },
    /// Publish microphone.
    PublishMicrophone {
        device_id: Option<String>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Publish camera.
    PublishCamera {
        device_path: Option<String>,
        config: Option<VideoCaptureConfig>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Stop microphone.
    StopMicrophone,
    /// Stop camera.
    StopCamera,
    /// Set microphone muted.
    SetMicrophoneMuted { muted: bool },
    /// Get microphone muted state.
    IsMicrophoneMuted {
        response: oneshot::Sender<bool>,
    },
    /// Set camera enabled.
    SetCameraEnabled { enabled: bool },
    /// Get camera enabled state.
    IsCameraEnabled {
        response: oneshot::Sender<bool>,
    },
    /// Publish screen share.
    PublishScreenShare {
        source: ScreenSource,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Stop screen share.
    StopScreenShare,
    /// Check if screen sharing.
    IsScreenSharing {
        response: oneshot::Sender<bool>,
    },
    /// Ratchet the encryption key for forward secrecy.
    RatchetKey {
        response: oneshot::Sender<Option<u32>>,
    },
    /// Set encryption key externally (from CallKeyState ratchet).
    SetEncryptionKey {
        key: Vec<u8>,
        response: oneshot::Sender<bool>,
    },
    /// Get current key epoch.
    GetKeyEpoch {
        response: oneshot::Sender<u32>,
    },
    /// Attempt reconnection to the call.
    Reconnect {
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Shutdown the worker.
    Shutdown,
}

/// Send-safe handle to the call worker.
///
/// This can be cloned and used from any thread. All operations are forwarded
/// to the dedicated worker thread via channels.
#[derive(Clone)]
pub struct CallWorkerHandle {
    cmd_tx: mpsc::UnboundedSender<CallCommand>,
    /// Room events forwarded from the worker.
    room_events_rx: Arc<RwLock<Option<mpsc::UnboundedReceiver<LiveKitRoomEvent>>>>,
}

impl CallWorkerHandle {
    /// Spawn a new call worker on a dedicated thread.
    pub fn spawn(call_client: CallClient) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        // Spawn dedicated thread for call operations
        std::thread::Builder::new()
            .name("call-worker".into())
            .spawn(move || {
                run_call_worker(cmd_rx, events_tx, call_client);
            })
            .expect("Failed to spawn call worker thread");

        Self {
            cmd_tx,
            room_events_rx: Arc::new(RwLock::new(Some(events_rx))),
        }
    }

    /// Take the room events receiver (can only be called once).
    pub fn take_room_events(&self) -> Option<mpsc::UnboundedReceiver<LiveKitRoomEvent>> {
        self.room_events_rx.write().take()
    }

    /// Initiate a new call.
    pub async fn initiate_call(
        &self,
        channel_id: ChannelId,
        title: &str,
        call_type: CallType,
        encryption_root_key: &[u8; 32],
        initiator_id: UserId,
    ) -> Result<Call, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::InitiateCall {
                channel_id,
                title: title.to_string(),
                call_type,
                encryption_root_key: *encryption_root_key,
                initiator_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Join an existing call.
    pub async fn join_call(
        &self,
        call_id: CallId,
        encryption_root_key: &[u8; 32],
        joiner_id: UserId,
    ) -> Result<Call, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::JoinCall {
                call_id,
                encryption_root_key: *encryption_root_key,
                joiner_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Leave the current call.
    pub async fn leave_call(&self) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::LeaveCall { response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// End the current call (creator only).
    pub async fn end_call(&self, reason: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::EndCall {
                reason: reason.map(|s| s.to_string()),
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Disconnect locally without server notification.
    pub async fn disconnect_local(&self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(CallCommand::DisconnectLocal { response: tx });
        let _ = rx.await;
    }

    /// Get the currently active call.
    pub async fn active_call(&self) -> Option<Call> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::GetActiveCall { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Get active call synchronously (best effort, may return stale data).
    /// For UI that needs immediate response without blocking.
    pub fn active_call_sync(&self) -> Option<Call> {
        // Fire-and-forget query - UI should use events for real-time updates
        None
    }

    /// Check if currently in a call.
    pub async fn is_in_call(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::IsInCall { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Get room connection state.
    pub async fn room_state(&self) -> RoomState {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::GetRoomState { response: tx }).is_err() {
            return RoomState::Disconnected;
        }
        rx.await.unwrap_or(RoomState::Disconnected)
    }

    /// Publish microphone audio.
    pub async fn publish_microphone(&self, device_id: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::PublishMicrophone {
                device_id: device_id.map(|s| s.to_string()),
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Publish camera video.
    pub async fn publish_camera(&self, device_path: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::PublishCamera {
                device_path: device_path.map(|s| s.to_string()),
                config: None,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Stop microphone capture.
    pub fn stop_microphone(&self) {
        let _ = self.cmd_tx.send(CallCommand::StopMicrophone);
    }

    /// Stop camera capture.
    pub fn stop_camera(&self) {
        let _ = self.cmd_tx.send(CallCommand::StopCamera);
    }

    /// Set microphone muted.
    pub fn set_microphone_muted(&self, muted: bool) {
        let _ = self.cmd_tx.send(CallCommand::SetMicrophoneMuted { muted });
    }

    /// Check if microphone is muted.
    pub async fn is_microphone_muted(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::IsMicrophoneMuted { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Set camera enabled.
    pub fn set_camera_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(CallCommand::SetCameraEnabled { enabled });
    }

    /// Check if camera is enabled.
    pub async fn is_camera_enabled(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::IsCameraEnabled { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Publish screen share.
    pub async fn publish_screen_share(&self, source: ScreenSource) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::PublishScreenShare {
                source,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Stop screen share.
    pub fn stop_screen_share(&self) {
        let _ = self.cmd_tx.send(CallCommand::StopScreenShare);
    }

    /// Check if screen sharing.
    pub async fn is_screen_sharing(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::IsScreenSharing { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Ratchet the encryption key for forward secrecy.
    ///
    /// This advances the key to a new epoch, providing forward secrecy
    /// so that compromise of a later key doesn't reveal earlier messages.
    /// Returns the new key epoch, or None if not connected.
    pub async fn ratchet_key(&self) -> Option<u32> {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::RatchetKey { response: tx }).is_err() {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Set encryption key externally (e.g., from CallKeyState ratchet).
    ///
    /// Use this when managing keys via the Signal-based CallKeyState.
    pub async fn set_encryption_key(&self, key: Vec<u8>) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::SetEncryptionKey { key, response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Get the current key epoch.
    pub async fn key_epoch(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CallCommand::GetKeyEpoch { response: tx }).is_err() {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Attempt to reconnect to the current call.
    ///
    /// This requests a new token from the server and reconnects to LiveKit.
    /// Useful when the connection drops unexpectedly.
    pub async fn reconnect(&self) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::Reconnect { response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Shutdown the worker.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(CallCommand::Shutdown);
    }
}

/// Run the call worker on the current thread.
///
/// This creates a single-threaded tokio runtime with LocalSet for non-Send futures.
fn run_call_worker(
    mut cmd_rx: mpsc::UnboundedReceiver<CallCommand>,
    events_tx: mpsc::UnboundedSender<LiveKitRoomEvent>,
    call_client: CallClient,
) {
    // Create single-threaded runtime for non-Send types
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create call worker runtime");

    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, async move {
        tracing::info!("Call worker started");

        // Create the actual call service (LiveKit types stay on this thread)
        let (room_manager, mut room_events_rx) = RoomManager::new();
        let room_manager = Arc::new(room_manager);
        let call_client = Arc::new(parking_lot::RwLock::new(call_client));
        let active_call: Arc<parking_lot::RwLock<Option<ActiveCallState>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Key ratchet timer control
        let ratchet_cancel: Arc<parking_lot::RwLock<Option<tokio::sync::oneshot::Sender<()>>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Forward room events to the handle
        let events_tx_clone = events_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(event) = room_events_rx.recv().await {
                if events_tx_clone.send(event).is_err() {
                    break;
                }
            }
        });

        // Process commands
        loop {
            let cmd = match cmd_rx.recv().await {
                Some(cmd) => cmd,
                None => break, // Channel closed
            };

            match cmd {
                CallCommand::InitiateCall {
                    channel_id,
                    title,
                    call_type,
                    encryption_root_key,
                    initiator_id,
                    response,
                } => {
                    let result = initiate_call_impl(
                        &call_client,
                        &room_manager,
                        &active_call,
                        channel_id,
                        &title,
                        call_type,
                        &encryption_root_key,
                        initiator_id,
                    )
                    .await;

                    // Start key ratchet timer on successful call initiation
                    if result.is_ok() {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel);
                    }

                    let _ = response.send(result);
                }

                CallCommand::JoinCall {
                    call_id,
                    encryption_root_key,
                    joiner_id,
                    response,
                } => {
                    let result = join_call_impl(
                        &call_client,
                        &room_manager,
                        &active_call,
                        call_id,
                        &encryption_root_key,
                        joiner_id,
                    )
                    .await;

                    // Start key ratchet timer on successful call join
                    if result.is_ok() {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel);
                    }

                    let _ = response.send(result);
                }

                CallCommand::LeaveCall { response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let result = leave_call_impl(&call_client, &room_manager, &active_call).await;
                    let _ = response.send(result);
                }

                CallCommand::EndCall { reason, response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let result =
                        end_call_impl(&call_client, &room_manager, &active_call, reason.as_deref())
                            .await;
                    let _ = response.send(result);
                }

                CallCommand::DisconnectLocal { response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    room_manager.disconnect().await;
                    *active_call.write() = None;
                    tracing::info!("Disconnected locally from call");
                    let _ = response.send(());
                }

                CallCommand::GetActiveCall { response } => {
                    let call = active_call.read().as_ref().map(|a| a.call.clone());
                    let _ = response.send(call);
                }

                CallCommand::IsInCall { response } => {
                    let _ = response.send(active_call.read().is_some());
                }

                CallCommand::GetRoomState { response } => {
                    let _ = response.send(room_manager.state());
                }

                CallCommand::PublishMicrophone { device_id, response } => {
                    let result = room_manager.publish_microphone(device_id.as_deref()).await;
                    let _ = response.send(result);
                }

                CallCommand::PublishCamera {
                    device_path,
                    config,
                    response,
                } => {
                    let result = room_manager
                        .publish_camera(device_path.as_deref(), config)
                        .await;
                    let _ = response.send(result);
                }

                CallCommand::StopMicrophone => {
                    room_manager.stop_microphone();
                }

                CallCommand::StopCamera => {
                    room_manager.stop_camera();
                }

                CallCommand::SetMicrophoneMuted { muted } => {
                    room_manager.set_microphone_muted(muted);
                }

                CallCommand::IsMicrophoneMuted { response } => {
                    let _ = response.send(room_manager.is_microphone_muted());
                }

                CallCommand::SetCameraEnabled { enabled } => {
                    room_manager.set_camera_enabled(enabled);
                }

                CallCommand::IsCameraEnabled { response } => {
                    let _ = response.send(room_manager.is_camera_enabled());
                }

                CallCommand::PublishScreenShare { source, response } => {
                    let result = room_manager.publish_screen_share(source, None).await;
                    let _ = response.send(result);
                }

                CallCommand::StopScreenShare => {
                    room_manager.stop_screen_share();
                }

                CallCommand::IsScreenSharing { response } => {
                    let _ = response.send(room_manager.is_screen_sharing());
                }

                CallCommand::RatchetKey { response } => {
                    let epoch = room_manager.ratchet_key();
                    let _ = response.send(epoch);
                }

                CallCommand::SetEncryptionKey { key, response } => {
                    let success = room_manager.set_encryption_key(key);
                    let _ = response.send(success);
                }

                CallCommand::GetKeyEpoch { response } => {
                    let epoch = room_manager.key_epoch();
                    let _ = response.send(epoch);
                }

                CallCommand::Reconnect { response } => {
                    let result = reconnect_impl(
                        &call_client,
                        &room_manager,
                        &active_call,
                    )
                    .await;
                    let _ = response.send(result);
                }

                CallCommand::Shutdown => {
                    tracing::info!("Call worker shutting down");
                    break;
                }
            }
        }

        tracing::info!("Call worker stopped");
    });
}

/// Internal state for active call.
struct ActiveCallState {
    call: Call,
    /// Key state for Signal-based key derivation and ratcheting.
    key_state: CallKeyState,
    /// Room name for potential future UI display during reconnection.
    #[allow(dead_code)]
    room_name: String,
    /// LiveKit server URL for reconnection.
    livekit_url: String,
}

async fn initiate_call_impl(
    call_client: &Arc<parking_lot::RwLock<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
    channel_id: ChannelId,
    title: &str,
    call_type: CallType,
    encryption_root_key: &[u8; 32],
    initiator_id: UserId,
) -> Result<Call, CallError> {
    // Check if already in a call
    if active_call.read().is_some() {
        return Err(CallError::AlreadyInCall);
    }

    // Convert call type to proto
    let proto_type = match call_type {
        CallType::Voice => ProtoCallType::Voice,
        CallType::Video => ProtoCallType::Video,
        CallType::Group => ProtoCallType::Video,
    };

    // Create call on server
    let response = {
        let mut client = call_client.write();
        client
            .create_call(&channel_id.to_string(), title, proto_type, None)
            .await
            .map_err(|e| CallError::ServerError(e.to_string()))?
    };

    // Derive E2EE key
    let key_state = CallKeyState::new(encryption_root_key, &response.call_id)
        .map_err(|e| CallError::CryptoError(e.to_string()))?;

    // Connect to LiveKit room
    room_manager
        .connect(
            &response.livekit_url,
            &response.livekit_token,
            key_state.current_key_vec(),
        )
        .await?;

    // Create domain call object
    let call_id =
        CallId::from_str(&response.call_id).map_err(|_| CallError::InvalidCallId)?;

    let mut call = Call::new(channel_id, initiator_id, call_type);
    call.id = call_id;
    call.start();
    call.add_participant(Participant::new(initiator_id));

    // Store active call
    *active_call.write() = Some(ActiveCallState {
        call: call.clone(),
        key_state,
        room_name: response.room_name,
        livekit_url: response.livekit_url,
    });

    Ok(call)
}

async fn join_call_impl(
    call_client: &Arc<parking_lot::RwLock<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
    call_id: CallId,
    encryption_root_key: &[u8; 32],
    joiner_id: UserId,
) -> Result<Call, CallError> {
    // Check if already in a call
    if active_call.read().is_some() {
        return Err(CallError::AlreadyInCall);
    }

    // Join call on server
    let response = {
        let mut client = call_client.write();
        client
            .join_call(&call_id.to_string())
            .await
            .map_err(|e| CallError::ServerError(e.to_string()))?
    };

    // Derive E2EE key
    let key_state = CallKeyState::new(encryption_root_key, &call_id.to_string())
        .map_err(|e| CallError::CryptoError(e.to_string()))?;

    // Connect to LiveKit room
    room_manager
        .connect(
            &response.livekit_url,
            &response.livekit_token,
            key_state.current_key_vec(),
        )
        .await?;

    // Convert proto to domain
    let call_info = response.call_info.ok_or(CallError::NotFound)?;
    let channel_id = ChannelId::from_str(&call_info.channel_id)
        .map_err(|_| CallError::InvalidChannelId)?;
    let initiator_id =
        UserId::from_str(&call_info.created_by).map_err(|_| CallError::InvalidUserId)?;

    let call_type = match ProtoCallType::try_from(call_info.r#type).unwrap_or(ProtoCallType::Unspecified)
    {
        ProtoCallType::Voice => CallType::Voice,
        ProtoCallType::Video | ProtoCallType::ScreenShare => CallType::Video,
        _ => CallType::Voice,
    };

    let mut call = Call::new(channel_id, initiator_id, call_type);
    call.id = call_id;
    call.start();
    call.add_participant(Participant::new(joiner_id));

    // Store active call
    *active_call.write() = Some(ActiveCallState {
        call: call.clone(),
        key_state,
        room_name: call_info.room_name,
        livekit_url: response.livekit_url,
    });

    Ok(call)
}

async fn leave_call_impl(
    call_client: &Arc<parking_lot::RwLock<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    let call_id = {
        let active = active_call.read();
        active
            .as_ref()
            .map(|a| a.call.id.to_string())
            .ok_or(CallError::NotInCall)?
    };

    // Leave on server with timeout
    {
        let mut client = call_client.write();
        match tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            client.leave_call(&call_id),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::debug!("Server acknowledged call leave");
            }
            Ok(Err(e)) => {
                tracing::warn!("Server leave_call error (continuing): {}", e);
            }
            Err(_) => {
                tracing::warn!("Server leave_call timed out (continuing with local disconnect)");
            }
        }
    }

    // Disconnect from LiveKit
    room_manager.disconnect().await;

    // Clear active call
    *active_call.write() = None;

    Ok(())
}

async fn end_call_impl(
    call_client: &Arc<parking_lot::RwLock<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
    reason: Option<&str>,
) -> Result<(), CallError> {
    let call_id = {
        let active = active_call.read();
        active
            .as_ref()
            .map(|a| a.call.id.to_string())
            .ok_or(CallError::NotInCall)?
    };

    // End on server
    {
        let mut client = call_client.write();
        client
            .end_call(&call_id, reason.unwrap_or(""))
            .await
            .map_err(|e| CallError::ServerError(e.to_string()))?;
    }

    // Disconnect from LiveKit
    room_manager.disconnect().await;

    // Clear active call
    *active_call.write() = None;

    Ok(())
}

/// Reconnect to the current call after a connection drop.
///
/// This gets a new token from the server and reconnects to LiveKit.
async fn reconnect_impl(
    call_client: &Arc<parking_lot::RwLock<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    // Get current call info
    let (call_id, key_vec, livekit_url) = {
        let active = active_call.read();
        let state = active.as_ref().ok_or(CallError::NotInCall)?;
        (
            state.call.id.to_string(),
            state.key_state.current_key_vec(),
            state.livekit_url.clone(),
        )
    };

    tracing::info!(call_id = %call_id, "Attempting reconnection");

    // Disconnect current connection first
    room_manager.disconnect().await;

    // Get a new token by rejoining the call
    let response = {
        let mut client = call_client.write();
        client
            .join_call(&call_id)
            .await
            .map_err(|e| CallError::ServerError(format!("Reconnect failed: {}", e)))?
    };

    // Reconnect to LiveKit with new token but same key
    room_manager
        .connect(&livekit_url, &response.livekit_token, key_vec)
        .await?;

    tracing::info!(call_id = %call_id, "Reconnected successfully");
    Ok(())
}

/// Key ratchet interval in seconds.
/// Ratcheting every 60 seconds provides good forward secrecy without excessive overhead.
const KEY_RATCHET_INTERVAL_SECS: u64 = 60;

/// Start the periodic key ratchet timer.
fn start_key_ratchet_timer(
    room_manager: &Arc<RoomManager>,
    ratchet_cancel: &Arc<parking_lot::RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
) {
    // Cancel any existing timer
    stop_key_ratchet_timer(ratchet_cancel);

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    *ratchet_cancel.write() = Some(cancel_tx);

    let room_manager = room_manager.clone();

    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(KEY_RATCHET_INTERVAL_SECS)
        );
        // Skip the first tick (immediate)
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Some(epoch) = room_manager.ratchet_key() {
                        tracing::debug!(epoch = epoch, "Periodic key ratchet completed");
                    } else {
                        // Not connected, stop the timer
                        tracing::debug!("Key ratchet timer stopping: not connected");
                        break;
                    }
                }
                _ = &mut cancel_rx => {
                    tracing::debug!("Key ratchet timer cancelled");
                    break;
                }
            }
        }
    });

    tracing::info!(
        interval_secs = KEY_RATCHET_INTERVAL_SECS,
        "Key ratchet timer started"
    );
}

/// Stop the periodic key ratchet timer.
fn stop_key_ratchet_timer(
    ratchet_cancel: &Arc<parking_lot::RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
) {
    if let Some(cancel_tx) = ratchet_cancel.write().take() {
        let _ = cancel_tx.send(());
    }
}
