//! Call worker - dedicated thread for non-Send LiveKit operations.
//!
//! LiveKit types don't implement Send/Sync, so all call operations must run
//! on a single dedicated thread. This module provides a Send-safe handle that
//! communicates with the worker via channels.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as TokioMutex};

use crate::calls::{LiveKitRoomEvent, RoomManager, RoomState, SharedVideoFrames, VideoCaptureConfig};
#[cfg(not(target_os = "android"))]
use crate::calls::ScreenSource;
use crate::crypto::call_key_provider::CallKeyState;
use crate::domain::call::{Call, CallState, CallType, Participant};
use crate::domain::ids::{CallId, ChannelId, UserId};
use crate::error::CallError;
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::network::grpc::CallClient;
use crate::proto::chat_v1::CallType as ProtoCallType;

// ============================================================================
// Orchestrator types
// ============================================================================
//
// The call worker doubles as the call orchestrator: it already owns the
// active-call slot and the LiveKit room, so concurrent-call policy (show
// modal? auto-decline? replace active?) belongs here rather than scattered
// across UI callbacks.
//
// Phase 1 (this module): types, commands, state, broadcast channel.
// Phase 2 (follow-up): EventBus bridge + UI migration.

/// Stable identifier used to match presented-incoming / pending-outgoing
/// calls across handler boundaries. For Signal-style DM calls this is the
/// server-assigned call id; for anonymous calls it is the LiveKit room name.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct CallHandle(String);

impl CallHandle {
    pub fn new(id: impl Into<String>) -> Self {
        CallHandle(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for CallHandle {
    fn from(s: String) -> Self {
        CallHandle(s)
    }
}

impl From<&str> for CallHandle {
    fn from(s: &str) -> Self {
        CallHandle(s.to_string())
    }
}

/// Which signalling transport the orchestrator is tracking for a given call.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OrchestratedCallKind {
    /// Signal-style DM call with ephemeral key exchange.
    DmStandard,
    /// Anonymous DM call (blind-signature token auth).
    AnonDm,
    /// Anonymous group call (blind-signature token auth).
    AnonGroup,
    /// Regular group channel call (sender-key distributed key).
    GroupChannel,
}

/// Reason an active call ended.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CallEndReason {
    /// Local user hung up deliberately.
    LocalHangup,
    /// Remote peer hung up.
    RemoteHangup,
    /// Local user replaced this call by accepting a newer incoming call.
    ReplacedByIncoming,
    /// Call was declined before it connected.
    Declined,
    /// Signalling / crypto / network failure.
    Error(String),
}

/// Events emitted by the orchestrator. Consumers (UI, notification service,
/// call history) subscribe via [`CallWorkerHandle::take_call_events`].
#[derive(Debug, Clone)]
pub enum CallOrchestratorEvent {
    /// An incoming call should be presented to the user. When
    /// `replaces_active` is true there is already an ongoing call — the UI
    /// should surface the accept action as "Accept & end current call".
    IncomingPresented {
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
        is_identity_verified: bool,
        identity_warning: String,
        /// For DM-standard calls: caller's ephemeral X25519 pk (32 bytes) to
        /// be passed back on accept. `None` for transports that derive the
        /// key server-side (anonymous calls).
        ephemeral_public_key: Option<[u8; 32]>,
        /// Caller-reported channel/room context (DM channel id, anon room
        /// name, group id) — opaque to the orchestrator, used by the UI and
        /// the signalling services for accept/decline.
        channel_context: Option<String>,
        replaces_active: bool,
    },

    /// A presented incoming call was withdrawn before the user acted (caller
    /// hung up, timed out, declined remotely).
    IncomingWithdrawn {
        call: CallHandle,
        reason: CallEndReason,
    },

    /// An outgoing call has been registered with the orchestrator.
    OutgoingStarted {
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
    },

    /// A call (incoming or outgoing) transitioned to the connected state.
    ActiveConnected {
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        is_video: bool,
    },

    /// An anonymous call (DM or group) fully connected to LiveKit. Carries
    /// everything the UI needs to mount the active-call surface without
    /// having to round-trip through additional service calls.
    AnonCallConnected {
        call: CallHandle,
        kind: OrchestratedCallKind,
        info: AnonCallConnectedInfo,
    },

    /// The active call has ended.
    ActiveEnded {
        call: CallHandle,
        reason: CallEndReason,
    },
}

/// UI-facing snapshot emitted with [`CallOrchestratorEvent::AnonCallConnected`].
///
/// The E2EE key is intentionally absent — it was consumed by the worker when
/// it connected to LiveKit and should not be broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct AnonCallConnectedInfo {
    pub room_name: String,
    pub channel_id: ChannelId,
    pub verification_emojis: [String; 4],
    pub is_video: bool,
    pub peer_display_name: String,
    /// Shared call-start anchor (ns since epoch) for the duration timer.
    /// Sourced from the callee's answer envelope so both peers count from
    /// the same reference point.
    pub started_at_ns: u64,
    /// Cached peer avatar bytes (from the local AnonDm row) for the active
    /// call tile. None when the peer never sent a profile avatar.
    pub peer_avatar: Option<Vec<u8>>,
}

/// Bookkeeping for a presented incoming call waiting for the user to decide.
#[derive(Debug, Clone)]
struct PendingIncoming {
    call: CallHandle,
    kind: OrchestratedCallKind,
    peer_id: UserId,
    peer_name: String,
    is_video: bool,
    is_identity_verified: bool,
    identity_warning: String,
    ephemeral_public_key: Option<[u8; 32]>,
    channel_context: Option<String>,
}

/// Bookkeeping for an outgoing call dialed but not yet connected.
#[derive(Debug, Clone)]
struct PendingOutgoing {
    call: CallHandle,
    kind: OrchestratedCallKind,
    peer_id: UserId,
    peer_name: String,
    is_video: bool,
}

/// Active-call tracking for the orchestrator. Distinct from
/// [`ActiveCallState`] which tracks LiveKit-level call state — this tracks
/// orchestrator-level identity so remote hangups and replacements can find
/// the right slot to clear.
#[derive(Debug, Clone)]
#[allow(dead_code)] // peer_id/kind/is_video consumed by phase-2 UI migration
struct OrchestratedActive {
    call: CallHandle,
    kind: OrchestratedCallKind,
    peer_id: UserId,
    is_video: bool,
}

/// Mutable orchestration state owned by the worker task.
#[derive(Default)]
struct OrchestratorState {
    active: Option<OrchestratedActive>,
    incoming: HashMap<CallHandle, PendingIncoming>,
    outgoing: HashMap<CallHandle, PendingOutgoing>,
}

/// Snapshot of a pending-incoming record returned by the orchestrator to
/// callers. Public counterpart of [`PendingIncoming`] — exposes the same
/// fields but by value so the receiving side does not need to hold any
/// references into worker-internal state.
#[derive(Debug, Clone)]
pub struct PendingIncomingSnapshot {
    pub call: CallHandle,
    pub kind: OrchestratedCallKind,
    pub peer_id: UserId,
    pub peer_name: String,
    pub is_video: bool,
    pub is_identity_verified: bool,
    pub identity_warning: String,
    pub ephemeral_public_key: Option<[u8; 32]>,
    pub channel_context: Option<String>,
}

impl From<PendingIncoming> for PendingIncomingSnapshot {
    fn from(p: PendingIncoming) -> Self {
        Self {
            call: p.call,
            kind: p.kind,
            peer_id: p.peer_id,
            peer_name: p.peer_name,
            is_video: p.is_video,
            is_identity_verified: p.is_identity_verified,
            identity_warning: p.identity_warning,
            ephemeral_public_key: p.ephemeral_public_key,
            channel_context: p.channel_context,
        }
    }
}

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
    /// Get the room name of the active call.
    GetRoomName {
        response: oneshot::Sender<Option<String>>,
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
    /// Locally mute/unmute a specific remote participant's audio.
    SetPeerAudioMuted { participant_id: String, muted: bool },
    /// Locally pause/resume a specific remote participant's video.
    SetPeerVideoPaused { participant_id: String, paused: bool },
    /// Publish screen share (desktop only).
    #[cfg(not(target_os = "android"))]
    PublishScreenShare {
        source: ScreenSource,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Stop screen share (desktop only).
    #[cfg(not(target_os = "android"))]
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
    /// Rotate the encryption key (for member-leave key rotation).
    /// Replaces the current key entirely with a new random key.
    RotateKey {
        new_key: [u8; 32],
        response: oneshot::Sender<Result<u32, CallError>>,
    },
    /// Connect directly to LiveKit with a pre-made access token (for anonymous calls).
    ConnectWithToken {
        access_token: String,
        livekit_url: String,
        encryption_key: Vec<u8>,
        room_name: String,
        call_id: CallId,
        channel_id: ChannelId,
        /// Whether this is a video call — drives auto-publish of the camera
        /// track after the connect handshake succeeds.
        is_video: bool,
        response: oneshot::Sender<Result<(), CallError>>,
    },

    // ========================================================================
    // Orchestrator commands
    // ========================================================================
    /// Record a newly received incoming call offer. The orchestrator decides
    /// whether to present it fresh or flag it as replacing the active call.
    OrchPresentIncoming {
        incoming: PendingIncoming,
    },
    /// Withdraw a previously presented incoming call (caller hung up / timed
    /// out / remote-declined before we acted).
    OrchWithdrawIncoming {
        call: CallHandle,
        reason: CallEndReason,
    },
    /// Register an outgoing call that was just dialed.
    OrchRegisterOutgoing {
        outgoing: PendingOutgoing,
    },
    /// Notify the orchestrator that a pending (incoming or outgoing) call
    /// has connected and should be installed as the active call.
    OrchMarkConnected {
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        is_video: bool,
    },
    /// Notify the orchestrator that the active call has been torn down
    /// locally or remotely. Clears the slot and emits `ActiveEnded`.
    OrchMarkEnded {
        call: CallHandle,
        reason: CallEndReason,
    },
    /// UI asks the orchestrator to accept a presented incoming call.
    /// Fails if the active slot is occupied — UI must use
    /// [`OrchAcceptIncomingReplacingActive`](Self::OrchAcceptIncomingReplacingActive)
    /// in that case.
    OrchAcceptIncoming {
        call: CallHandle,
        response: oneshot::Sender<Result<PendingIncoming, CallError>>,
    },
    /// UI asks the orchestrator to end the active call and then hand back
    /// the pending incoming so the UI can drive the accept flow.
    OrchAcceptIncomingReplacingActive {
        call: CallHandle,
        response: oneshot::Sender<Result<PendingIncoming, CallError>>,
    },
    /// UI asks the orchestrator to decline a presented incoming call. The
    /// orchestrator clears pending state and emits `IncomingWithdrawn`; the
    /// UI is still responsible for sending the wire-level decline because
    /// the signalling services live outside the worker for now.
    OrchDeclineIncoming {
        call: CallHandle,
        response: oneshot::Sender<Result<PendingIncoming, CallError>>,
    },
    /// UI queries whether there is currently an active call, for callers
    /// that need a synchronous-ish answer (e.g. UI routing).
    OrchIsActive {
        response: oneshot::Sender<bool>,
    },
    /// Return the kind of the active orchestrated call, or `None` if no call.
    OrchActiveKind {
        response: oneshot::Sender<Option<OrchestratedCallKind>>,
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
    cmd_tx: mpsc::Sender<CallCommand>,
    /// Room events forwarded from the worker (single-consumer — taken by UI).
    room_events_rx: Arc<RwLock<Option<mpsc::Receiver<LiveKitRoomEvent>>>>,
    /// Fan-out broadcast of room events for core listeners (e.g. key
    /// rotation on `ParticipantLeft`). UI reads the mpsc receiver above; core
    /// policy hooks subscribe here.
    room_events_broadcast: broadcast::Sender<LiveKitRoomEvent>,
    /// Shared video frames for UI access (updated by call worker, read by UI).
    shared_video_frames: SharedVideoFrames,
    /// Broadcast sender for orchestrator events. Kept on the handle so UI
    /// subscribers can call `subscribe_call_events` any number of times.
    call_events_tx: broadcast::Sender<CallOrchestratorEvent>,
    /// Snapshot of the most recently presented incoming call. The worker
    /// writes here whenever it fires `IncomingPresented`, and drains the
    /// slot on withdraw / accept / decline so callers always see current
    /// state without plumbing extra channels.
    latest_pending_incoming: Arc<parking_lot::RwLock<Option<PendingIncomingSnapshot>>>,
}

impl CallWorkerHandle {
    /// Spawn a new call worker on a dedicated thread.
    pub fn spawn(call_client: CallClient) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (call_events_tx, _) = broadcast::channel(64);
        let (room_events_broadcast, _) = broadcast::channel(128);
        let latest_pending_incoming: Arc<parking_lot::RwLock<Option<PendingIncomingSnapshot>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Create shared video frames storage (shared between worker and UI)
        let shared_video_frames = SharedVideoFrames::new();
        let shared_frames_for_worker = shared_video_frames.clone();
        let call_events_for_worker = call_events_tx.clone();
        let room_events_broadcast_for_worker = room_events_broadcast.clone();
        let latest_pending_for_worker = latest_pending_incoming.clone();

        // Spawn dedicated thread for call operations
        std::thread::Builder::new()
            .name("call-worker".into())
            .spawn(move || {
                run_call_worker(
                    cmd_rx,
                    events_tx,
                    call_client,
                    shared_frames_for_worker,
                    call_events_for_worker,
                    room_events_broadcast_for_worker,
                    latest_pending_for_worker,
                );
            })
            .expect("Failed to spawn call worker thread");

        Self {
            cmd_tx,
            room_events_rx: Arc::new(RwLock::new(Some(events_rx))),
            room_events_broadcast,
            shared_video_frames,
            call_events_tx,
            latest_pending_incoming,
        }
    }

    /// Read the most recently presented pending incoming call without
    /// consuming it. Returns `None` if there is no outstanding presented
    /// incoming.
    pub fn peek_latest_pending_incoming(&self) -> Option<PendingIncomingSnapshot> {
        self.latest_pending_incoming.read().clone()
    }

    /// Take (consume) the most recently presented pending incoming call.
    pub fn take_latest_pending_incoming(&self) -> Option<PendingIncomingSnapshot> {
        self.latest_pending_incoming.write().take()
    }

    // ========================================================================
    // Orchestrator API
    // ========================================================================

    /// Subscribe to orchestrator events (incoming-call presentation, active
    /// call transitions, withdrawals). Each subscriber gets its own receiver.
    pub fn subscribe_call_events(&self) -> broadcast::Receiver<CallOrchestratorEvent> {
        self.call_events_tx.subscribe()
    }

    /// Subscribe to LiveKit room events as a broadcast (multi-consumer).
    /// Used by core-side policy hooks that need to react to room state (e.g.
    /// key rotation on `ParticipantLeft`) alongside the UI.
    pub fn subscribe_room_events(&self) -> broadcast::Receiver<LiveKitRoomEvent> {
        self.room_events_broadcast.subscribe()
    }

    /// Register a newly received incoming call offer with the orchestrator.
    /// The orchestrator decides whether to present it fresh or flag it as
    /// replacing the active call, and emits `IncomingPresented`.
    #[allow(clippy::too_many_arguments)]
    pub fn orch_present_incoming(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
        is_identity_verified: bool,
        identity_warning: String,
        ephemeral_public_key: Option<[u8; 32]>,
        channel_context: Option<String>,
    ) {
        let incoming = PendingIncoming {
            call,
            kind,
            peer_id,
            peer_name,
            is_video,
            is_identity_verified,
            identity_warning,
            ephemeral_public_key,
            channel_context,
        };
        let _ = self.cmd_tx.try_send(CallCommand::OrchPresentIncoming { incoming });
    }

    /// Withdraw a previously presented incoming call.
    pub fn orch_withdraw_incoming(&self, call: CallHandle, reason: CallEndReason) {
        let _ = self
            .cmd_tx
            .try_send(CallCommand::OrchWithdrawIncoming { call, reason });
    }

    /// Register an outgoing call that was just dialed.
    pub fn orch_register_outgoing(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
    ) {
        let outgoing = PendingOutgoing { call, kind, peer_id, peer_name, is_video };
        let _ = self.cmd_tx.try_send(CallCommand::OrchRegisterOutgoing { outgoing });
    }

    /// Notify the orchestrator that a pending call has connected.
    pub fn orch_mark_connected(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        is_video: bool,
    ) {
        let _ = self.cmd_tx.try_send(CallCommand::OrchMarkConnected {
            call,
            kind,
            peer_id,
            is_video,
        });
    }

    /// Notify the orchestrator that the active call ended.
    pub fn orch_mark_ended(&self, call: CallHandle, reason: CallEndReason) {
        let _ = self
            .cmd_tx
            .try_send(CallCommand::OrchMarkEnded { call, reason });
    }

    /// Accept a presented incoming call. Returns the pending incoming record
    /// so the caller can drive the wire-level accept flow, or
    /// `Err(CallError::AlreadyInCall)` if an active call is holding the slot
    /// (caller must use [`orch_accept_incoming_replacing_active`](Self::orch_accept_incoming_replacing_active)).
    pub async fn orch_accept_incoming(
        &self,
        call: CallHandle,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchAcceptIncoming { call, response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Accept a presented incoming call while ending the currently-active
    /// call. The orchestrator emits `ActiveEnded { ReplacedByIncoming }` for
    /// the old call and hands back the pending incoming so the caller can
    /// drive the wire-level accept.
    pub async fn orch_accept_incoming_replacing_active(
        &self,
        call: CallHandle,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchAcceptIncomingReplacingActive {
                call,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Decline a presented incoming call. Returns the pending record so the
    /// caller can send the wire-level decline.
    pub async fn orch_decline_incoming(
        &self,
        call: CallHandle,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchDeclineIncoming { call, response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Query whether there is an orchestrator-tracked active call.
    pub async fn orch_is_active(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::OrchIsActive { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Return the kind of the active orchestrated call, or `None`.
    pub async fn orch_active_kind(&self) -> Option<OrchestratedCallKind> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchActiveKind { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Get access to shared video frames for UI rendering.
    pub fn video_frames(&self) -> &SharedVideoFrames {
        &self.shared_video_frames
    }

    /// Take the room events receiver (can only be called once).
    pub fn take_room_events(&self) -> Option<mpsc::Receiver<LiveKitRoomEvent>> {
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
            .try_send(CallCommand::InitiateCall {
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
            .try_send(CallCommand::JoinCall {
                call_id,
                encryption_root_key: *encryption_root_key,
                joiner_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Connect directly to LiveKit with a pre-made access token (for anonymous calls).
    ///
    /// This bypasses the server call creation and directly connects to LiveKit.
    /// Used for anonymous calls where we get the token from the anonymous call service.
    pub async fn connect_with_token(
        &self,
        access_token: &str,
        livekit_url: &str,
        encryption_key: Vec<u8>,
        room_name: &str,
        call_id: CallId,
        channel_id: ChannelId,
        is_video: bool,
    ) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::ConnectWithToken {
                access_token: access_token.to_string(),
                livekit_url: livekit_url.to_string(),
                encryption_key,
                room_name: room_name.to_string(),
                call_id,
                channel_id,
                is_video,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Leave the current call.
    pub async fn leave_call(&self) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::LeaveCall { response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// End the current call (creator only).
    pub async fn end_call(&self, reason: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::EndCall {
                reason: reason.map(|s| s.to_string()),
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Disconnect locally without server notification.
    pub async fn disconnect_local(&self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.try_send(CallCommand::DisconnectLocal { response: tx });
        let _ = rx.await;
    }

    /// Fire-and-forget variant of [`disconnect_local`] for use in sync
    /// event handlers (e.g. `handle_domain_event`) that can't `.await`.
    /// The worker will disconnect LiveKit, clear `active_call`, clear the
    /// orchestrator slot, and emit `ActiveEnded` — same as the awaited
    /// version, just without backpressure.
    pub fn disconnect_local_nonblocking(&self) {
        let (tx, _rx) = oneshot::channel();
        let _ = self.cmd_tx.try_send(CallCommand::DisconnectLocal { response: tx });
    }

    /// Get the currently active call.
    pub async fn active_call(&self) -> Option<Call> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetActiveCall { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Get active call synchronously (best effort, may return stale data).
    /// For UI that needs immediate response without blocking.
    pub fn active_call_sync(&self) -> Option<Call> {
        // Fire-and-forget query - UI should use events for real-time updates
        None
    }

    /// Plain-data snapshot of participants for FFI consumers. Maps the
    /// domain `Participant` list onto a `Send + Clone` shape and tags
    /// which one is the local user.
    pub async fn participant_snapshot(
        &self,
    ) -> Vec<crate::client::calls::CallParticipantSnapshot> {
        let Some(call) = self.active_call().await else { return vec![]; };
        let local_id = call.initiator_id.clone();
        call.participants.iter().map(|p| {
            crate::client::calls::CallParticipantSnapshot {
                identity: p.user_id.to_string(),
                display_name: p.user_id.to_string(),
                has_audio: !p.is_muted,
                has_video: p.is_video_enabled,
                is_speaking: false,
                is_muted: p.is_muted,
                is_local: p.user_id == local_id,
            }
        }).collect()
    }

    /// Get the room name of the active call (for anonymous call management).
    pub async fn get_room_name(&self) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetRoomName { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Check if currently in a call.
    pub async fn is_in_call(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::IsInCall { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Get room connection state.
    pub async fn room_state(&self) -> RoomState {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::GetRoomState { response: tx }).is_err() {
            return RoomState::Disconnected;
        }
        rx.await.unwrap_or(RoomState::Disconnected)
    }

    /// Publish microphone audio.
    pub async fn publish_microphone(&self, device_id: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::PublishMicrophone {
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
            .try_send(CallCommand::PublishCamera {
                device_path: device_path.map(|s| s.to_string()),
                config: None,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Stop microphone capture.
    pub fn stop_microphone(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopMicrophone);
    }

    /// Stop camera capture.
    pub fn stop_camera(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopCamera);
    }

    /// Set microphone muted.
    pub fn set_microphone_muted(&self, muted: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetMicrophoneMuted { muted });
    }

    /// Check if microphone is muted.
    pub async fn is_microphone_muted(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::IsMicrophoneMuted { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Set camera enabled.
    pub fn set_camera_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetCameraEnabled { enabled });
    }

    /// Locally mute/unmute a specific remote peer's audio. Fire-and-forget;
    /// drops silently if the worker has shut down.
    pub fn set_peer_audio_muted(&self, participant_id: String, muted: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetPeerAudioMuted { participant_id, muted });
    }

    /// Locally pause/resume a specific remote peer's video.
    pub fn set_peer_video_paused(&self, participant_id: String, paused: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetPeerVideoPaused { participant_id, paused });
    }

    /// Check if camera is enabled.
    pub async fn is_camera_enabled(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::IsCameraEnabled { response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Publish screen share (desktop only).
    #[cfg(not(target_os = "android"))]
    pub async fn publish_screen_share(&self, source: ScreenSource) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::PublishScreenShare {
                source,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Stop screen share (desktop only).
    #[cfg(not(target_os = "android"))]
    pub fn stop_screen_share(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopScreenShare);
    }

    /// Check if screen sharing.
    pub async fn is_screen_sharing(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::IsScreenSharing { response: tx }).is_err() {
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
        if self.cmd_tx.try_send(CallCommand::RatchetKey { response: tx }).is_err() {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Rotate the encryption key after a member leaves.
    ///
    /// Replaces the current key with a new random key (not derived from the old one),
    /// so the departed member cannot decrypt future frames. Returns the new epoch.
    pub async fn rotate_key(&self, new_key: [u8; 32]) -> Result<u32, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::RotateKey { new_key, response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Set encryption key externally (e.g., from CallKeyState ratchet).
    ///
    /// Use this when managing keys via the Signal-based CallKeyState.
    pub async fn set_encryption_key(&self, key: Vec<u8>) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::SetEncryptionKey { key, response: tx }).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Get the current key epoch.
    pub async fn key_epoch(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.try_send(CallCommand::GetKeyEpoch { response: tx }).is_err() {
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
            .try_send(CallCommand::Reconnect { response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Shutdown the worker.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::Shutdown);
    }
}

/// Derive the orchestrator `CallHandle` from the active call state.
/// Anonymous calls key by room_name; standard calls by call_id.
fn derive_call_handle(state: &ActiveCallState) -> CallHandle {
    if state.room_name.starts_with("anon-") {
        CallHandle::from(state.room_name.clone())
    } else {
        CallHandle::from(state.call.id.to_string())
    }
}

/// Run the call worker on the current thread.
///
/// This creates a single-threaded tokio runtime with LocalSet for non-Send futures.
fn run_call_worker(
    mut cmd_rx: mpsc::Receiver<CallCommand>,
    events_tx: mpsc::Sender<LiveKitRoomEvent>,
    call_client: CallClient,
    shared_video_frames: SharedVideoFrames,
    call_events_tx: broadcast::Sender<CallOrchestratorEvent>,
    room_events_broadcast: broadcast::Sender<LiveKitRoomEvent>,
    latest_pending_incoming: Arc<parking_lot::RwLock<Option<PendingIncomingSnapshot>>>,
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
        // Pass shared_video_frames so UI can access video frames directly
        let (room_manager, mut room_events_rx) = RoomManager::with_shared_frames(Some(shared_video_frames));
        let room_manager = Arc::new(room_manager);
        let call_client = Arc::new(TokioMutex::new(call_client));
        let active_call: Arc<parking_lot::RwLock<Option<ActiveCallState>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Key ratchet timer control
        let ratchet_cancel: Arc<parking_lot::RwLock<Option<tokio::sync::oneshot::Sender<()>>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Orchestrator state (single source of truth for incoming/outgoing/active
        // call presentation policy).
        let mut orchestrator = OrchestratorState::default();

        // Forward room events to the handle (UI via mpsc) and fan out to any
        // core-side policy subscribers (e.g. key rotation) via broadcast.
        let events_tx_clone = events_tx.clone();
        let room_bus = room_events_broadcast.clone();
        tokio::task::spawn_local(async move {
            tracing::info!("Room events forwarder started");
            while let Some(event) = room_events_rx.recv().await {
                tracing::info!("Forwarding room event: {:?}", event);
                // Fire-and-forget broadcast — no subscribers is fine.
                let _ = room_bus.send(event.clone());
                if events_tx_clone.try_send(event).is_err() {
                    tracing::warn!("Failed to forward room event - receiver dropped");
                    break;
                }
            }
            tracing::info!("Room events forwarder stopped");
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

                    // Start key ratchet timer + auto-publish media on successful
                    // call initiation. Media publish was previously driven by UI
                    // glue — folding it into the worker keeps all call
                    // lifecycle side-effects in one place.
                    if result.is_ok() {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel, &active_call);
                        auto_publish_media(&room_manager, matches!(call_type, CallType::Video)).await;
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

                    if result.is_ok() {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel, &active_call);
                        let is_video = result
                            .as_ref()
                            .ok()
                            .map(|c| matches!(c.call_type, CallType::Video))
                            .unwrap_or(false);
                        auto_publish_media(&room_manager, is_video).await;
                    }

                    let _ = response.send(result);
                }

                CallCommand::ConnectWithToken {
                    access_token,
                    livekit_url,
                    encryption_key,
                    room_name,
                    call_id,
                    channel_id,
                    is_video,
                    response,
                } => {
                    let result = connect_with_token_impl(
                        &room_manager,
                        &active_call,
                        access_token,
                        livekit_url,
                        encryption_key,
                        room_name,
                        call_id,
                        channel_id,
                    )
                    .await;

                    if result.is_ok() {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel, &active_call);
                        auto_publish_media(&room_manager, is_video).await;
                    }

                    let _ = response.send(result);
                }

                CallCommand::LeaveCall { response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    // Capture handle before leave clears active_call
                    let handle = active_call.read().as_ref().map(derive_call_handle);

                    let result = leave_call_impl(&call_client, &room_manager, &active_call).await;

                    // Atomic orchestrator cleanup (previously required
                    // separate orch_mark_ended from callers)
                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason: CallEndReason::LocalHangup,
                        });
                    }

                    let _ = response.send(result);
                }

                CallCommand::EndCall { reason, response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let handle = active_call.read().as_ref().map(derive_call_handle);

                    let result =
                        end_call_impl(&call_client, &room_manager, &active_call, reason.as_deref())
                            .await;

                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason: CallEndReason::LocalHangup,
                        });
                    }

                    let _ = response.send(result);
                }

                CallCommand::DisconnectLocal { response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let handle = active_call.read().as_ref().map(derive_call_handle);

                    room_manager.disconnect().await;
                    *active_call.write() = None;
                    tracing::info!("Disconnected locally from call");

                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason: CallEndReason::LocalHangup,
                        });
                    }

                    let _ = response.send(());
                }

                CallCommand::GetActiveCall { response } => {
                    let call = active_call.read().as_ref().map(|a| a.call.clone());
                    let _ = response.send(call);
                }

                CallCommand::GetRoomName { response } => {
                    let name = active_call.read().as_ref().map(|a| a.room_name.clone());
                    let _ = response.send(name);
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

                CallCommand::SetPeerAudioMuted { participant_id, muted } => {
                    room_manager.set_peer_audio_muted(&participant_id, muted);
                }

                CallCommand::SetPeerVideoPaused { participant_id, paused } => {
                    room_manager.set_peer_video_paused(&participant_id, paused);
                }

                #[cfg(not(target_os = "android"))]
                CallCommand::PublishScreenShare { source, response } => {
                    let result = room_manager.publish_screen_share(source, None).await;
                    let _ = response.send(result);
                }

                #[cfg(not(target_os = "android"))]
                CallCommand::StopScreenShare => {
                    room_manager.stop_screen_share().await;
                }

                CallCommand::IsScreenSharing { response } => {
                    let _ = response.send(room_manager.is_screen_sharing());
                }

                CallCommand::RotateKey { new_key, response } => {
                    let result = {
                        let mut call_guard = active_call.write();
                        if let Some(ref mut state) = *call_guard {
                            state.key_state.reset(new_key);
                            let epoch = state.key_state.epoch();
                            tracing::info!(epoch, "Call key rotated after member departure");
                            Ok(epoch)
                        } else {
                            Err(CallError::NotInCall)
                        }
                    };

                    if result.is_ok() {
                        room_manager.set_encryption_key(new_key.to_vec());
                    }

                    let _ = response.send(result);
                }

                CallCommand::RatchetKey { response } => {
                    let epoch = room_manager.ratchet_key();
                    let _ = response.send(epoch);
                }

                CallCommand::SetEncryptionKey { key, response } => {
                    // DM caller path: the real ECDH-derived key has arrived
                    // (the callee connected with this same key). Re-root the
                    // ratchet chain on it and install it at LiveKit index 0 so
                    // both peers share one key_state + epoch baseline. Without
                    // re-rooting, the caller would keep ratcheting from the
                    // transient placeholder and desync from the callee at the
                    // first 30s ratchet.
                    let success = match <[u8; 32]>::try_from(key.as_slice()) {
                        Ok(arr) => {
                            let mut guard = active_call.write();
                            if let Some(state) = guard.as_mut() {
                                let call_id = state.key_state.call_id().to_string();
                                match CallKeyState::new(&arr, &call_id) {
                                    Ok(ks) => state.key_state = ks,
                                    Err(e) => tracing::error!("Re-root key_state failed: {}", e),
                                }
                            }
                            drop(guard);
                            room_manager.reset_shared_key(key)
                        }
                        Err(_) => {
                            tracing::error!("Encryption key is not 32 bytes; ignoring");
                            false
                        }
                    };
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

                // ============================================================
                // Orchestrator commands
                // ============================================================

                CallCommand::OrchPresentIncoming { incoming } => {
                    let replaces_active = orchestrator.active.is_some();
                    let snapshot = incoming.clone();
                    orchestrator.incoming.insert(incoming.call.clone(), incoming);
                    *latest_pending_incoming.write() =
                        Some(PendingIncomingSnapshot::from(snapshot.clone()));
                    let _ = call_events_tx.send(CallOrchestratorEvent::IncomingPresented {
                        call: snapshot.call,
                        kind: snapshot.kind,
                        peer_id: snapshot.peer_id,
                        peer_name: snapshot.peer_name,
                        is_video: snapshot.is_video,
                        is_identity_verified: snapshot.is_identity_verified,
                        identity_warning: snapshot.identity_warning,
                        ephemeral_public_key: snapshot.ephemeral_public_key,
                        channel_context: snapshot.channel_context,
                        replaces_active,
                    });
                }

                CallCommand::OrchWithdrawIncoming { call, reason } => {
                    if orchestrator.incoming.remove(&call).is_some() {
                        clear_latest_if_matches(&latest_pending_incoming, &call);
                        let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                            call,
                            reason,
                        });
                    } else {
                        tracing::debug!(
                            call = %call.as_str(),
                            "OrchWithdrawIncoming: no matching pending incoming"
                        );
                    }
                }

                CallCommand::OrchRegisterOutgoing { outgoing } => {
                    let snapshot = outgoing.clone();
                    orchestrator.outgoing.insert(outgoing.call.clone(), outgoing);
                    let _ = call_events_tx.send(CallOrchestratorEvent::OutgoingStarted {
                        call: snapshot.call,
                        kind: snapshot.kind,
                        peer_id: snapshot.peer_id,
                        peer_name: snapshot.peer_name,
                        is_video: snapshot.is_video,
                    });
                }

                CallCommand::OrchMarkConnected { call, kind, peer_id, is_video } => {
                    // Clear from pending maps regardless of whether it was
                    // incoming or outgoing — either way it's now connected.
                    orchestrator.outgoing.remove(&call);
                    orchestrator.incoming.remove(&call);
                    clear_latest_if_matches(&latest_pending_incoming, &call);
                    orchestrator.active = Some(OrchestratedActive {
                        call: call.clone(),
                        kind,
                        peer_id,
                        is_video,
                    });
                    let _ = call_events_tx.send(CallOrchestratorEvent::ActiveConnected {
                        call,
                        kind,
                        peer_id,
                        is_video,
                    });
                }

                CallCommand::OrchMarkEnded { call, reason } => {
                    let matched_active = orchestrator
                        .active
                        .as_ref()
                        .map(|a| a.call == call)
                        .unwrap_or(false);
                    if matched_active {
                        orchestrator.active = None;
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call,
                            reason,
                        });
                    } else if orchestrator.outgoing.remove(&call).is_some() {
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call,
                            reason,
                        });
                    } else if orchestrator.incoming.remove(&call).is_some() {
                        clear_latest_if_matches(&latest_pending_incoming, &call);
                        let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                            call,
                            reason,
                        });
                    } else {
                        tracing::debug!(
                            call = %call.as_str(),
                            "OrchMarkEnded: no matching call in orchestrator state"
                        );
                    }
                }

                CallCommand::OrchAcceptIncoming { call, response } => {
                    // Fail if an active call is holding the slot — UI must
                    // explicitly send the replacing variant so the user has
                    // chosen to end their current call.
                    let result = if orchestrator.active.is_some() {
                        Err(CallError::AlreadyInCall)
                    } else if let Some(p) = orchestrator.incoming.remove(&call) {
                        clear_latest_if_matches(&latest_pending_incoming, &call);
                        Ok(p)
                    } else {
                        Err(CallError::NotInCall)
                    };
                    let _ = response.send(result);
                }

                CallCommand::OrchAcceptIncomingReplacingActive { call, response } => {
                    let pending = orchestrator.incoming.remove(&call);
                    let result = match pending {
                        Some(p) => {
                            clear_latest_if_matches(&latest_pending_incoming, &call);
                            if let Some(prev) = orchestrator.active.take() {
                                let _ = call_events_tx.send(
                                    CallOrchestratorEvent::ActiveEnded {
                                        call: prev.call,
                                        reason: CallEndReason::ReplacedByIncoming,
                                    },
                                );
                            }
                            Ok(p)
                        }
                        None => Err(CallError::NotInCall),
                    };
                    let _ = response.send(result);
                }

                CallCommand::OrchDeclineIncoming { call, response } => {
                    let result = match orchestrator.incoming.remove(&call) {
                        Some(p) => {
                            clear_latest_if_matches(&latest_pending_incoming, &call);
                            let _ = call_events_tx.send(
                                CallOrchestratorEvent::IncomingWithdrawn {
                                    call: p.call.clone(),
                                    reason: CallEndReason::Declined,
                                },
                            );
                            Ok(p)
                        }
                        None => Err(CallError::NotInCall),
                    };
                    let _ = response.send(result);
                }

                CallCommand::OrchIsActive { response } => {
                    let _ = response.send(orchestrator.active.is_some());
                }

                CallCommand::OrchActiveKind { response } => {
                    let kind = orchestrator.active.as_ref().map(|a| a.kind);
                    let _ = response.send(kind);
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

#[allow(clippy::too_many_arguments)]
async fn initiate_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
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
        let mut client = call_client.lock().await;
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
    call_client: &Arc<TokioMutex<CallClient>>,
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
        let mut client = call_client.lock().await;
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

/// Connect directly to LiveKit with a pre-made access token (for anonymous calls).
/// 
/// This bypasses the server call creation and directly connects to LiveKit.
/// Used for anonymous calls where we get the token from the anonymous call service.
async fn connect_with_token_impl(
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
    access_token: String,
    livekit_url: String,
    encryption_key: Vec<u8>,
    room_name: String,
    call_id: CallId,
    channel_id: ChannelId,
) -> Result<(), CallError> {
    // Check if already in a call
    if active_call.read().is_some() {
        return Err(CallError::AlreadyInCall);
    }

    // Connect to LiveKit room with E2EE key
    room_manager
        .connect(&livekit_url, &access_token, encryption_key.clone())
        .await?;

    // Create a minimal Call object for UI purposes
    let call = Call {
        id: call_id,
        channel_id,
        initiator_id: UserId::new(), // Anonymous
        call_type: CallType::Voice, // Default to voice for anonymous calls
        state: CallState::Connected,
        participants: vec![],
        started_at: Some(chrono::Utc::now()),
        connected_at: Some(chrono::Utc::now()),
        ended_at: None,
        is_encrypted: true, // Anonymous calls are always encrypted (oblivious)
    };

    // Create key state from real encryption key
    let mut key_arr = [0u8; 32];
    if encryption_key.len() >= 32 { key_arr.copy_from_slice(&encryption_key[..32]); }
    // Use room_name as call_id for CallKeyState — both sides know it, ensuring
    // coordinated ratcheting produces identical keys on both sides
    let key_state = CallKeyState::new(&key_arr, &room_name)
        .map_err(|e| CallError::CryptoError(e.to_string()))?;

    // Store active call info
    *active_call.write() = Some(ActiveCallState {
        call: call.clone(),
        key_state,
        room_name: room_name.clone(),
        livekit_url: livekit_url.to_string(),
    });

    tracing::info!(
        call_id = %call_id,
        "Connected to LiveKit for anonymous call"
    );

    Ok(())
}

async fn leave_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    let (call_id, is_anonymous) = {
        let active = active_call.read();
        let state = active.as_ref().ok_or(CallError::NotInCall)?;
        (state.call.id.to_string(), state.room_name.starts_with("anon-"))
    };

    // Skip server leave_call for anonymous calls (no server-side call state)
    if !is_anonymous {
        let mut client = call_client.lock().await;
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
    call_client: &Arc<TokioMutex<CallClient>>,
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
        let mut client = call_client.lock().await;
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
    call_client: &Arc<TokioMutex<CallClient>>,
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
        let mut client = call_client.lock().await;
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
/// Ratcheting every 30 seconds provides good forward secrecy without excessive overhead.
const KEY_RATCHET_INTERVAL_SECS: u64 = 30;

/// Start the coordinated key ratchet timer.
///
/// Both sides ratchet at deterministic epoch boundaries aligned to the call start
/// timestamp, ensuring they produce identical keys without any signaling.
///
/// The ratchet uses `CallKeyState::ratchet()` (HKDF-SHA256 with call_id + epoch)
/// which is deterministic, then pushes the new key to LiveKit via `set_encryption_key()`.
///
/// # Forward secrecy
/// Each ratchet derives a new key from the current one. Compromising epoch N
/// doesn't reveal keys for epochs 0..N-1 (pre-compromise security).
///
/// # Post-compromise security
/// After a key compromise, the next ratchet produces a key the attacker can derive.
/// However, once the call ends, the keys are zeroized and can't be recovered.
fn start_key_ratchet_timer(
    room_manager: &Arc<RoomManager>,
    ratchet_cancel: &Arc<parking_lot::RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    active_call: &Arc<parking_lot::RwLock<Option<ActiveCallState>>>,
) {
    // Cancel any existing timer
    stop_key_ratchet_timer(ratchet_cancel);

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    *ratchet_cancel.write() = Some(cancel_tx);

    let room_manager = room_manager.clone();
    let active_call = active_call.clone();

    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(KEY_RATCHET_INTERVAL_SECS)
        );
        // Skip the first tick (immediate)
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Advance our deterministic CallKeyState and capture both
                    // this epoch's key and the next one (for the forward
                    // window). Both peers run the same chain at the same
                    // cadence, so they reach the same epoch == key index.
                    let ratcheted = {
                        let mut call_guard = active_call.write();
                        if let Some(ref mut state) = *call_guard {
                            match state.key_state.ratchet() {
                                Ok(()) => {
                                    let epoch = state.key_state.epoch();
                                    let key_cur = state.key_state.current_key_vec();
                                    let key_next = match state.key_state.peek_next_key() {
                                        Ok(k) => k.to_vec(),
                                        Err(e) => {
                                            tracing::error!("Key peek failed: {}", e);
                                            key_cur.clone()
                                        }
                                    };
                                    tracing::info!(epoch = epoch, "Coordinated key ratchet completed");
                                    Some((epoch, key_cur, key_next))
                                }
                                Err(e) => {
                                    tracing::error!("Key ratchet failed: {}", e);
                                    None
                                }
                            }
                        } else {
                            tracing::debug!("Key ratchet timer stopping: no active call");
                            break;
                        }
                    };

                    // Push the keys to LiveKit outside the lock.
                    if let Some((epoch, key_cur, key_next)) = ratcheted {
                        if !room_manager.set_ratcheted_shared_key(epoch, key_cur, key_next) {
                            tracing::warn!("Failed to push ratcheted key to LiveKit");
                        }
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

/// Auto-publish microphone (and camera, for video calls) on the worker
/// thread immediately after a successful connect.
///
/// Previously the UI spawned a `publish_media` task from every accept /
/// key-exchange path. Centralising it in the actor:
///   - removes business logic from the UI layer,
///   - guarantees all media publication runs on the worker thread (LiveKit
///     tracks are `!Send`), and
///   - ensures we publish exactly once per connect regardless of who
///     initiated (caller, callee, DM, anon).
///
/// Failures are logged but do not fail the connect — the call stays up
/// even if mic/camera permissions are denied.
async fn auto_publish_media(room_manager: &RoomManager, is_video: bool) {
    if let Err(e) = room_manager.publish_microphone(None).await {
        tracing::warn!(error = %e, "auto-publish microphone failed");
    }
    if is_video {
        // None lets the room manager pick a sensible default capture config.
        if let Err(e) = room_manager.publish_camera(None, None).await {
            tracing::warn!(error = %e, "auto-publish camera failed");
        }
    }
}

/// Clear the "latest pending incoming" slot if it currently points at
/// `call`. Caller-responsibility is to hold no other locks at call time —
/// this grabs a write guard on the shared cache.
fn clear_latest_if_matches(
    slot: &Arc<parking_lot::RwLock<Option<PendingIncomingSnapshot>>>,
    call: &CallHandle,
) {
    let mut guard = slot.write();
    let matches = guard.as_ref().map(|s| &s.call == call).unwrap_or(false);
    if matches {
        *guard = None;
    }
}

/// Stop the periodic key ratchet timer.
fn stop_key_ratchet_timer(
    ratchet_cancel: &Arc<parking_lot::RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
) {
    if let Some(cancel_tx) = ratchet_cancel.write().take() {
        let _ = cancel_tx.send(());
    }
}

// ============================================================================
// EventBus bridge
// ============================================================================
//
// Subscribes to DomainEvents on the main tokio runtime and forwards call
// offers / answers / hangups into the worker's orchestrator state. Replaces
// the per-event UI-side dispatch that used to live in
// `domain_handler/{calls,anon_calls}.rs` and which scattered modal-policy
// decisions across the UI layer.

/// Spawn the EventBus → CallWorker orchestrator bridge. Safe to call once at
/// app startup, after both the EventBus and the call worker exist.
///
/// `signaling` is required so the bridge can own answer-handling end-to-end:
/// token fetch, LiveKit connect and call-history side effects now live inside
/// the actor instead of being split across UI glue. UI subscribers only need
/// to listen for [`CallOrchestratorEvent::AnonCallConnected`] /
/// [`CallOrchestratorEvent::ActiveEnded`].
pub fn spawn_call_orchestrator_bridge(
    bus: &EventBus,
    worker: CallWorkerHandle,
    signaling: crate::services::call_signaling::CallSignalingService,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe_domain();
    tokio::spawn(async move {
        tracing::info!("Call orchestrator EventBus bridge started");
        loop {
            match rx.recv().await {
                Ok(event) => handle_domain_event(&worker, &signaling, event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        "Call orchestrator bridge lagged on DomainEvent channel"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Call orchestrator bridge stopping — domain bus closed");
                    break;
                }
            }
        }
    })
}

fn handle_domain_event(
    worker: &CallWorkerHandle,
    signaling: &crate::services::call_signaling::CallSignalingService,
    event: DomainEvent,
) {
    match event {
        DomainEvent::CallKeyOfferReceived {
            call_id,
            peer_id,
            ephemeral_public_key,
            call_type,
            channel_id,
            is_identity_verified,
            identity_warning,
            caller_name,
            ..
        } => {
            let is_video = call_type.eq_ignore_ascii_case("video")
                || call_type.eq_ignore_ascii_case("Video Call");
            worker.orch_present_incoming(
                CallHandle::from(call_id),
                OrchestratedCallKind::DmStandard,
                peer_id,
                caller_name,
                is_video,
                is_identity_verified,
                identity_warning,
                Some(ephemeral_public_key),
                Some(channel_id),
            );
        }

        DomainEvent::CallHangupReceived { .. } => {
            // Disconnect from LiveKit + clear active_call + orchestrator.
            // Previously only cleared orchestrator, leaving the room connected.
            worker.disconnect_local_nonblocking();
        }

        DomainEvent::AnonCallOfferReceived {
            sender_id,
            room_name,
            call_type,
            caller_display_name,
            conversation_id,
            ..
        } => {
            let is_video = call_type == 2;
            // Prefer the name WE locally know for this peer (synced via the
            // anon-DM profile exchange) over the caller's self-reported name in
            // the offer — reliable and spoof-resistant. Fall back to the offer's
            // name, then "Anonymous".
            let peer_name = signaling
                .anon_dm_peer_name(&conversation_id)
                .or_else(|| (!caller_display_name.is_empty()).then_some(caller_display_name))
                .unwrap_or_else(|| "Anonymous".to_string());
            worker.orch_present_incoming(
                CallHandle::from(room_name),
                OrchestratedCallKind::AnonDm,
                sender_id,
                peer_name,
                is_video,
                true, // anon-auth is treated as verified
                String::new(),
                None,
                Some(conversation_id.to_string()),
            );
        }

        DomainEvent::AnonCallAnswerReceived {
            accepted,
            call_room_id,
            callee_display_name,
            timestamp,
            ..
        } => {
            spawn_anon_call_answer_task(
                worker.clone(),
                signaling.clone(),
                OrchestratedCallKind::AnonDm,
                call_room_id,
                accepted,
                callee_display_name,
                timestamp,
            );
        }

        DomainEvent::AnonCallHangupReceived { .. } => {
            // Disconnect from LiveKit + clear active_call + orchestrator.
            // Previously only cleared orchestrator, leaving the room connected
            // and active_call set — causing "Already in a call" on retry.
            worker.disconnect_local_nonblocking();
        }

        DomainEvent::AnonGroupCallOfferReceived {
            sender_id,
            room_name,
            call_type,
            caller_display_name,
            group_id,
            ..
        } => {
            let is_video = call_type == 2;
            let peer_name = if caller_display_name.is_empty() {
                "Anonymous".to_string()
            } else {
                caller_display_name
            };
            worker.orch_present_incoming(
                CallHandle::from(room_name),
                OrchestratedCallKind::AnonGroup,
                sender_id,
                peer_name,
                is_video,
                true,
                String::new(),
                None,
                Some(group_id.to_string()),
            );
        }

        DomainEvent::AnonGroupCallAnswerReceived {
            accepted,
            call_room_id,
            callee_display_name,
            timestamp,
            ..
        } => {
            spawn_anon_call_answer_task(
                worker.clone(),
                signaling.clone(),
                OrchestratedCallKind::AnonGroup,
                call_room_id,
                accepted,
                callee_display_name,
                timestamp,
            );
        }

        DomainEvent::AnonGroupCallHangupReceived { .. } => {
            worker.disconnect_local_nonblocking();
        }

        DomainEvent::CallKeyAnswerReceived { .. }
        | DomainEvent::GroupCallKeyReceived { .. } => {
            // Key-answer handling is driven by the UI/glue layer against the
            // existing ChatterClient API. Nothing for the orchestrator to do
            // here in phase 1.
        }

        _ => {}
    }
}

/// Handle an anonymous call answer (DM or group) inside the orchestrator.
///
/// Calls `CallSignalingService::handle_anon_call_answer` on a background task
/// so the bridge's sync dispatch loop stays responsive, then translates the
/// outcome into orchestrator events:
///
/// - `Connected`   → emit `AnonCallConnected`; UI mounts the active-call surface.
/// - `Declined`    → emit `ActiveEnded(Declined)`.
/// - `AlreadyInCall` → no-op (another participant's fan-out answer in a group
///   call, or our own echo — the call is already active).
fn spawn_anon_call_answer_task(
    worker: CallWorkerHandle,
    signaling: crate::services::call_signaling::CallSignalingService,
    kind: OrchestratedCallKind,
    call_room_id: Vec<u8>,
    accepted: bool,
    callee_display_name: String,
    answer_timestamp_ns: u64,
) {
    tokio::spawn(async move {
        let room_handle = CallHandle::from(format!(
            "anon-{}",
            uuid::Uuid::from_bytes(
                <[u8; 16]>::try_from(call_room_id.as_slice()).unwrap_or([0u8; 16])
            )
        ));

        match signaling
            .handle_anon_call_answer(
                &call_room_id,
                accepted,
                &callee_display_name,
                answer_timestamp_ns,
                &worker,
            )
            .await
        {
            Ok(crate::domain::call::AnonCallAnswerOutcome::Connected(setup)) => {
                let info = AnonCallConnectedInfo {
                    room_name: setup.room_name.clone(),
                    channel_id: setup.channel_id,
                    verification_emojis: setup.verification_emojis.clone(),
                    is_video: setup.is_video,
                    peer_display_name: setup.peer_display_name.clone(),
                    started_at_ns: setup.started_at_ns,
                    peer_avatar: setup.peer_avatar.clone(),
                };
                let handle = CallHandle::from(setup.room_name.clone());
                // Register as the active call so subsequent hangup / new-call
                // offers find the slot. Anon peers have no stable UserId, so
                // we use nil — the orchestrator only uses peer_id for
                // replace-active policy, which anon calls don't participate in.
                worker.orch_mark_connected(
                    handle.clone(),
                    kind,
                    UserId(uuid::Uuid::nil()),
                    setup.is_video,
                );
                let _ = worker.call_events_tx.send(CallOrchestratorEvent::AnonCallConnected {
                    call: handle,
                    kind,
                    info,
                });
            }
            Ok(crate::domain::call::AnonCallAnswerOutcome::Declined) => {
                worker.orch_mark_ended(room_handle, CallEndReason::Declined);
            }
            Ok(crate::domain::call::AnonCallAnswerOutcome::AlreadyInCall) => {
                tracing::debug!(
                    call = %room_handle.as_str(),
                    "Ignoring duplicate anon call answer (already in call)"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "Anon call answer handling failed");
                worker.orch_mark_ended(
                    room_handle,
                    CallEndReason::Error(e.to_string()),
                );
            }
        }
    });
}
