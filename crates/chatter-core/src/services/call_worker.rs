//! Call worker - dedicated thread for non-Send LiveKit operations.
//!
//! LiveKit types don't implement Send/Sync, so all call operations must run
//! on a single dedicated thread. This module provides a Send-safe handle that
//! communicates with the worker via channels.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{Mutex as TokioMutex, broadcast, mpsc, oneshot};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::calls::ScreenSource;
use crate::calls::{
    LiveKitRoomEvent, RoomManager, RoomState, SharedVideoFrames, VideoCaptureConfig,
};
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

impl OrchestratedCallKind {
    /// Which conversation flavour this transport belongs to. Total by
    /// construction — every call-capable conversation type has exactly one
    /// signalling transport, so call history can label a row from `kind`
    /// alone and does not carry a second discriminant.
    pub fn conversation_type(self) -> crate::domain::message::ConversationType {
        use crate::domain::message::ConversationType as CT;
        match self {
            Self::DmStandard => CT::ServerDm,
            Self::AnonDm => CT::AnonDm,
            Self::AnonGroup => CT::AnonGroup,
            Self::GroupChannel => CT::Channel,
        }
    }
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
    /// Another of our own devices answered this incoming call.
    AnsweredElsewhere,
    /// Our outgoing call rang for [`RING_TIMEOUT`] without an answer.
    NoAnswer,
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
        /// The conversation this call arrived in, as `ConversationType`
        /// dictates for `kind` (peer `UserId` for a DM, `AnonDmId` /
        /// `AnonGroupId` for the anonymous flavours). Call history needs it
        /// to offer "call back" — `call` alone is a signaling id.
        conversation_id: String,
        replaces_active: bool,
    },

    /// An incoming offer arrived too late to ring (older than
    /// [`RING_TIMEOUT`] — delivered from the queue after the app was offline).
    /// Never presented; it only becomes a missed-call history row.
    IncomingMissed {
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_name: String,
        is_video: bool,
        conversation_id: String,
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
    /// Callee's answer envelope timestamp (Unix nanos), retained as metadata.
    /// Presentation duration uses the active Call's local connected_at.
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
    conversation_id: String,
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
struct OrchestratedActive {
    call: CallHandle,
    kind: OrchestratedCallKind,
}

/// Mutable orchestration state owned by the worker task.
#[derive(Default)]
struct OrchestratorState {
    active: Option<OrchestratedActive>,
    incoming: HashMap<CallHandle, PendingIncoming>,
    outgoing: HashMap<CallHandle, PendingOutgoing>,
}

impl OrchestratorState {
    /// The connected call's kind, else a dialed call's that is still ringing.
    fn call_kind(&self) -> Option<OrchestratedCallKind> {
        self.active
            .as_ref()
            .map(|a| a.kind)
            .or_else(|| self.outgoing.values().next().map(|o| o.kind))
    }
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
    pub conversation_id: String,
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
            conversation_id: p.conversation_id,
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
        /// `None`: a channel group call, joined holding no key. We connect
        /// under a random key of our own that is never sent, and publish
        /// nothing until the key authority's key is installed ([`PublishGate`]).
        encryption_root_key: Option<[u8; 32]>,
        joiner_id: UserId,
        response: oneshot::Sender<Result<Call, CallError>>,
    },
    /// Leave the current call.
    LeaveCall {
        /// What the emitted `ActiveEnded` reports.
        reason: CallEndReason,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// End the current call (creator only).
    EndCall {
        reason: Option<String>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Disconnect locally without server notification. `reason` is what the
    /// resulting `ActiveEnded` carries. With `call`, only if that is the
    /// active call — a hangup names its call, and one for another call must
    /// not end this one.
    DisconnectLocal {
        call: Option<CallHandle>,
        reason: CallEndReason,
        response: oneshot::Sender<()>,
    },
    /// Swap the worker's gRPC call client (anon-mode transport switch).
    ///
    /// The command loop is strictly sequential and fully awaits each call
    /// operation before dequeuing the next, so this is only ever processed
    /// BETWEEN complete operations — never mid-RPC — making the swap safe.
    SwapCallClient { client: CallClient },
    /// Get active call info.
    GetActiveCall {
        response: oneshot::Sender<Option<Call>>,
    },
    /// Get the room name of the active call.
    GetRoomName {
        response: oneshot::Sender<Option<String>>,
    },
    /// Check if in a call.
    IsInCall { response: oneshot::Sender<bool> },
    /// Get room state.
    GetRoomState {
        response: oneshot::Sender<RoomState>,
    },
    /// Get the identities (user ids) of the remote peers currently in the room.
    GetRemoteParticipants {
        response: oneshot::Sender<Vec<String>>,
    },
    /// Our join time and the remote peers' `(identity, join time)`.
    GetRoomSeniority {
        response: oneshot::Sender<Option<RoomSeniority>>,
    },
    /// Test-only: how many tracks we publish.
    #[cfg(feature = "testing")]
    GetLocalTrackCount { response: oneshot::Sender<usize> },
    /// The active call's group-key standing.
    GetGroupKeyStatus {
        response: oneshot::Sender<Option<GroupKeyStatus>>,
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
    IsMicrophoneMuted { response: oneshot::Sender<bool> },
    /// Get camera enabled state.
    IsCameraEnabled { response: oneshot::Sender<bool> },
    /// Locally mute/unmute a specific remote participant's audio.
    SetPeerAudioMuted { participant_id: String, muted: bool },
    /// Route remote audio to an output device (CPAL device name).
    SetAudioOutput {
        device: String,
        response: oneshot::Sender<Result<(), String>>,
    },
    /// Locally pause/resume a specific remote participant's video.
    SetPeerVideoPaused {
        participant_id: String,
        paused: bool,
    },
    /// Publish screen share (desktop only).
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    PublishScreenShare {
        source: ScreenSource,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Stop screen share (desktop only).
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    StopScreenShare,
    /// Check if screen sharing.
    IsScreenSharing { response: oneshot::Sender<bool> },
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
    GetKeyEpoch { response: oneshot::Sender<u32> },
    /// Test-only: the epoch and the whole shared-key ring LiveKit holds.
    #[cfg(feature = "testing")]
    GetInstalledKeyRing {
        response: oneshot::Sender<Option<(u32, Vec<Option<Vec<u8>>>)>>,
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
    /// Install a group-call key a participant sent us, exactly as the
    /// rotator installed it. `Ok(false)` when `epoch` is stale.
    InstallGroupKey {
        call_id: CallId,
        key: [u8; 32],
        epoch: u32,
        response: oneshot::Sender<Result<bool, CallError>>,
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
    OrchPresentIncoming { incoming: PendingIncoming },
    /// Withdraw a previously presented incoming call (caller hung up / timed
    /// out / remote-declined before we acted).
    OrchWithdrawIncoming {
        call: CallHandle,
        reason: CallEndReason,
    },
    /// Register an outgoing call that was just dialed.
    OrchRegisterOutgoing { outgoing: PendingOutgoing },
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
    /// Return the kind of the call we are in — connected, or dialed and
    /// still ringing — or `None` if no call.
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
}

impl CallWorkerHandle {
    /// Spawn a new call worker on a dedicated thread.
    pub fn spawn(call_client: CallClient) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (call_events_tx, _) = broadcast::channel(64);
        let (room_events_broadcast, _) = broadcast::channel(128);

        // Create shared video frames storage (shared between worker and UI)
        let shared_video_frames = SharedVideoFrames::new();
        let shared_frames_for_worker = shared_video_frames.clone();
        let call_events_for_worker = call_events_tx.clone();
        let room_events_broadcast_for_worker = room_events_broadcast.clone();

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
                );
            })
            .expect("Failed to spawn call worker thread");

        Self {
            cmd_tx,
            room_events_rx: Arc::new(RwLock::new(Some(events_rx))),
            room_events_broadcast,
            shared_video_frames,
            call_events_tx,
        }
    }

    /// Swap the worker's gRPC call client so subsequent signaling RPCs
    /// (create/join/leave/end) egress over the pool's current transport.
    /// Used on anon-mode toggle. The worker applies it between complete call
    /// operations, so no in-flight call is corrupted. Awaits enqueue so the
    /// swap is not silently dropped under buffer pressure; a closed channel
    /// (worker gone) is logged and ignored.
    pub async fn swap_call_client(&self, client: CallClient) {
        if self
            .cmd_tx
            .send(CallCommand::SwapCallClient { client })
            .await
            .is_err()
        {
            tracing::warn!("call worker: failed to enqueue call client swap (worker unavailable)");
        }
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
        conversation_id: String,
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
            conversation_id,
        };
        let _ = self
            .cmd_tx
            .try_send(CallCommand::OrchPresentIncoming { incoming });
    }

    /// Record an incoming call whose offer is too old to ring. Touches no
    /// orchestrator state, so it is emitted straight onto the event stream.
    pub fn orch_missed_incoming(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_name: String,
        is_video: bool,
        conversation_id: String,
    ) {
        tracing::info!(call = %call.as_str(), "Incoming call offer is past the ring timeout — recording as missed, not ringing");
        let _ = self
            .call_events_tx
            .send(CallOrchestratorEvent::IncomingMissed {
                call,
                kind,
                peer_name,
                is_video,
                conversation_id,
            });
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
        let outgoing = PendingOutgoing {
            call,
            kind,
            peer_id,
            peer_name,
            is_video,
        };
        let _ = self
            .cmd_tx
            .try_send(CallCommand::OrchRegisterOutgoing { outgoing });
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
            .try_send(CallCommand::OrchAcceptIncomingReplacingActive { call, response: tx })
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

    /// Kind of the call we are in — connected, or our outgoing call still
    /// ringing — or `None`. What `end_active_call` dispatches on, so a caller
    /// who cancels while it rings still takes the hangup path.
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
                encryption_root_key: Some(*encryption_root_key),
                joiner_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Join a channel group call holding no key: connect under a random key
    /// of our own that is never sent, and publish nothing until the key
    /// authority's rotation is installed ([`Self::install_group_key`]) or we
    /// rotate ourselves ([`Self::rotate_key`]).
    pub async fn join_group_call(
        &self,
        call_id: CallId,
        joiner_id: UserId,
    ) -> Result<Call, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::JoinCall {
                call_id,
                encryption_root_key: None,
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

    /// Leave the current call. `reason` is what the emitted `ActiveEnded`
    /// reports.
    pub async fn leave_call(&self, reason: CallEndReason) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::LeaveCall {
                reason,
                response: tx,
            })
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

    /// Disconnect locally without server notification. `reason` is what the
    /// emitted `ActiveEnded` reports, so the UI can say why the call went away.
    pub async fn disconnect_local(&self, reason: CallEndReason) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.try_send(CallCommand::DisconnectLocal {
            call: None,
            reason,
            response: tx,
        });
        let _ = rx.await;
    }

    /// Fire-and-forget variant of [`disconnect_local`] for use in sync
    /// event handlers (e.g. `handle_domain_event`) that can't `.await`.
    /// The worker will disconnect LiveKit, clear `active_call`, clear the
    /// orchestrator slot, and emit `ActiveEnded` — same as the awaited
    /// version, just without backpressure. With `call`, a no-op unless that
    /// is the active call.
    pub fn disconnect_local_nonblocking(&self, call: Option<CallHandle>, reason: CallEndReason) {
        let (tx, _rx) = oneshot::channel();
        let _ = self.cmd_tx.try_send(CallCommand::DisconnectLocal {
            call,
            reason,
            response: tx,
        });
    }

    /// Get the currently active call.
    pub async fn active_call(&self) -> Option<Call> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetActiveCall { response: tx })
            .ok()?;
        rx.await.ok().flatten()
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
        if self
            .cmd_tx
            .try_send(CallCommand::IsInCall { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Identities (user ids) of the remote peers currently in the LiveKit room.
    ///
    /// This is the real room roster. `Call::participants` is not — nothing
    /// tracks remote joins/leaves into it — so anything that must act on "who
    /// is actually in the call right now" (notably key rotation) reads this.
    pub async fn remote_participant_identities(&self) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetRemoteParticipants { response: tx })
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Who has been in the LiveKit room longest — what the group-call key
    /// authority is chosen by. `None` when not connected.
    pub async fn room_seniority(&self) -> Option<RoomSeniority> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetRoomSeniority { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// The active call's group-key standing: which call, the generation of
    /// the newest key from outside our own chain, and whether our media is
    /// still held back for want of a key. `None` when not in a call.
    pub async fn group_key_status(&self) -> Option<GroupKeyStatus> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetGroupKeyStatus { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Get room connection state.
    pub async fn room_state(&self) -> RoomState {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetRoomState { response: tx })
            .is_err()
        {
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
        let _ = self
            .cmd_tx
            .try_send(CallCommand::SetMicrophoneMuted { muted });
    }

    /// Check if microphone is muted.
    pub async fn is_microphone_muted(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsMicrophoneMuted { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Locally mute/unmute a specific remote peer's audio. Fire-and-forget;
    /// drops silently if the worker has shut down.
    pub fn set_peer_audio_muted(&self, participant_id: String, muted: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetPeerAudioMuted {
            participant_id,
            muted,
        });
    }

    /// Route remote audio to the named output device.
    pub async fn set_audio_output(&self, device: String) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::SetAudioOutput {
                device,
                response: tx,
            })
            .map_err(|_| "call worker is not running".to_string())?;
        rx.await
            .map_err(|_| "call worker dropped the request".to_string())?
    }

    /// Locally pause/resume a specific remote peer's video.
    pub fn set_peer_video_paused(&self, participant_id: String, paused: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetPeerVideoPaused {
            participant_id,
            paused,
        });
    }

    /// Check if camera is enabled.
    pub async fn is_camera_enabled(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsCameraEnabled { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Publish screen share (desktop only).
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
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
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn stop_screen_share(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopScreenShare);
    }

    /// Check if screen sharing.
    pub async fn is_screen_sharing(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsScreenSharing { response: tx })
            .is_err()
        {
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
        if self
            .cmd_tx
            .try_send(CallCommand::RatchetKey { response: tx })
            .is_err()
        {
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
            .try_send(CallCommand::RotateKey {
                new_key,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Install a group-call `(key, epoch)` received from a participant of
    /// `call_id`. Returns `Ok(false)` for a stale epoch, `Err(NotInCall)` when
    /// `call_id` is not the active call.
    pub async fn install_group_key(
        &self,
        call_id: CallId,
        key: [u8; 32],
        epoch: u32,
    ) -> Result<bool, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::InstallGroupKey {
                call_id,
                key,
                epoch,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Set encryption key externally (e.g., from CallKeyState ratchet).
    ///
    /// Use this when managing keys via the Signal-based CallKeyState.
    pub async fn set_encryption_key(&self, key: Vec<u8>) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::SetEncryptionKey { key, response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Get the current key epoch.
    pub async fn key_epoch(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetKeyEpoch { response: tx })
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Test-only: `(epoch, ring)` where `ring[i]` is the shared key LiveKit's
    /// `KeyProvider` holds at key index `i` — the material frames are actually
    /// encrypted with, not our own bookkeeping. `None` when not connected.
    #[cfg(feature = "testing")]
    pub async fn installed_key_ring(&self) -> Option<(u32, Vec<Option<Vec<u8>>>)> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetInstalledKeyRing { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Test-only: how many tracks we publish to the room (0 when not in one).
    #[cfg(feature = "testing")]
    pub async fn local_track_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetLocalTrackCount { response: tx })
            .is_err()
        {
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
        // Single-threaded: this state lives entirely on the call-worker thread
        // (created here, accessed synchronously or from spawn_local tasks, never
        // sent across threads — the handle reaches it only via the command
        // channel). Rc<RefCell> is the honest type; Rc-not-Send compile-guards
        // the invariant.
        let active_call: Rc<RefCell<Option<ActiveCallState>>> =
            Rc::new(RefCell::new(None));

        // Key ratchet timer control
        let ratchet_cancel: Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>> =
            Rc::new(RefCell::new(None));

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
                        // the DM caller connects under an all-zero
                        // placeholder key — its real ECDH key only lands later
                        // in complete_dm_call. Publishing here would leak live
                        // mic/cam to the SFU under a fully server-derivable key.
                        // So defer the DM-caller publish until the real key is
                        // installed (complete_dm_call publishes right after
                        // set_encryption_key). Group/join connect with a real
                        // random key, so they still publish immediately.
                        //
                        // A group call the server says someone else started is
                        // one we joined (`CreateCall` returns a channel's live
                        // call): we hold no key of the call's yet, so publishing
                        // waits behind the gate like any joiner's.
                        let gated = active_call.borrow().as_ref().is_some_and(|s| s.publish_gate.is_closed());
                        if encryption_root_key != [0u8; 32] && !gated {
                            auto_publish_media(&room_manager, matches!(call_type, CallType::Video)).await;
                        }
                    }

                    let _ = response.send(result);
                }

                CallCommand::JoinCall {
                    call_id,
                    encryption_root_key,
                    joiner_id,
                    response,
                } => {
                    tracing::info!(stage = "join_call_handler_start", %call_id, elapsed_us = 0u64, "Call stage");
                    let result = join_call_impl(
                        &call_client,
                        &room_manager,
                        &active_call,
                        call_id,
                        encryption_root_key,
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
                        // A group joiner publishes once it holds the call's key.
                        if encryption_root_key.is_some() {
                            auto_publish_media(&room_manager, is_video).await;
                        }
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

                CallCommand::LeaveCall { reason, response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    // Capture handle before leave clears active_call
                    let handle = active_call.borrow().as_ref().map(derive_call_handle);

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
                            reason,
                        });
                    }

                    let _ = response.send(result);
                }

                CallCommand::EndCall { reason, response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let handle = active_call.borrow().as_ref().map(derive_call_handle);

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

                CallCommand::DisconnectLocal { call, reason, response } => {
                    let handle = active_call.borrow().as_ref().map(derive_call_handle);
                    if call.is_some() && call != handle {
                        tracing::info!(
                            call = ?call.as_ref().map(CallHandle::as_str),
                            active = ?handle.as_ref().map(CallHandle::as_str),
                            "Hangup for a call that is not the active one; ignored"
                        );
                        let _ = response.send(());
                        continue;
                    }

                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    room_manager.disconnect().await;
                    *active_call.borrow_mut() = None;
                    tracing::info!("Disconnected locally from call");

                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason,
                        });
                    }

                    let _ = response.send(());
                }

                CallCommand::SwapCallClient { client } => {
                    // Safe by construction: this arm only runs between complete
                    // call operations (the loop awaits each fully), so the
                    // signaling client's mutex is free and no RPC is in flight.
                    // Any active LiveKit media session lives in `room_manager`,
                    // independent of this signaling client, so an ongoing call
                    // is not disturbed — only the NEXT signaling RPC (e.g. the
                    // eventual leave/end) egresses over the new transport.
                    *call_client.lock().await = client;
                    tracing::info!("call worker: swapped gRPC call client for new transport");
                }

                CallCommand::GetActiveCall { response } => {
                    let call = active_call.borrow().as_ref().map(|a| a.call.clone());
                    let _ = response.send(call);
                }

                CallCommand::GetRoomName { response } => {
                    let name = active_call.borrow().as_ref().map(|a| a.room_name.clone());
                    let _ = response.send(name);
                }

                CallCommand::IsInCall { response } => {
                    let _ = response.send(active_call.borrow().is_some());
                }

                CallCommand::GetRoomState { response } => {
                    let _ = response.send(room_manager.state());
                }

                CallCommand::GetRemoteParticipants { response } => {
                    let _ = response.send(room_manager.remote_participant_identities());
                }

                CallCommand::GetRoomSeniority { response } => {
                    let seniority = room_manager.room_seniority().map(|(local_joined_at_ms, remote)| {
                        RoomSeniority { local_joined_at_ms, remote }
                    });
                    let _ = response.send(seniority);
                }

                #[cfg(feature = "testing")]
                CallCommand::GetLocalTrackCount { response } => {
                    let _ = response.send(room_manager.local_track_count());
                }

                CallCommand::GetGroupKeyStatus { response } => {
                    let status = active_call.borrow().as_ref().map(|s| GroupKeyStatus {
                        call_id: s.call.id,
                        anchor: s.key_state.anchor(),
                        awaiting_key: s.publish_gate.is_closed(),
                    });
                    let _ = response.send(status);
                }

                CallCommand::PublishMicrophone { device_id, response } => {
                    let result = if gate_defers(&active_call, |g| g.defer_mic(device_id.clone())) {
                        tracing::info!("Microphone held back until the call's key arrives");
                        Ok(())
                    } else {
                        room_manager.publish_microphone(device_id.as_deref()).await
                    };
                    let _ = response.send(result);
                }

                CallCommand::PublishCamera {
                    device_path,
                    config,
                    response,
                } => {
                    if gate_defers(&active_call, PublishGate::defer_camera) {
                        tracing::info!("Camera held back until the call's key arrives");
                        let _ = response.send(Ok(()));
                        continue;
                    }
                    // Spawn — never await a capture publish inside the command
                    // loop (see PublishScreenShare below for the full rationale).
                    let rm = room_manager.clone();
                    tokio::task::spawn_local(async move {
                        let result = rm.publish_camera(device_path.as_deref(), config).await;
                        let _ = response.send(result);
                    });
                }

                CallCommand::StopMicrophone => {
                    gate_defers(&active_call, |g| { g.drop_mic(); false });
                    room_manager.stop_microphone();
                }

                CallCommand::StopCamera => {
                    gate_defers(&active_call, |g| { g.drop_camera(); false });
                    room_manager.stop_camera().await;
                }

                CallCommand::SetMicrophoneMuted { muted } => {
                    room_manager.set_microphone_muted(muted);
                }

                CallCommand::IsMicrophoneMuted { response } => {
                    let _ = response.send(room_manager.is_microphone_muted());
                }

                CallCommand::IsCameraEnabled { response } => {
                    let _ = response.send(room_manager.is_camera_enabled());
                }

                CallCommand::SetPeerAudioMuted { participant_id, muted } => {
                    room_manager.set_peer_audio_muted(&participant_id, muted);
                }

                CallCommand::SetAudioOutput { device, response } => {
                    let _ = response.send(room_manager.set_audio_output(&device).map_err(|e| e.to_string()));
                }

                CallCommand::SetPeerVideoPaused { participant_id, paused } => {
                    room_manager.set_peer_video_paused(&participant_id, paused);
                }

                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                CallCommand::PublishScreenShare { source, response } => {
                    if gate_defers(&active_call, |g| g.is_closed()) {
                        let _ = response.send(Err(CallError::AwaitingKey));
                        continue;
                    }
                    // CRITICAL: spawn, don't await here. publish_screen_share runs
                    // the desktop-capture handshake, which on Wayland waits on the
                    // XDG portal and can stall for tens of seconds (flaky portals)
                    // or until the user picks a source. Awaiting it inside the
                    // sequential command loop wedges the ENTIRE worker: GetRoomState
                    // and every other command block, so call_is_connected() hangs
                    // and all call UI buttons (share, video, …) go dead with no
                    // feedback. Spawning keeps the loop live; the oneshot still
                    // carries the result back when the publish finishes or fails.
                    let rm = room_manager.clone();
                    tokio::task::spawn_local(async move {
                        let result = rm.publish_screen_share(source, None).await;
                        let _ = response.send(result);
                    });
                }

                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                CallCommand::StopScreenShare => {
                    room_manager.stop_screen_share().await;
                }

                CallCommand::IsScreenSharing { response } => {
                    let _ = response.send(room_manager.is_screen_sharing());
                }

                CallCommand::RotateKey { new_key, response } => {
                    let rotated = active_call.borrow_mut().as_mut().map(|state| {
                        state.key_state.rotate(new_key).map(|_| chain_install(&state.key_state))
                    });
                    let result = match rotated {
                        Some(Ok((epoch, key_cur, key_next))) => {
                            room_manager.set_ratcheted_shared_key(epoch, key_cur, key_next);
                            tracing::info!(epoch, "Call key rotated after a membership change");
                            // We decided this key, so we hold the current one.
                            open_publish_gate(&active_call, &room_manager);
                            Ok(epoch)
                        }
                        Some(Err(e)) => Err(CallError::CryptoError(e.to_string())),
                        None => Err(CallError::NotInCall),
                    };
                    let _ = response.send(result);
                }

                CallCommand::InstallGroupKey { call_id, key, epoch, response } => {
                    let adopted = adopt_into_call(active_call.borrow_mut().as_mut(), call_id, key, epoch);
                    let result = adopted.map(|adopted| match adopted {
                        Some((epoch, key_cur, key_next)) => {
                            // Key first, then media: the deferred publishes
                            // start under the key just installed.
                            room_manager.set_ratcheted_shared_key(epoch, key_cur, key_next);
                            tracing::info!(epoch, "Installed group call key from participant");
                            open_publish_gate(&active_call, &room_manager);
                            true
                        }
                        None => {
                            tracing::debug!(epoch, "Group call key is not newer, or too far ahead; ignored");
                            false
                        }
                    });
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
                            let mut guard = active_call.borrow_mut();
                            // Install the SAME key the callee gave LiveKit: the
                            // HKDF-derived `current_key_vec()`, NOT the raw ECDH
                            // secret. The callee connects with
                            // `CallKeyState::new(secret, call_id).current_key_vec()`
                            // (see join_call_impl), so installing the raw secret
                            // here leaves the caller's LiveKit index-0 key one HKDF
                            // step off from the callee's — E2EE frames then never
                            // decrypt and remote video/audio stays black.
                            let derived = if let Some(state) = guard.as_mut() {
                                let call_id = state.key_state.call_id().to_string();
                                match CallKeyState::new(&arr, &call_id) {
                                    Ok(ks) => {
                                        let k = ks.current_key_vec();
                                        state.key_state = ks;
                                        Some(k)
                                    }
                                    Err(e) => {
                                        tracing::error!("Re-root key_state failed: {}", e);
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            drop(guard);
                            match derived {
                                Some(k) => room_manager.reset_shared_key(k),
                                None => false,
                            }
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

                #[cfg(feature = "testing")]
                CallCommand::GetInstalledKeyRing { response } => {
                    let _ = response.send(room_manager.installed_key_ring());
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
                        conversation_id: snapshot.conversation_id,
                        replaces_active,
                    });
                }

                CallCommand::OrchWithdrawIncoming { call, reason } => {
                    if orchestrator.incoming.remove(&call).is_some() {
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
                    if let Some(state) = active_call.borrow_mut().as_mut() {
                        if derive_call_handle(state) == call && state.call.connected_at.is_none() {
                            state.call.start();
                        }
                    }
                    // Clear from pending maps regardless of whether it was
                    // incoming or outgoing — either way it's now connected.
                    orchestrator.outgoing.remove(&call);
                    orchestrator.incoming.remove(&call);
                    orchestrator.active = Some(OrchestratedActive {
                        call: call.clone(),
                        kind,
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
                        let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                            call,
                            reason,
                        });
                    } else {
                        // Not in any slot — an outgoing call, which nothing
                        // registers. The caller is still telling us the call
                        // is over, and swallowing that left the "Calling…"
                        // view up after a peer declined and left call history
                        // with a row that never closed. Announce it anyway;
                        // subscribers key off the handle, not the slot.
                        tracing::debug!(
                            call = %call.as_str(),
                            "OrchMarkEnded: untracked call — forwarding terminal event"
                        );
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call,
                            reason,
                        });
                    }
                }

                CallCommand::OrchAcceptIncoming { call, response } => {
                    // Fail if an active call is holding the slot — UI must
                    // explicitly send the replacing variant so the user has
                    // chosen to end their current call.
                    let result = if orchestrator.active.is_some() {
                        Err(CallError::AlreadyInCall)
                    } else if let Some(p) = orchestrator.incoming.remove(&call) {
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

                CallCommand::OrchActiveKind { response } => {
                    let _ = response.send(orchestrator.call_kind());
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

/// The room's seniority: our LiveKit `joined_at_ms` and each remote
/// participant's `(identity, joined_at_ms)`. Server-assigned, so every
/// participant sees the same order — and server-asserted, like the roster.
#[derive(Debug, Clone)]
pub struct RoomSeniority {
    pub local_joined_at_ms: i64,
    pub remote: Vec<(String, i64)>,
}

/// Internal state for active call.
struct ActiveCallState {
    call: Call,
    /// Key state for Signal-based key derivation and ratcheting.
    key_state: CallKeyState,
    /// Room name for potential future UI display during reconnection.
    room_name: String,
    /// LiveKit server URL for reconnection.
    livekit_url: String,
    /// Closed while we hold none of the call's keys (a group joiner).
    publish_gate: PublishGate,
}

/// The active call's group-key standing ([`CallWorkerHandle::group_key_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupKeyStatus {
    pub call_id: CallId,
    /// Generation of the newest key from outside our chain
    /// ([`CallKeyState::anchor`]); 0 = only our starting key.
    pub anchor: u32,
    /// Our media is held back: we hold no key of the call's yet.
    pub awaiting_key: bool,
}

/// Media a channel group-call participant may not publish yet (§124 1k).
///
/// A joiner connects under a random key of its own that nobody else holds —
/// the ring no longer carries one — so frames it sent before the key
/// authority's key arrived would be undecryptable to everyone, and before
/// this gate they went out under the ring's root key, which every channel
/// member held. Closed, it publishes nothing and remembers what was asked
/// for; it opens when the authority's key is installed or we rotate
/// ourselves, and the remembered mic/camera are published then — after the
/// key is in LiveKit, so the first frame already uses it.
#[derive(Debug, Default)]
struct PublishGate {
    closed: bool,
    /// `Some(device)`: publish the microphone (on `device`) once open.
    mic: Option<Option<String>>,
    camera: bool,
}

/// What a [`PublishGate`] had held back when it opened.
#[derive(Debug, Default, PartialEq, Eq)]
struct DeferredMedia {
    mic: Option<Option<String>>,
    camera: bool,
}

impl PublishGate {
    /// A joiner's gate: closed, with the call's auto-publish waiting in it.
    fn awaiting_key(is_video: bool) -> Self {
        Self {
            closed: true,
            mic: Some(None),
            camera: is_video,
        }
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    /// Hold back a microphone publish; false (nothing held) when open.
    fn defer_mic(&mut self, device: Option<String>) -> bool {
        if self.closed {
            self.mic = Some(device);
        }
        self.closed
    }

    /// Hold back a camera publish; false (nothing held) when open.
    fn defer_camera(&mut self) -> bool {
        if self.closed {
            self.camera = true;
        }
        self.closed
    }

    fn drop_mic(&mut self) {
        self.mic = None;
    }

    fn drop_camera(&mut self) {
        self.camera = false;
    }

    /// Open the gate: what it held back, the first time; `None` when it
    /// was already open.
    fn open(&mut self) -> Option<DeferredMedia> {
        if !self.closed {
            return None;
        }
        self.closed = false;
        Some(DeferredMedia {
            mic: self.mic.take(),
            camera: std::mem::take(&mut self.camera),
        })
    }
}

/// Run `f` on the active call's publish gate; false when not in a call.
fn gate_defers(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    f: impl FnOnce(&mut PublishGate) -> bool,
) -> bool {
    active_call
        .borrow_mut()
        .as_mut()
        .is_some_and(|s| f(&mut s.publish_gate))
}

/// Open the active call's publish gate and publish what it held back.
/// Spawned: capture start-up must not hold the command loop.
fn open_publish_gate(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    room_manager: &Arc<RoomManager>,
) {
    let Some(media) = active_call
        .borrow_mut()
        .as_mut()
        .and_then(|s| s.publish_gate.open())
    else {
        return;
    };
    tracing::info!(
        mic = media.mic.is_some(),
        camera = media.camera,
        "Call key held: publishing"
    );
    let rm = room_manager.clone();
    tokio::task::spawn_local(async move {
        if let Some(device) = media.mic {
            if let Err(e) = rm.publish_microphone(device.as_deref()).await {
                tracing::warn!(error = %e, "deferred microphone publish failed");
            }
        }
        if media.camera {
            if let Err(e) = rm.publish_camera(None, None).await {
                tracing::warn!(error = %e, "deferred camera publish failed");
            }
        }
    });
}

/// [`adopt_group_key`] into the active call, which must be `call_id`.
fn adopt_into_call(
    active: Option<&mut ActiveCallState>,
    call_id: CallId,
    key: [u8; 32],
    epoch: u32,
) -> Result<Option<(u32, Vec<u8>, Vec<u8>)>, CallError> {
    match active {
        Some(state) if state.call.id == call_id => {
            Ok(adopt_group_key(&mut state.key_state, key, epoch))
        }
        _ => Err(CallError::NotInCall),
    }
}

/// Adopt a group-call `key` issued at `epoch` into `key_state`
/// ([`CallKeyState::adopt`]), returning what to hand LiveKit. `None` when the
/// key is refused (not newer than the last one adopted, or too far ahead).
fn adopt_group_key(
    key_state: &mut CallKeyState,
    key: [u8; 32],
    epoch: u32,
) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    match key_state.adopt(key, epoch) {
        Ok(true) => Some(chain_install(key_state)),
        Ok(false) => None,
        Err(e) => {
            tracing::error!(error = %e, "Group call key derivation failed");
            None
        }
    }
}

/// What to hand LiveKit for the chain's current position (its epoch = the
/// key index): that key, and the next epoch's pre-installed — the same shape
/// the periodic ratchet installs, so a rotator and every receiver of its key
/// hold byte-identical chains and LiveKit indices.
fn chain_install(key_state: &CallKeyState) -> (u32, Vec<u8>, Vec<u8>) {
    let key_cur = key_state.current_key_vec();
    let key_next = key_state
        .peek_next_key()
        .map(|k| k.to_vec())
        .unwrap_or_else(|_| key_cur.clone());
    (key_state.epoch(), key_cur, key_next)
}

#[allow(clippy::too_many_arguments)]
async fn initiate_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    channel_id: ChannelId,
    title: &str,
    call_type: CallType,
    encryption_root_key: &[u8; 32],
    initiator_id: UserId,
) -> Result<Call, CallError> {
    // Check if already in a call
    if active_call.borrow().is_some() {
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
    let call_id = CallId::from_str(&response.call_id).map_err(|_| CallError::InvalidCallId)?;

    // `CreateCall` hands back a group channel's live call when there is one:
    // then its creator is someone else and we have joined it, holding none of
    // its keys (the key we connected with is ours alone).
    let creator = response
        .call_info
        .as_ref()
        .and_then(|ci| UserId::from_str(&ci.created_by).ok())
        .unwrap_or(initiator_id);
    let joined_existing = creator != initiator_id && encryption_root_key != &[0u8; 32];
    let mut call = Call::new(channel_id, creator, call_type);
    call.id = call_id;
    // The DM caller joins with a placeholder before the peer answers. Keep
    // connected_at unset until OrchMarkConnected; group calls connect now.
    if encryption_root_key == &[0u8; 32] {
        call.ring();
    } else {
        call.start();
    }
    // Retain server creation time for signaling, separate from connected_at.
    if let Some(dt) = response
        .call_info
        .as_ref()
        .and_then(|ci| server_ns_to_utc(ci.created_at))
    {
        call.started_at = Some(dt);
    }
    call.add_participant(Participant::new(initiator_id));

    // Store active call
    *active_call.borrow_mut() = Some(ActiveCallState {
        call: call.clone(),
        key_state,
        room_name: response.room_name,
        livekit_url: response.livekit_url,
        publish_gate: if joined_existing {
            PublishGate::awaiting_key(matches!(call_type, CallType::Video))
        } else {
            PublishGate::default()
        },
    });

    Ok(call)
}

/// Convert a server Unix-nanoseconds timestamp to `DateTime<Utc>`, or `None`
/// when the value is 0/out of range (caller then keeps the local `started_at`).
fn server_ns_to_utc(ns: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    if ns == 0 {
        return None;
    }
    chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
}

async fn join_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    call_id: CallId,
    encryption_root_key: Option<[u8; 32]>,
    joiner_id: UserId,
) -> Result<Call, CallError> {
    // Check if already in a call
    if active_call.borrow().is_some() {
        return Err(CallError::AlreadyInCall);
    }

    // Join call on server
    let response = {
        let mut client = call_client.lock().await;
        let rpc_started = std::time::Instant::now();
        tracing::info!(stage = "join_call_rpc_begin", %call_id, elapsed_us = 0u64, "Call stage");
        let joined = client.join_call(&call_id.to_string()).await;
        let stage = if joined.is_ok() {
            "join_call_rpc_succeeded"
        } else {
            "join_call_rpc_failed"
        };
        tracing::info!(stage, %call_id, elapsed_us = rpc_started.elapsed().as_micros() as u64, "Call stage");
        joined.map_err(|e| CallError::ServerError(e.to_string()))?
    };

    // Derive E2EE key. A group joiner has none of the call's: it connects
    // under a random key of its own, never sent, and publishes nothing until
    // the key authority's key is installed (`PublishGate`).
    let root = zeroize::Zeroizing::new(
        encryption_root_key.unwrap_or_else(crate::crypto::generate_group_call_key),
    );
    let key_state = CallKeyState::new(&root, &call_id.to_string())
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
    let server_channel_id =
        ChannelId::from_str(&call_info.channel_id).map_err(|_| CallError::InvalidChannelId)?;
    let initiator_id =
        UserId::from_str(&call_info.created_by).map_err(|_| CallError::InvalidUserId)?;
    let channel_id = joined_channel_id(server_channel_id, &initiator_id, &joiner_id);

    let call_type =
        match ProtoCallType::try_from(call_info.r#type).unwrap_or(ProtoCallType::Unspecified) {
            ProtoCallType::Voice => CallType::Voice,
            ProtoCallType::Video | ProtoCallType::ScreenShare => CallType::Video,
            _ => CallType::Voice,
        };

    let mut call = Call::new(channel_id, initiator_id, call_type);
    call.id = call_id;
    call.start();
    // Share the server creation time for key-ratchet boundaries; duration
    // separately uses the local connected_at timestamp.
    if let Some(dt) = server_ns_to_utc(call_info.created_at) {
        call.started_at = Some(dt);
    }
    call.add_participant(Participant::new(joiner_id));

    // Store active call
    let publish_gate = if encryption_root_key.is_none() {
        PublishGate::awaiting_key(matches!(call.call_type, CallType::Video))
    } else {
        PublishGate::default()
    };
    *active_call.borrow_mut() = Some(ActiveCallState {
        call: call.clone(),
        key_state,
        room_name: call_info.room_name,
        livekit_url: response.livekit_url,
        publish_gate,
    });

    Ok(call)
}

/// The conversation a joined call belongs to, from the joiner's side.
///
/// A channel-free 1:1 call is created with the caller's scoping id, which is
/// `ChannelId::from(callee)` — so the callee reads its own id back from the
/// server. Filed verbatim, that is the callee's Saved Messages; from here the
/// conversation is the DM with the caller. Group and channel-backed DM calls
/// carry a real channel id and pass through.
fn joined_channel_id(
    server_channel_id: ChannelId,
    initiator_id: &UserId,
    joiner_id: &UserId,
) -> ChannelId {
    if server_channel_id == ChannelId::from(*joiner_id) {
        ChannelId::from(*initiator_id)
    } else {
        server_channel_id
    }
}

/// Connect directly to LiveKit with a pre-made access token (for anonymous calls).
///
/// This bypasses the server call creation and directly connects to LiveKit.
/// Used for anonymous calls where we get the token from the anonymous call service.
async fn connect_with_token_impl(
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    access_token: String,
    livekit_url: String,
    encryption_key: Vec<u8>,
    room_name: String,
    call_id: CallId,
    channel_id: ChannelId,
) -> Result<(), CallError> {
    // Check if already in a call
    if active_call.borrow().is_some() {
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
        call_type: CallType::Voice,  // Default to voice for anonymous calls
        state: CallState::Connected,
        participants: vec![],
        started_at: Some(chrono::Utc::now()),
        connected_at: Some(chrono::Utc::now()),
        ended_at: None,
        is_encrypted: true, // Anonymous calls are always encrypted (oblivious)
    };

    // Create key state from real encryption key
    let mut key_arr = [0u8; 32];
    if encryption_key.len() >= 32 {
        key_arr.copy_from_slice(&encryption_key[..32]);
    }
    // Use room_name as call_id for CallKeyState — both sides know it, ensuring
    // coordinated ratcheting produces identical keys on both sides
    let key_state = CallKeyState::new(&key_arr, &room_name)
        .map_err(|e| CallError::CryptoError(e.to_string()))?;

    // Store active call info
    *active_call.borrow_mut() = Some(ActiveCallState {
        call: call.clone(),
        key_state,
        room_name: room_name.clone(),
        livekit_url: livekit_url.to_string(),
        publish_gate: PublishGate::default(),
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
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    let (call_id, is_anonymous) = {
        let active = active_call.borrow();
        let state = active.as_ref().ok_or(CallError::NotInCall)?;
        (
            state.call.id.to_string(),
            state.room_name.starts_with("anon-"),
        )
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
    *active_call.borrow_mut() = None;

    Ok(())
}

async fn end_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    reason: Option<&str>,
) -> Result<(), CallError> {
    let call_id = {
        let active = active_call.borrow();
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
    *active_call.borrow_mut() = None;

    Ok(())
}

/// Reconnect to the current call after a connection drop.
///
/// This gets a new token from the server and reconnects to LiveKit.
async fn reconnect_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    // Get current call info
    let (call_id, key_vec, livekit_url) = {
        let active = active_call.borrow();
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

/// Given the seconds elapsed since the shared call-start anchor, return the
/// epoch that should be current now and the delay (seconds) until the next
/// ratchet boundary at `call_start + (epoch+1) * interval`.
///
/// Pure so it can be unit-tested without a running call. Both peers feed the
/// same `call_start`, so they compute the same boundaries and ratchet in
/// lockstep regardless of when each connected.
fn aligned_ratchet_schedule(elapsed_secs: f64, interval_secs: u64) -> (u32, f64) {
    let interval = interval_secs as f64;
    let elapsed = elapsed_secs.max(0.0);
    let epoch_now = (elapsed / interval).floor() as u32;
    let next_boundary = (f64::from(epoch_now) + 1.0) * interval;
    (epoch_now, (next_boundary - elapsed).max(0.0))
}

/// A timer tick can fire a little before the wall-clock boundary it was
/// scheduled for (monotonic and wall clocks drift); it still counts as that
/// boundary.
const RATCHET_TICK_TOLERANCE_SECS: f64 = 0.5;

/// Most epochs one fast-forward may ratchet through: 24 h of 30 s epochs. A
/// server-asserted `started_at` further back would otherwise have the call
/// worker hash up to `u32::MAX` times.
const MAX_RATCHET_CATCH_UP: u32 = 24 * 60 * 2;

/// The chain position the wall clock says is current for a call that
/// started at `call_start`.
fn wall_clock_epoch(call_start: chrono::DateTime<chrono::Utc>) -> u32 {
    let elapsed = (chrono::Utc::now() - call_start).num_milliseconds() as f64 / 1000.0;
    aligned_ratchet_schedule(
        elapsed + RATCHET_TICK_TOLERANCE_SECS,
        KEY_RATCHET_INTERVAL_SECS,
    )
    .0
}

/// Ratchet `key_state` up to chain position `target`, returning what to hand
/// LiveKit when it moved. A chain already at or past `target` stays put: a
/// rotation is issued one position ahead (`CallKeyState::rotate`) and the
/// clock catches up to it rather than the chain running further ahead — so
/// every participant's position stays the wall-clock epoch (or the newest
/// rotation's, until the clock passes it), however many rotations the call
/// has had — so a joiner's first key stays within `MAX_JOIN_LEAD`.
fn ratchet_to(key_state: &mut CallKeyState, target: u32) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    let behind = target.checked_sub(key_state.epoch()).filter(|n| *n > 0)?;
    if behind > MAX_RATCHET_CATCH_UP {
        tracing::error!(
            epoch = key_state.epoch(),
            target,
            "Call key ratchet target out of range; not advancing"
        );
        return None;
    }
    while key_state.epoch() < target {
        if let Err(e) = key_state.ratchet() {
            tracing::error!("Key ratchet failed: {}", e);
            return None;
        }
    }
    Some(chain_install(key_state))
}

/// Start the coordinated key ratchet timer.
///
/// Both peers ratchet at wall-clock boundaries anchored to the server-assigned
/// call start time (`call.started_at`, identical for every participant). A peer
/// that connects 4s later than another still ratchets epoch N at exactly
/// `call_start + N*interval`, so the two never drift out of sync — the earlier
/// free-running `interval()` (counting from each peer's own connect moment) let
/// them ratchet up to `interval` seconds apart, which froze video whenever the
/// skew window coincided with a keyframe/loss (LiveKit's own recovery ratchet is
/// disabled). A peer joining after boundaries have already passed fast-forwards
/// its key chain to the current epoch on start.
///
/// The ratchet uses `CallKeyState::ratchet()` (HKDF-SHA256 with call_id + epoch)
/// which is deterministic, then pushes the new key to LiveKit.
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
    ratchet_cancel: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) {
    // Cancel any existing timer
    stop_key_ratchet_timer(ratchet_cancel);

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    *ratchet_cancel.borrow_mut() = Some(cancel_tx);

    // Shared wall-clock anchor. Read once — it's fixed for the call's lifetime.
    let call_start = active_call
        .borrow()
        .as_ref()
        .and_then(|s| s.call.started_at);

    let room_manager = room_manager.clone();
    let active_call = active_call.clone();

    tokio::task::spawn_local(async move {
        // Advance our deterministic CallKeyState to the wall-clock epoch (one
        // step past ours when there is no anchor) and push that epoch's key
        // plus the next (forward window) to LiveKit. Returns false if the call
        // ended (caller should stop the timer).
        let ratchet_once = |active_call: &Rc<RefCell<Option<ActiveCallState>>>,
                            room_manager: &Arc<RoomManager>|
         -> bool {
            let ratcheted = {
                let mut call_guard = active_call.borrow_mut();
                let Some(state) = call_guard.as_mut() else {
                    tracing::debug!("Key ratchet timer stopping: no active call");
                    return false;
                };
                let target = match call_start {
                    Some(start) => wall_clock_epoch(start),
                    None => state.key_state.epoch().saturating_add(1),
                };
                ratchet_to(&mut state.key_state, target)
            };
            if let Some((epoch, key_cur, key_next)) = ratcheted {
                tracing::info!(epoch, "Coordinated key ratchet completed");
                if !room_manager.set_ratcheted_shared_key(epoch, key_cur, key_next) {
                    tracing::warn!("Failed to push ratcheted key to LiveKit");
                }
            }
            true
        };

        // Align the first tick to the next boundary past `call_start`, and
        // catch up any epochs that elapsed before we connected.
        let first_delay_secs = match call_start {
            Some(start) => {
                let elapsed = (chrono::Utc::now() - start).num_milliseconds() as f64 / 1000.0;
                let (epoch_now, delay) =
                    aligned_ratchet_schedule(elapsed, KEY_RATCHET_INTERVAL_SECS);

                // Fast-forward to the wall-clock epoch (a peer that joined after
                // one or more boundaries already passed).
                let ratcheted = active_call
                    .borrow_mut()
                    .as_mut()
                    .and_then(|state| ratchet_to(&mut state.key_state, epoch_now));
                if let Some((epoch, key_cur, key_next)) = ratcheted {
                    tracing::info!(epoch, "Fast-forwarded key to wall-clock epoch on join");
                    room_manager.set_ratcheted_shared_key(epoch, key_cur, key_next);
                }
                delay
            }
            // No anchor (shouldn't happen): behave like the old free-running timer.
            None => KEY_RATCHET_INTERVAL_SECS as f64,
        };

        // Wait to the aligned boundary, then ratchet every interval from there.
        let sleep = tokio::time::sleep(tokio::time::Duration::from_secs_f64(first_delay_secs));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            _ = &mut cancel_rx => {
                tracing::debug!("Key ratchet timer cancelled");
                return;
            }
        }

        // interval_at with start=now fires its first tick immediately (at the
        // boundary we just reached), then every interval after.
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now(),
            tokio::time::Duration::from_secs(KEY_RATCHET_INTERVAL_SECS),
        );

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if !ratchet_once(&active_call, &room_manager) {
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
        "Key ratchet timer started (wall-clock aligned)"
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

/// Stop the periodic key ratchet timer.
fn stop_key_ratchet_timer(ratchet_cancel: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>) {
    if let Some(cancel_tx) = ratchet_cancel.borrow_mut().take() {
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
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = bus.subscribe_domain();
    let mut room_rx = worker.subscribe_room_events();
    tokio::spawn(async move {
        tracing::info!("Call orchestrator EventBus bridge started");
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(event) => handle_domain_event(&worker, &signaling, event),
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "Call orchestrator bridge lagged on DomainEvent channel"
                        );
                    }
                    Err(RecvError::Closed) => {
                        tracing::info!("Call orchestrator bridge stopping — domain bus closed");
                        break;
                    }
                },
                event = room_rx.recv() => match event {
                    Ok(LiveKitRoomEvent::ParticipantLeft { identity }) => {
                        spawn_end_if_left_alone(worker.clone(), identity);
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => {
                        tracing::info!("Call orchestrator bridge stopping — room bus closed");
                        break;
                    }
                },
            }
        }
    })
}

/// How long a 1:1 peer may be absent from the room before the call is ended
/// for them. Covers the peer's own hangup signal landing first (it is sent
/// before they leave the room, and it ends the call with history) and a brief
/// LiveKit reconnect, which reappears under the same identity.
const PEER_LEFT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `departed` leaving ends the call: nobody else is left in the room
/// (`remaining` may still list `departed` — the SDK's eviction order is not
/// something to depend on), and it is a 1:1 or anon group call.
fn peer_left_ends_call(
    kind: Option<OrchestratedCallKind>,
    remaining: &[String],
    departed: &str,
) -> bool {
    matches!(
        kind,
        Some(
            OrchestratedCallKind::DmStandard
                | OrchestratedCallKind::AnonDm
                | OrchestratedCallKind::AnonGroup
        )
    ) && remaining.iter().all(|id| id == departed)
}

/// A 1:1 call (DM or anon DM) is over when the other person leaves the room,
/// and an anon group call when the last other member does.
///
/// The peer's hangup signal normally ends a 1:1 call, but that signal is a
/// message and can be late or lost — the room saying the peer is gone is the
/// ground truth. Without this the caller sat alone in the room, timer
/// running, until they hung up themselves. An anon group member's hangup only
/// leaves (`handle_anon_hangup`), so this is what ends the group call once we
/// are alone. Channel calls are untouched: the initiator rekeys instead.
fn spawn_end_if_left_alone(worker: CallWorkerHandle, departed: String) {
    tokio::spawn(async move {
        let Some(call) = worker.active_call().await.map(|c| c.id) else {
            return;
        };
        let still_ends = || async {
            peer_left_ends_call(
                worker.orch_active_kind().await,
                &worker.remote_participant_identities().await,
                &departed,
            )
        };
        if !still_ends().await {
            return;
        }
        tokio::time::sleep(PEER_LEFT_GRACE).await;
        // Still the same call, still alone? Otherwise the hangup signal already
        // ended it, or the peer came back.
        if worker.active_call().await.map(|c| c.id) != Some(call) || !still_ends().await {
            return;
        }
        tracing::info!(call = %call, "Last other participant left the room — ending the call");
        // The resulting `ActiveEnded` files the DM's Call bubble
        // (`call_history_recorder`), as for any other end of the call.
        worker.disconnect_local(CallEndReason::RemoteHangup).await;
    });
}

/// How long an incoming offer may ring. Signal's ring timeout: an offer older
/// than this — one that sat in the queue while the app was offline, or whose
/// call was answered on another of our devices long ago — never rings.
pub const RING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether an offer received at `sent_ns` (ns since epoch) is past the ring
/// timeout at `now_ns`. For any offer that is the server's receive stamp
/// (Signal's rule; the inbox falls back to the caller's clock only when the
/// server gave none). `0` means nothing was stamped: ring rather than silently
/// drop a live call. A clock ahead of ours saturates to age 0 and rings.
pub(crate) fn offer_is_stale(sent_ns: u64, now_ns: u64) -> bool {
    sent_ns != 0 && now_ns.saturating_sub(sent_ns) > RING_TIMEOUT.as_nanos() as u64
}

pub(crate) fn offer_is_stale_now(sent_ns: u64) -> bool {
    offer_is_stale(sent_ns, crate::util::time::TimestampNanos::now().as_u64())
}

/// The withdrawal reason for a hangup saying another of our devices answered
/// or declined.
fn elsewhere_reason(reason: crate::services::gateway::CallHangupReason) -> Option<CallEndReason> {
    use crate::services::gateway::CallHangupReason as R;
    match reason {
        R::AcceptedElsewhere => Some(CallEndReason::AnsweredElsewhere),
        R::DeclinedElsewhere => Some(CallEndReason::Declined),
        _ => None,
    }
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
            channel_id: _,
            is_identity_verified,
            identity_warning,
            caller_name,
            timestamp,
            ..
        } => {
            let is_video = call_type.eq_ignore_ascii_case("video")
                || call_type.eq_ignore_ascii_case("Video Call");
            if offer_is_stale_now(timestamp) {
                worker.orch_missed_incoming(
                    CallHandle::from(call_id),
                    OrchestratedCallKind::DmStandard,
                    caller_name,
                    is_video,
                    peer_id.to_string(),
                );
                return;
            }
            worker.orch_present_incoming(
                CallHandle::from(call_id),
                OrchestratedCallKind::DmStandard,
                peer_id,
                caller_name,
                is_video,
                is_identity_verified,
                identity_warning,
                Some(ephemeral_public_key),
                // DM has no channel name — notification reads "X is calling".
                None,
                // DMs are channel-free: the conversation key IS the peer.
                peer_id.to_string(),
            );
        }

        DomainEvent::CallHangupReceived {
            call_id,
            peer_id,
            reason,
        } => {
            // Signal's Hangup ACCEPTED/DECLINED: another of OUR devices took
            // this call. Only our own user may say so, and it only stops the
            // ringing — it must never tear down a call this device is in.
            if let Some(reason) = elsewhere_reason(reason) {
                if signaling.is_current_user(&peer_id) {
                    worker.orch_withdraw_incoming(CallHandle::from(call_id), reason);
                    return;
                }
                tracing::warn!(peer_id = %peer_id, "Answered/declined-elsewhere hangup from another user; treating as a plain hangup");
            }
            // The callee declined our outgoing call: say so, and file it as
            // declined rather than unanswered.
            let end = if reason == crate::services::gateway::CallHangupReason::Declined {
                CallEndReason::Declined
            } else {
                CallEndReason::RemoteHangup
            };
            // Still ringing? The caller gave up — stop ringing. A no-op when
            // this call was never presented or was already accepted.
            let call = CallHandle::from(call_id);
            worker.orch_withdraw_incoming(call.clone(), end.clone());
            // In this call? Disconnect from LiveKit + clear active_call +
            // orchestrator. A hangup for any other call leaves this one alone.
            worker.disconnect_local_nonblocking(Some(call), end);
        }

        DomainEvent::AnonCallOfferReceived {
            sender_id,
            room_name,
            call_type,
            caller_display_name,
            conversation_id,
            timestamp,
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
            if offer_is_stale_now(timestamp) {
                worker.orch_missed_incoming(
                    CallHandle::from(room_name),
                    OrchestratedCallKind::AnonDm,
                    peer_name,
                    is_video,
                    conversation_id.to_string(),
                );
                return;
            }
            worker.orch_present_incoming(
                CallHandle::from(room_name),
                OrchestratedCallKind::AnonDm,
                sender_id,
                peer_name,
                is_video,
                true, // anon-auth is treated as verified
                String::new(),
                None,
                // 1:1 anon DM — no channel name.
                None,
                conversation_id.to_string(),
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

        DomainEvent::AnonCallHangupReceived {
            call_room_id,
            sender_id,
            ..
        } => {
            handle_anon_hangup(
                worker,
                signaling,
                OrchestratedCallKind::AnonDm,
                &call_room_id,
                sender_id,
            );
        }
        DomainEvent::AnonGroupCallHangupReceived {
            call_room_id,
            sender_id,
            ..
        } => {
            handle_anon_hangup(
                worker,
                signaling,
                OrchestratedCallKind::AnonGroup,
                &call_room_id,
                sender_id,
            );
        }

        DomainEvent::AnonGroupCallOfferReceived {
            sender_id,
            room_name,
            call_type,
            caller_display_name,
            group_id,
            timestamp,
            ..
        } => {
            let is_video = call_type == 2;
            let peer_name = if caller_display_name.is_empty() {
                "Anonymous".to_string()
            } else {
                caller_display_name
            };
            if offer_is_stale_now(timestamp) {
                worker.orch_missed_incoming(
                    CallHandle::from(room_name),
                    OrchestratedCallKind::AnonGroup,
                    peer_name,
                    is_video,
                    group_id.to_string(),
                );
                return;
            }
            worker.orch_present_incoming(
                CallHandle::from(room_name),
                OrchestratedCallKind::AnonGroup,
                sender_id,
                peer_name,
                is_video,
                true,
                String::new(),
                None,
                // group name would read "X is calling in <group>", but no
                // resolver wired yet — None beats leaking the raw group UUID. Pass a
                // resolved name here when a group-name lookup exists.
                None,
                group_id.to_string(),
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

        // Key answers are completed by the UI/glue layer against the
        // ChatterClient API; group-call signals (ring, rotation, key request)
        // by `ChatterClient::handle_group_call_signal`, which needs channel
        // membership and the server's view of the call.
        DomainEvent::CallKeyAnswerReceived { .. } | DomainEvent::GroupCallSignalReceived { .. } => {
        }

        _ => {}
    }
}

/// `sender`'s hangup of the anon call `kind` in room `call_room_id`.
///
/// It calls off the ring `sender` started: the callee stops ringing and the
/// pending call is dropped. Beyond that, only a 1:1 call ends on the peer's
/// hangup ([`anon_hangup_ends_call`]). In a group the member only leaves — the
/// room drops them from the roster, and ends the call for us only once nobody
/// else is left in it ([`peer_left_ends_call`]).
fn handle_anon_hangup(
    worker: &CallWorkerHandle,
    signaling: &crate::services::call_signaling::CallSignalingService,
    kind: OrchestratedCallKind,
    call_room_id: &[u8],
    sender: UserId,
) {
    let Some(call) = anon_room_handle(call_room_id) else {
        tracing::debug!(
            len = call_room_id.len(),
            "Anon hangup with a malformed call_room_id; ignored"
        );
        return;
    };
    let rang_us = signaling.take_incoming_anon_call(call.as_str(), sender);
    if !anon_hangup_ends_call(kind) {
        // Only the member who rang us can call our ring off; another's hangup
        // (or our own, echoed back by the group mailbox) leaves it ringing.
        if rang_us {
            worker.orch_withdraw_incoming(call, CallEndReason::RemoteHangup);
        }
        return;
    }
    // Still ringing? The caller gave up or it rang out — stop ringing.
    // A no-op when this call was never presented or already accepted.
    worker.orch_withdraw_incoming(call.clone(), CallEndReason::RemoteHangup);
    // In this call? Disconnect from LiveKit + clear active_call +
    // orchestrator. A hangup for any other call leaves this one alone.
    worker.disconnect_local_nonblocking(Some(call), CallEndReason::RemoteHangup);
}

/// Whether a participant's hangup ends anon call `kind` for us: a 1:1 call
/// ends on the peer's hangup, a group call outlives any one member (Signal).
fn anon_hangup_ends_call(kind: OrchestratedCallKind) -> bool {
    kind == OrchestratedCallKind::AnonDm
}

/// The orchestrator handle of an anon call: its room name, `anon-<uuid>` of
/// the 16-byte `call_room_id` every anon offer, answer and hangup carries.
fn anon_room_handle(call_room_id: &[u8]) -> Option<CallHandle> {
    let id: [u8; 16] = call_room_id.try_into().ok()?;
    Some(CallHandle::from(format!(
        "anon-{}",
        uuid::Uuid::from_bytes(id)
    )))
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
        let room_handle = anon_room_handle(&call_room_id)
            .unwrap_or_else(|| CallHandle::from(format!("anon-{}", uuid::Uuid::nil())));

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
                let _ = worker
                    .call_events_tx
                    .send(CallOrchestratorEvent::AnonCallConnected {
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
            // Not a call of ours that is ringing: cancelled or rung out
            // before this answer landed (that end already closed it), or
            // another member's call in a group.
            Err(crate::services::call_signaling::CallSignalingError::NoPendingCall) => {
                tracing::debug!(call = %room_handle.as_str(), "Anon call answer for no pending call; ignored");
            }
            Err(e) => {
                tracing::error!(error = %e, "Anon call answer handling failed");
                worker.orch_mark_ended(room_handle, CallEndReason::Error(e.to_string()));
            }
        }
    });
}

#[cfg(test)]
mod ring_timeout_tests {
    use super::{RING_TIMEOUT, offer_is_stale};

    #[test]
    fn only_offers_past_the_ring_timeout_are_stale() {
        let now = 1_800_000_000_000_000_000u64;
        let timeout = RING_TIMEOUT.as_nanos() as u64;
        assert!(!offer_is_stale(now - 5_000_000_000, now));
        assert!(!offer_is_stale(now - timeout, now));
        assert!(offer_is_stale(now - timeout - 1, now));
        // The F56 case: an offer from 40 minutes ago, drained on app start.
        assert!(offer_is_stale(now - 40 * 60 * 1_000_000_000, now));
        // Sender clock ahead of ours, or no timestamp at all: ring.
        assert!(!offer_is_stale(now + 10_000_000_000, now));
        assert!(!offer_is_stale(0, now));
    }
}

#[cfg(test)]
mod joined_channel_tests {
    use super::{ChannelId, UserId, joined_channel_id};

    #[test]
    fn callee_files_a_channel_free_dm_call_under_the_caller() {
        let (caller, callee) = (UserId::new(), UserId::new());
        // The caller scoped it to us: file under our DM with the caller, not Saved Messages.
        assert_eq!(
            joined_channel_id(callee.into(), &caller, &callee),
            ChannelId::from(caller)
        );
        // A real channel id (group, or channel-backed DM) is kept.
        let channel = ChannelId::new();
        assert_eq!(joined_channel_id(channel, &caller, &callee), channel);
    }
}

#[cfg(test)]
mod orchestrator_kind_tests {
    use super::{
        CallHandle, OrchestratedActive, OrchestratedCallKind as K, OrchestratorState,
        PendingOutgoing, UserId,
    };

    /// `end_active_call` dispatches on this kind; `None` returns early with no
    /// hangup sent. A DM call cancelled while it rings must resolve to
    /// `DmStandard`, whose end path sends `CallHangup` and leaves.
    #[test]
    fn a_ringing_outgoing_call_resolves_to_its_kind() {
        let mut orch = OrchestratorState::default();
        assert_eq!(orch.call_kind(), None);

        let call = CallHandle::from("ringing");
        orch.outgoing.insert(
            call.clone(),
            PendingOutgoing {
                call: call.clone(),
                kind: K::DmStandard,
                peer_id: UserId::new(),
                peer_name: String::new(),
                is_video: false,
            },
        );
        assert_eq!(orch.call_kind(), Some(K::DmStandard));

        // Connected: the active slot is what counts.
        orch.outgoing.remove(&call);
        orch.active = Some(OrchestratedActive {
            call,
            kind: K::DmStandard,
        });
        assert_eq!(orch.call_kind(), Some(K::DmStandard));
    }
}

#[cfg(test)]
mod peer_left_tests {
    use super::{OrchestratedCallKind as K, peer_left_ends_call};

    #[test]
    fn one_to_one_ends_only_when_the_room_is_empty() {
        let gone = vec!["peer".to_string()];
        assert!(peer_left_ends_call(Some(K::DmStandard), &[], "peer"));
        assert!(peer_left_ends_call(Some(K::AnonDm), &gone, "peer"));
        assert!(!peer_left_ends_call(
            Some(K::DmStandard),
            &["other".into()],
            "peer"
        ));
        // Channel calls and no-call are never ended by a departure.
        assert!(!peer_left_ends_call(Some(K::GroupChannel), &[], "peer"));
        assert!(!peer_left_ends_call(None, &[], "peer"));
    }

    #[test]
    fn anon_group_ends_only_when_we_are_left_alone() {
        assert!(!peer_left_ends_call(
            Some(K::AnonGroup),
            &["other".into()],
            "peer"
        ));
        assert!(peer_left_ends_call(
            Some(K::AnonGroup),
            &["peer".into()],
            "peer"
        ));
        assert!(peer_left_ends_call(Some(K::AnonGroup), &[], "peer"));
    }

    #[test]
    fn group_hangup_does_not_end_the_call() {
        use super::anon_hangup_ends_call;
        assert!(!anon_hangup_ends_call(K::AnonGroup));
        assert!(anon_hangup_ends_call(K::AnonDm));
    }
}

#[cfg(test)]
mod ratchet_align_tests {
    use super::{KEY_RATCHET_INTERVAL_SECS, aligned_ratchet_schedule};

    #[test]
    fn two_peers_align_to_same_boundaries() {
        let iv = KEY_RATCHET_INTERVAL_SECS; // 30
        // Peer A connects ~at call start, peer B 4s later. Both still target
        // epoch 0 now and the SAME absolute first boundary (call_start + 30).
        let (ea, da) = aligned_ratchet_schedule(0.2, iv);
        let (eb, db) = aligned_ratchet_schedule(4.2, iv);
        assert_eq!(ea, 0);
        assert_eq!(eb, 0);
        // call_start + da (from A) == call_start + 4.0 + db (from B) == boundary 30.
        assert!((da - 29.8).abs() < 1e-6, "A delay {}", da);
        assert!((db - 25.8).abs() < 1e-6, "B delay {}", db);
        assert!(
            ((0.2 + da) - (4.2 + db)).abs() < 1e-6,
            "boundaries must match"
        );
    }

    #[test]
    fn late_joiner_fast_forwards_to_current_epoch() {
        let iv = KEY_RATCHET_INTERVAL_SECS;
        // Joined 95s in: epochs 0,1,2 already passed -> current epoch 3,
        // next boundary at 120s -> 25s away.
        let (epoch, delay) = aligned_ratchet_schedule(95.0, iv);
        assert_eq!(epoch, 3);
        assert!((delay - 25.0).abs() < 1e-6, "delay {}", delay);
    }

    #[test]
    fn negative_elapsed_clamps_to_zero() {
        // Clock skew: local clock behind server anchor.
        let (epoch, delay) = aligned_ratchet_schedule(-3.0, KEY_RATCHET_INTERVAL_SECS);
        assert_eq!(epoch, 0);
        assert!((delay - 30.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod failed_accept_tests {
    use super::{
        CallEndReason, CallHandle, CallOrchestratorEvent, CallWorkerHandle, OrchestratedCallKind,
        UserId,
    };

    /// An accept takes the call out of the ringing set before it connects, so
    /// a failed connect leaves it in no slot. `accept_presented_incoming_call`
    /// ends it with `orch_mark_ended(Error)`, which must still reach the
    /// history recorder (else the answered call stays "missed") and the UI
    /// (else the answering view never comes down).
    #[tokio::test]
    async fn ending_an_accepted_call_that_never_connected_emits_active_ended() {
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
        let worker = CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
            channel.into(),
            crate::network::grpc::interceptors::AuthInterceptor::new(),
        ));
        let mut events = worker.subscribe_call_events();
        let call = CallHandle::from("accepted-then-failed");

        worker.orch_present_incoming(
            call.clone(),
            OrchestratedCallKind::DmStandard,
            UserId::new(),
            "Peer".into(),
            false,
            true,
            String::new(),
            Some([0; 32]),
            None,
            String::new(),
        );
        worker
            .orch_accept_incoming(call.clone())
            .await
            .expect("presented call accepts");
        worker.orch_mark_ended(
            call.clone(),
            CallEndReason::Error("connect timed out".into()),
        );

        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
                Ok(Ok(CallOrchestratorEvent::ActiveEnded {
                    call: ended,
                    reason,
                })) => {
                    assert_eq!(ended, call);
                    assert!(matches!(reason, CallEndReason::Error(_)));
                    break;
                }
                Ok(Ok(CallOrchestratorEvent::IncomingPresented { .. })) => continue,
                other => panic!("expected ActiveEnded, got {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod anon_cancel_tests {
    use super::*;

    /// §124 1f: cancelling our anon call while it still rings reaches the
    /// cancel branch. `end_active_call` routes on `orch_active_kind` — the
    /// ringing outgoing call must resolve to its anon kind — and
    /// `end_anon_call` takes the ringing branch (which sends the anon hangup,
    /// `call_signaling.rs` `finish_anon_call`) only when we are in no room.
    /// The hangup's delivery to the callee is not covered here.
    #[tokio::test]
    async fn a_ringing_anon_call_cancels_through_the_hangup_path() {
        for kind in [
            OrchestratedCallKind::AnonDm,
            OrchestratedCallKind::AnonGroup,
        ] {
            let channel =
                tonic::transport::Endpoint::from_static("http://127.0.0.1:9").connect_lazy();
            let worker = CallWorkerHandle::spawn(crate::network::grpc::CallClient::new(
                channel.into(),
                crate::network::grpc::interceptors::AuthInterceptor::new(),
            ));
            let room = format!("anon-{}", uuid::Uuid::new_v4());
            worker.orch_register_outgoing(
                CallHandle::from(room.as_str()),
                kind,
                UserId(uuid::Uuid::nil()),
                String::new(),
                false,
            );
            assert_eq!(
                worker.orch_active_kind().await,
                Some(kind),
                "a ringing {kind:?} call is not what End ends"
            );
            assert_eq!(
                worker.get_room_name().await,
                None,
                "ringing, yet in a room: the cancel would skip the hangup"
            );
            worker.shutdown();
        }
    }
}

#[cfg(test)]
mod publish_gate_tests {
    use super::{DeferredMedia, PublishGate};

    /// §124 1k item 2: a group joiner publishes nothing until it holds the
    /// call's key; what it asked for meanwhile is published then, once.
    #[test]
    fn a_joiner_publishes_only_once_keyed() {
        let mut gate = PublishGate::awaiting_key(false);
        assert!(gate.is_closed());
        assert!(
            gate.defer_mic(Some("usb".into())),
            "mic held back while keyless"
        );
        assert!(gate.defer_camera(), "camera held back while keyless");
        assert_eq!(
            gate.open(),
            Some(DeferredMedia {
                mic: Some(Some("usb".into())),
                camera: true
            })
        );
        assert!(!gate.is_closed());
        assert_eq!(gate.open(), None, "published once");
        assert!(!gate.defer_mic(None), "open: publishes go straight out");
        assert!(!gate.defer_camera());
    }

    #[test]
    fn a_video_joiner_holds_its_camera_and_stopping_drops_it() {
        let mut gate = PublishGate::awaiting_key(true);
        gate.drop_camera();
        gate.drop_mic();
        assert_eq!(
            gate.open(),
            Some(DeferredMedia {
                mic: None,
                camera: false
            })
        );
    }

    #[test]
    fn a_keyed_participant_is_never_held() {
        let mut gate = PublishGate::default();
        assert!(!gate.is_closed());
        assert!(!gate.defer_mic(None));
        assert_eq!(gate.open(), None);
    }
}

#[cfg(test)]
mod group_key_install_tests {
    use super::{
        ActiveCallState, Call, CallError, CallId, CallKeyState, CallType, ChannelId,
        MAX_RATCHET_CATCH_UP, PublishGate, UserId, adopt_group_key, adopt_into_call, chain_install,
        ratchet_to,
    };

    const CALL: &str = "11111111-2222-4333-8444-555555555555";

    fn active(root: [u8; 32]) -> ActiveCallState {
        let mut call = Call::new(ChannelId::new(), UserId::new(), CallType::Voice);
        call.id = CALL.parse().unwrap();
        ActiveCallState {
            call,
            key_state: CallKeyState::new(&root, CALL).unwrap(),
            room_name: String::new(),
            livekit_url: String::new(),
            publish_gate: PublishGate::default(),
        }
    }

    /// The rotator rotates (to its epoch + 1) and sends that `(key, epoch)`;
    /// a receiver whose chain has ratcheted differently
    /// installs it. Both must hand LiveKit the same index and bytes, and keep
    /// agreeing on the next 30 s ratchet.
    #[test]
    fn received_key_installs_the_rotators_bytes() {
        let root = [7u8; 32];
        let new_key = [9u8; 32];
        let mut rotator = CallKeyState::new(&root, CALL).unwrap();
        rotator.ratchet().unwrap();
        rotator.ratchet().unwrap();
        let epoch = rotator.rotate(new_key).unwrap();
        let sent = chain_install(&rotator);

        let mut receiver = CallKeyState::new(&root, CALL).unwrap();
        let installed = adopt_group_key(&mut receiver, new_key, epoch).unwrap();
        assert_eq!(sent, installed);
        assert_eq!(sent.0, 3);
        assert_eq!(rotator.current_key(), receiver.current_key());

        rotator.ratchet().unwrap();
        receiver.ratchet().unwrap();
        assert_eq!(rotator.current_key(), receiver.current_key());
        assert_eq!(rotator.epoch(), receiver.epoch());
    }

    /// Rotations put the chain ahead of the clock only until the clock
    /// catches up, so however many a call has had, a joiner (on the
    /// wall-clock epoch) can adopt the join rotation's key and then ratchets
    /// in step.
    #[test]
    fn rotations_do_not_drift_the_chain_from_the_clock() {
        let root = [7u8; 32];
        let mut rotator = CallKeyState::new(&root, CALL).unwrap();
        ratchet_to(&mut rotator, 10).unwrap();
        for _ in 0..3 {
            rotator
                .rotate(crate::crypto::generate_group_call_key())
                .unwrap();
        }
        assert_eq!(rotator.epoch(), 13);
        assert!(
            ratchet_to(&mut rotator, 11).is_none(),
            "ahead of the clock: wait for it"
        );
        assert!(ratchet_to(&mut rotator, 12).is_none());

        let mut joiner = CallKeyState::new(&root, CALL).unwrap();
        ratchet_to(&mut joiner, 12).unwrap();
        let join_key = crate::crypto::generate_group_call_key();
        let issued = rotator.rotate(join_key).unwrap();
        assert!(adopt_group_key(&mut joiner, join_key, issued).is_some());

        for wall in 13..20 {
            ratchet_to(&mut rotator, wall);
            ratchet_to(&mut joiner, wall);
            assert_eq!(rotator.epoch(), wall.max(14));
            assert_eq!(
                joiner.current_key(),
                rotator.current_key(),
                "wall epoch {wall}"
            );
        }
    }

    /// Security review F1: a member who left and rejoins is sent only the
    /// join rotation's fresh key, from which no key used while they were away
    /// follows — nor does one from the chain they kept.
    #[test]
    fn rejoiner_cannot_derive_the_key_used_while_away() {
        use crate::crypto::generate_group_call_key as fresh;
        let root = [7u8; 32];
        let mut rotator = CallKeyState::new(&root, CALL).unwrap();
        let mut member = CallKeyState::new(&root, CALL).unwrap();
        ratchet_to(&mut rotator, 4).unwrap();
        ratchet_to(&mut member, 4).unwrap();
        let key = fresh();
        let at = rotator.rotate(key).unwrap();
        assert!(
            adopt_group_key(&mut member, key, at).is_some(),
            "the member's own join rotation"
        );

        // They leave: the leave rotation keys the call while they are away.
        rotator.rotate(fresh()).unwrap();
        let mut while_away = Vec::new();
        for wall in 6..12 {
            ratchet_to(&mut rotator, wall);
            while_away.push(*rotator.current_key());
            ratchet_to(&mut member, wall);
            assert!(
                !while_away.contains(member.current_key()),
                "their old chain, wall epoch {wall}"
            );
        }

        // They rejoin: the join rotation's key is all they are sent.
        let rejoin_key = fresh();
        let at = rotator.rotate(rejoin_key).unwrap();
        assert!(adopt_group_key(&mut member, rejoin_key, at).is_some());
        assert_eq!(member.current_key(), rotator.current_key());
        for wall in 12..40 {
            ratchet_to(&mut rotator, wall);
            ratchet_to(&mut member, wall);
            assert_eq!(member.current_key(), rotator.current_key());
            assert!(
                !while_away.contains(member.current_key()),
                "after rejoining, wall epoch {wall}"
            );
        }
    }

    /// A joiner is sent the join rotation's key — fresh, so nothing the call
    /// was keyed with before the join follows from it.
    #[test]
    fn joiner_is_sent_no_key_from_before_its_join() {
        use crate::crypto::generate_group_call_key as fresh;
        let mut rotator = CallKeyState::new(&[7u8; 32], CALL).unwrap();
        rotator.rotate(fresh()).unwrap();
        let mut before_join = Vec::new();
        for wall in 2..10 {
            ratchet_to(&mut rotator, wall);
            before_join.push(*rotator.current_key());
        }
        before_join.push(rotator.peek_next_key().unwrap());

        let join_key = fresh();
        let at = rotator.rotate(join_key).unwrap();
        let (_, sent, _) = chain_install(&rotator);
        assert_eq!(
            sent,
            join_key.to_vec(),
            "what goes to the joiner is the rotation's key"
        );
        assert!(!before_join.contains(&join_key));

        let mut joiner = CallKeyState::new(&[7u8; 32], CALL).unwrap();
        ratchet_to(&mut joiner, 9).unwrap();
        assert!(adopt_group_key(&mut joiner, join_key, at).is_some());
        for wall in 10..40 {
            ratchet_to(&mut joiner, wall);
            assert!(
                !before_join.contains(joiner.current_key()),
                "wall epoch {wall}"
            );
        }
    }

    #[test]
    fn ratchet_catch_up_is_bounded() {
        let mut state = CallKeyState::new(&[7u8; 32], CALL).unwrap();
        assert!(ratchet_to(&mut state, MAX_RATCHET_CATCH_UP + 1).is_none());
        assert!(ratchet_to(&mut state, u32::MAX).is_none());
        assert_eq!(state.epoch(), 0);
    }

    #[test]
    fn stale_epoch_changes_nothing() {
        let mut state = active([7u8; 32]);
        let call: CallId = CALL.parse().unwrap();
        assert!(
            adopt_into_call(Some(&mut state), call, [9u8; 32], 2)
                .unwrap()
                .is_some()
        );
        let held = *state.key_state.current_key();
        for stale in [0, 1, 2] {
            assert_eq!(
                adopt_into_call(Some(&mut state), call, [5u8; 32], stale).unwrap(),
                None
            );
        }
        assert_eq!(*state.key_state.current_key(), held);
        assert_eq!(state.key_state.epoch(), 2);
    }

    #[test]
    fn key_for_another_call_is_refused() {
        let mut state = active([7u8; 32]);
        let other = CallId(uuid::Uuid::new_v4());
        let before = *state.key_state.current_key();
        assert!(matches!(
            adopt_into_call(Some(&mut state), other, [9u8; 32], 5),
            Err(CallError::NotInCall)
        ));
        assert!(matches!(
            adopt_into_call(None, other, [9u8; 32], 5),
            Err(CallError::NotInCall)
        ));
        assert_eq!(*state.key_state.current_key(), before);
        assert_eq!(state.key_state.epoch(), 0);
    }
}
