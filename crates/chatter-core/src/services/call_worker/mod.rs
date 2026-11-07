//! Call worker - dedicated thread for non-Send LiveKit operations.
//!
//! LiveKit types don't implement Send/Sync, so all call operations must run
//! on a single dedicated thread. This module provides a Send-safe handle that
//! communicates with the worker via channels.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
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
use crate::domain::call::{Call, CallType, Participant};
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
/// calls across handler boundaries: the call id.
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
    /// Regular group channel call (sender-key distributed key).
    GroupChannel,
}

pub(crate) enum SignalHangupDisposition {
    Reject,
    Forward,
    GroupRingWithdrawn(u64),
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

/// `CallEndReason::Error` text for a session the SDK could not recover and
/// one fresh-token reconnect did not bring back. The UI keys its notice on it.
pub const CONNECTION_LOST: &str = "Connection lost";

/// Events emitted by the orchestrator. Consumers (UI, notification service,
/// call history) subscribe via [`CallWorkerHandle::take_call_events`].
#[derive(Debug, Clone, PartialEq)]
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
        /// key elsewhere (group channel calls).
        ephemeral_public_key: Option<[u8; 32]>,
        /// Caller-reported channel context (DM channel id, group id) —
        /// opaque to the orchestrator, used by the UI and
        /// the signalling services for accept/decline.
        channel_context: Option<String>,
        /// The conversation this call arrived in, as `ConversationType`
        /// dictates for `kind` (peer `UserId` for a DM, `ChannelId` for a
        /// group channel). Call history needs it
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

    /// The camera could not produce a frame; audio continues.
    CameraUnavailable { call: CallHandle },

    /// The active call has ended.
    ActiveEnded {
        call: CallHandle,
        reason: CallEndReason,
    },
}

/// Where orchestrator events go: a bounded broadcast for the UI-facing
/// subscribers, which tolerate a lag, plus lossless taps for a consumer that
/// cannot — the call-history recorder, for which a lost `ActiveConnected`
/// files a talked call as "No answer". Events are rare (a handful per call),
/// so an unbounded tap stays small.
#[derive(Clone)]
pub(crate) struct CallEventsTx {
    broadcast: broadcast::Sender<CallOrchestratorEvent>,
    lossless: Arc<parking_lot::Mutex<Vec<mpsc::UnboundedSender<CallOrchestratorEvent>>>>,
}

impl CallEventsTx {
    fn new(capacity: usize) -> Self {
        Self {
            broadcast: broadcast::channel(capacity).0,
            lossless: Arc::default(),
        }
    }

    /// Deliver to every lossless tap, then broadcast. Mirrors
    /// `broadcast::Sender::send`: an error only means no broadcast receiver.
    pub(crate) fn send(
        &self,
        event: CallOrchestratorEvent,
    ) -> Result<usize, broadcast::error::SendError<CallOrchestratorEvent>> {
        self.lossless
            .lock()
            .retain(|tap| tap.send(event.clone()).is_ok());
        self.broadcast.send(event)
    }

    fn subscribe(&self) -> broadcast::Receiver<CallOrchestratorEvent> {
        self.broadcast.subscribe()
    }

    fn subscribe_lossless(&self) -> mpsc::UnboundedReceiver<CallOrchestratorEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lossless.lock().push(tx);
        rx
    }
}

/// Bookkeeping for a presented incoming call waiting for the user to decide.
#[derive(Debug, Clone)]
struct PendingIncoming {
    deadline: tokio::time::Instant,
    session_revision: Option<u64>,
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
    session_revision: Option<u64>,
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
    peer_id: Option<UserId>,
    session_revision: Option<u64>,
    call: CallHandle,
    kind: OrchestratedCallKind,
}

/// Mutable orchestration state owned by the worker task.
#[derive(Default)]
struct OrchestratorState {
    active: Option<OrchestratedActive>,
    incoming: HashMap<CallHandle, PendingIncoming>,
    outgoing: HashMap<CallHandle, PendingOutgoing>,
    generation: u64,
    incoming_order: HashMap<CallHandle, u64>,
}

impl OrchestratorState {
    fn withdraw_expired(&mut self, now: tokio::time::Instant, presentation: &IncomingCallPresentationState, events: &CallEventsTx) {
        for pending in self.expire_incoming(now) {
            if presentation.accepts_session(pending.session_revision) {
                let _ = events.send(CallOrchestratorEvent::IncomingWithdrawn {
                    call: pending.call, reason: CallEndReason::NoAnswer,
                });
            }
        }
        self.incoming_order.retain(|call, _| self.incoming.contains_key(call));
        presentation.publish(self);
    }

    fn expire_incoming(&mut self, now: tokio::time::Instant) -> Vec<PendingIncoming> {
        let expired: Vec<_> = self.incoming.iter().filter(|(_, p)| p.deadline <= now)
            .map(|(call, _)| call.clone()).collect();
        expired.into_iter().filter_map(|call| self.incoming.remove(&call)).collect()
    }
    /// The connected call's kind, else a dialed call's that is still ringing.
    fn call_kind(&self) -> Option<OrchestratedCallKind> {
        self.active
            .as_ref()
            .map(|a| a.kind)
            .or_else(|| self.outgoing.values().next().map(|o| o.kind))
    }
}

/// Authoritative presentation state, read on the worker command queue.
/// No signaling keys leave the actor. Events are ordered oldest to newest.
#[derive(Debug, Clone)]
pub struct IncomingCallPresentationSnapshot {
    pub session_revision: Option<u64>,
    pub generation: u64,
    pub presentations: Vec<CallOrchestratorEvent>,
}

/// Shared presentation authority. Readers hold a short synchronous lease while
/// applying a projection; actor acceptance cannot overtake that projection.
#[derive(Clone)]
pub struct IncomingCallPresentationState {
    snapshot: Arc<RwLock<IncomingCallPresentationSnapshot>>,
    changes: tokio::sync::watch::Sender<u64>,
    requires_session: Arc<std::sync::atomic::AtomicBool>,
    view_session: Option<u64>,
    active_identity: Arc<RwLock<Option<CallHandle>>>,
}

impl IncomingCallPresentationState {
    fn new() -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(IncomingCallPresentationSnapshot {
                session_revision: None,
                generation: 0,
                presentations: Vec::new(),
            })),
            changes: tokio::sync::watch::channel(0).0,
            requires_session: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            view_session: None,
            active_identity: Arc::new(RwLock::new(None)),
        }
    }

    /// The callback must be short and must not call back into the worker.
    /// The lease orders native/UI projection before the next actor publication.
    pub fn with_snapshot<R>(
        &self,
        apply: impl FnOnce(&IncomingCallPresentationSnapshot) -> R,
    ) -> R {
        let current = self.snapshot.read();
        if self.view_session.is_some() && self.view_session != current.session_revision {
            return apply(&IncomingCallPresentationSnapshot {
                session_revision: self.view_session,
                generation: current.generation,
                presentations: Vec::new(),
            });
        }
        apply(&current)
    }

    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// A retired bridge can cancel its own posts, but can never post for the
    /// next owner even while its graceful drain is still completing.
    pub fn for_session(&self, revision: u64) -> Self {
        let mut state = self.clone();
        state.view_session = Some(revision);
        state
    }

    pub(crate) fn capture_session(mut self) -> Self {
        if self.view_session.is_none() {
            self.view_session = self.session_revision();
        }
        self
    }

    pub fn begin_session(&self, revision: u64) {
        self.requires_session
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut current = self.snapshot.write();
        if current.session_revision == Some(revision) {
            return;
        }
        current.session_revision = Some(revision);
        current.presentations.clear();
        *self.active_identity.write() = None;
        current.generation += 1;
        self.changes.send_replace(current.generation);
    }

    fn end_session(&self, revision: u64) {
        let mut current = self.snapshot.write();
        if current.session_revision != Some(revision) {
            return;
        }
        current.session_revision = None;
        current.presentations.clear();
        *self.active_identity.write() = None;
        current.generation += 1;
        self.changes.send_replace(current.generation);
    }

    pub fn session_revision(&self) -> Option<u64> {
        self.snapshot.read().session_revision
    }

    fn accepts_session(&self, revision: Option<u64>) -> bool {
        let current = self.session_revision();
        revision == current
            && (revision.is_some()
                || !self
                    .requires_session
                    .load(std::sync::atomic::Ordering::SeqCst))
    }

    fn publish(&self, orchestrator: &OrchestratorState) {
        let mut current = self.snapshot.write();
        *self.active_identity.write() = if current.session_revision.is_some()
            || !self
                .requires_session
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            orchestrator
                .active
                .as_ref()
                .filter(|a| a.session_revision == current.session_revision)
                .map(|a| a.call.clone())
        } else {
            None
        };
        let mut snapshot = orchestrator.incoming_snapshot_for(current.session_revision);
        snapshot.generation = current.generation + 1;
        let generation = snapshot.generation;
        if current.presentations == snapshot.presentations {
            return;
        }
        *current = snapshot;
        self.changes.send_replace(generation);
    }
}

impl OrchestratorState {
    fn incoming_snapshot_for(
        &self,
        session_revision: Option<u64>,
    ) -> IncomingCallPresentationSnapshot {
        let mut incoming: Vec<_> = self
            .incoming
            .values()
            .filter(|p| p.session_revision == session_revision)
            .collect();
        incoming.sort_by_key(|p| self.incoming_order.get(&p.call).copied().unwrap_or(0));
        IncomingCallPresentationSnapshot {
            session_revision,
            generation: self.generation,
            presentations: incoming
                .into_iter()
                .map(|p| CallOrchestratorEvent::IncomingPresented {
                    call: p.call.clone(),
                    kind: p.kind,
                    peer_id: p.peer_id,
                    peer_name: p.peer_name.clone(),
                    is_video: p.is_video,
                    is_identity_verified: p.is_identity_verified,
                    identity_warning: p.identity_warning.clone(),
                    ephemeral_public_key: None,
                    channel_context: p.channel_context.clone(),
                    conversation_id: p.conversation_id.clone(),
                    replaces_active: self.active.is_some(),
                })
                .collect(),
        }
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

/// Public comparison state; media key material never leaves the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEncryptionVerification {
    ExistingHandshake,
    WaitingForKey,
    MediaKey { epoch: u32, emojis: [String; 4] },
}

/// Call identity and its verification state captured in one worker command.
#[derive(Debug, Clone)]
pub struct CallVerificationSnapshot {
    pub call: Call,
    pub verification: CallEncryptionVerification,
}

/// Commands sent to the call worker thread.
enum CallCommand {
    /// Initiate a new call.
    InitiateCall {
        attempt: Option<crate::client::calls::CallStartAttempt>,
        channel_id: ChannelId,
        title: String,
        call_type: CallType,
        encryption_root_key: [u8; 32],
        initiator_id: UserId,
        response: oneshot::Sender<Result<Call, CallError>>,
    },
    /// Join an existing call.
    JoinCall {
        session_revision: Option<u64>,
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
        session_revision: Option<u64>,
        reason: Option<String>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Disconnect locally without server notification. `reason` is what the
    /// resulting `ActiveEnded` carries. With `call`, only if that is the
    /// active call — a hangup names its call, and one for another call must
    /// not end this one.
    DisconnectLocal {
        notify_server: bool,
        session_revision: Option<u64>,
        call: Option<CallHandle>,
        reason: CallEndReason,
        response: oneshot::Sender<()>,
    },
    BeginPeerLeftCheck {
        generation: u64,
    },
    FinishPeerLeftCheck {
        call: CallHandle,
        generation: u64,
        membership_revision: u64,
        session_revision: Option<u64>,
    },
    /// Swap the worker's gRPC call client (anon-mode transport switch).
    ///
    /// The command loop is strictly sequential and fully awaits each call
    /// operation before dequeuing the next, so this is only ever processed
    /// BETWEEN complete operations — never mid-RPC — making the swap safe.
    SwapCallClient {
        client: CallClient,
    },
    /// Authenticate a Signal hangup before any domain/UI teardown.
    AuthenticateDmHangup {
        session_revision: Option<u64>,
        call: CallHandle,
        peer: UserId,
        own_elsewhere: bool,
        response: oneshot::Sender<SignalHangupDisposition>,
    },
    /// Get active call info.
    GetActiveCall {
        response: oneshot::Sender<Option<Call>>,
    },
    GetVerificationSnapshot {
        response: oneshot::Sender<Option<CallVerificationSnapshot>>,
    },
    /// Check if in a call.
    IsInCall {
        response: oneshot::Sender<bool>,
    },
    GetRoomConnection {
        response: oneshot::Sender<(u64, RoomState)>,
    },
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
        answer_scope: Option<DmAnswerScope>,
        device_id: Option<String>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Publish camera.
    PublishCamera {
        answer_scope: Option<DmAnswerScope>,
        device_path: Option<String>,
        config: Option<VideoCaptureConfig>,
        response: oneshot::Sender<Result<(), CallError>>,
    },
    /// Flip the active camera between front and back lens (mobile).
    SwapCamera {
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
    /// Available devices and acknowledged/default selection for the picker.
    #[cfg(not(target_os = "android"))]
    AudioDeviceSnapshot {
        output: bool,
        response: oneshot::Sender<Result<(Vec<crate::calls::AudioDevice>, String), String>>,
    },
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
    #[cfg(target_os = "android")]
    PublishAndroidScreenShare { response: oneshot::Sender<Result<(), CallError>> },
    /// Stop screen share.
    #[cfg(not(target_os = "ios"))]
    StopScreenShare,
    /// Check if screen sharing.
    IsScreenSharing { response: oneshot::Sender<bool> },
    /// Ratchet the encryption key for forward secrecy.
    RatchetKey {
        response: oneshot::Sender<Option<u32>>,
    },
    /// Set encryption key externally (from CallKeyState ratchet).
    SetEncryptionKey {
        answer_scope: Option<DmAnswerScope>,
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
    /// Internal: the room-events forwarder saw the SDK give up on a session.
    /// `call` and `generation` were captured when the loss was observed; the
    /// handler acts only if both still name the live call and connection.
    RoomLost {
        call: CallHandle,
        generation: u64,
        retryable: bool,
        reason: String,
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

    // ========================================================================
    // Orchestrator commands
    // ========================================================================
    /// Record a newly received incoming call offer. The orchestrator decides
    /// whether to present it fresh or flag it as replacing the active call.
    OrchPresentIncoming { incoming: PendingIncoming },
    /// Withdraw a previously presented incoming call (caller hung up / timed
    /// out / remote-declined before we acted).
    OrchWithdrawIncoming {
        session_revision: Option<u64>,
        call: CallHandle,
        reason: CallEndReason,
        response: Option<oneshot::Sender<()>>,
    },
    /// Register an outgoing call that was just dialed.
    OrchRegisterOutgoing {
        outgoing: PendingOutgoing,
        session_revision: Option<u64>,
    },
    /// Notify the orchestrator that a pending (incoming or outgoing) call
    /// has connected and should be installed as the active call.
    OrchMarkConnected {
        answer_scope: Option<DmAnswerScope>,
        require_media_match: bool,
        session_revision: Option<u64>,
        response: Option<oneshot::Sender<Result<(), CallError>>>,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        is_video: bool,
    },
    /// Notify the orchestrator that the active call has been torn down
    /// locally or remotely. Clears the slot and emits `ActiveEnded`.
    OrchMarkEnded {
        session_revision: Option<u64>,
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
    /// Seed media-independent state for actor ownership regressions; never
    /// creates a room, publishes tracks, reads credentials or touches storage.
    #[cfg(test)]
    TestAdvanceIncomingClock {
        elapsed: std::time::Duration,
        response: oneshot::Sender<()>,
    },
    #[cfg(test)]
    TestExpireIncoming {
        elapsed: std::time::Duration,
        response: oneshot::Sender<()>,
    },
    #[cfg(test)]
    TestPrimeCall {
        call: Call,
        response: oneshot::Sender<()>,
    },
    #[cfg(test)]
    TestPause {
        entered: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    TestDmAnswerBackend {
        publish: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
        response: oneshot::Sender<()>,
    },
    #[cfg(test)]
    TestDmAnswerEffects {
        response: oneshot::Sender<(usize, usize)>,
    },
    /// Actor acceptance and signaling ownership transfer share the invite lock.
    OrchAcceptPresented {
        call: CallHandle,
        replaces_active: bool,
        invites: Arc<super::call_signaling::GroupCallInvites>,
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

    ResetPresentationSession {
        revision: u64,
        response: oneshot::Sender<Vec<String>>,
    },
    /// Serialize presentation recovery with acceptance and withdrawal.
    OrchIncomingSnapshot {
        response: oneshot::Sender<IncomingCallPresentationSnapshot>,
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
    call_events_tx: CallEventsTx,
    incoming_presentation: IncomingCallPresentationState,
    presentation_session: Option<u64>,
    answer_scope: Option<DmAnswerScope>,
}

/// Retained answer ownership travels with each queued mutation.
#[derive(Clone)]
struct DmAnswerScope {
    call: CallHandle,
    session_revision: Option<u64>,
    attempt: Option<crate::client::calls::CallStartAttempt>,
}

impl DmAnswerScope {
    fn accepts(
        &self,
        presentation: &IncomingCallPresentationState,
        active: &Rc<RefCell<Option<ActiveCallState>>>,
    ) -> bool {
        presentation.accepts_session(self.session_revision)
            && !self.attempt.as_ref().is_some_and(|a| a.is_cancelled())
            && active.borrow().as_ref().map(derive_call_handle).as_ref() == Some(&self.call)
    }
}


mod handle;
mod actor;
mod state;
mod lifecycle;
mod ratchet;
mod bridge;

use actor::*;
pub use state::{RoomSeniority, GroupKeyStatus};
use state::*;
use lifecycle::*;
pub(crate) use ratchet::{reconnect_wall_epoch, install_reconnect_catch_up};
pub use bridge::RING_TIMEOUT;
pub(crate) use bridge::{incoming_ring_budget, offer_is_stale_now};
use bridge::{peer_left_ends_call, PEER_LEFT_GRACE};
#[cfg(test)]
use bridge::offer_is_stale;
use ratchet::*;
pub use bridge::spawn_call_orchestrator_bridge;

#[cfg(test)]
mod media_key_tests;
#[cfg(test)]
mod reconnect_verification_tests;
#[cfg(test)]
mod orchestrator_tests;
