//! Call domain entity.
//!
//! Represents voice/video calls, participants, and verification badges.

use crate::domain::ids::{CallId, ChannelId, UserId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Call Type
// ============================================================================

/// Type of call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallType {
    /// Voice-only call.
    Voice,
    /// Video call.
    Video,
    /// Group call (multi-participant).
    Group,
}

// ============================================================================
// Call End Reason
// ============================================================================

/// Reason why a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallEndReason {
    /// Call completed normally (hung up by participant).
    Completed,
    /// Call was declined by recipient.
    Declined,
    /// Call was not answered (timed out).
    Missed,
    /// Recipient was busy on another call.
    Busy,
    /// Call failed to connect.
    Failed,
    /// Call ended due to network error.
    NetworkError,
}

// ============================================================================
// Call State
// ============================================================================

/// Call state machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallState {
    /// Call is being initiated (setting up).
    Initiating,
    /// Call is ringing on recipient's device.
    Ringing,
    /// Connection is being established.
    Connecting,
    /// Call is active and connected.
    Connected,
    /// Temporarily lost connection, attempting to reconnect.
    Reconnecting,
    /// Call has ended.
    Ended {
        /// The reason the call ended.
        reason: CallEndReason,
    },
}

impl CallState {

}

// ============================================================================
// Participant
// ============================================================================

/// Call participant information.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    /// The user in the call.
    pub user_id: UserId,
    /// When the user joined the call.
    pub joined_at: DateTime<Utc>,
    /// Whether the participant's microphone is muted.
    pub is_muted: bool,
    /// Whether the participant's video is enabled.
    pub is_video_enabled: bool,
    /// Whether the participant is sharing their screen.
    pub is_screen_sharing: bool,
}

impl Participant {
    /// Create a new participant with default settings.
    ///
    /// By default, participants are not muted, have video disabled, and are not screen sharing.
    #[must_use]
    pub fn new(user_id: UserId) -> Self {
        Self {
            user_id,
            joined_at: Utc::now(),
            is_muted: false,
            is_video_enabled: false,
            is_screen_sharing: false,
        }
    }

}

// ============================================================================
// Call
// ============================================================================

/// Represents a voice or video call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// Unique call identifier.
    pub id: CallId,
    /// Channel where the call is taking place.
    pub channel_id: ChannelId,
    /// User who initiated the call.
    pub initiator_id: UserId,
    /// Type of call (voice, video, group).
    pub call_type: CallType,
    /// Current state of the call.
    pub state: CallState,
    /// Participants in the call.
    pub participants: Vec<Participant>,
    /// When the call was initiated (ringing started).
    pub started_at: Option<DateTime<Utc>>,
    /// When the call was connected (answered).
    pub connected_at: Option<DateTime<Utc>>,
    /// When the call ended.
    pub ended_at: Option<DateTime<Utc>>,
    /// Whether the call is end-to-end encrypted.
    pub is_encrypted: bool,
}

impl Call {
    /// Create a new call in the Initiating state.
    #[must_use]
    pub fn new(channel_id: ChannelId, initiator_id: UserId, call_type: CallType) -> Self {
        Self {
            id: CallId::new(),
            channel_id,
            initiator_id,
            call_type,
            state: CallState::Initiating,
            participants: Vec::new(),
            started_at: Some(Utc::now()),
            connected_at: None,
            ended_at: None,
            is_encrypted: true,
        }
    }

    /// Transition to the Ringing state.
    pub fn ring(&mut self) {
        self.state = CallState::Ringing;
    }

    /// Transition to Connected, retaining the first connection timestamp.
    pub fn start(&mut self) {
        self.state = CallState::Connected;
        self.connected_at.get_or_insert_with(Utc::now);
    }

    /// Get the duration of the call.
    ///
    /// Returns the time between `connected_at` and `ended_at` if the call has ended,
    /// or between `connected_at` and now if the call is still active.
    /// Returns `None` if the call was never connected.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        let connected = self.connected_at?;
        let end_time = self.ended_at.unwrap_or_else(Utc::now);
        Some(end_time - connected)
    }

    /// Connected duration for presentation and hangup metadata, in whole seconds.
    /// Ringing time is excluded; a backwards wall-clock adjustment cannot make
    /// the displayed duration negative.
    #[must_use]
    pub fn duration_seconds(&self) -> i64 {
        self.duration().map(|duration| duration.num_seconds().max(0)).unwrap_or(0)
    }

    /// Add a participant to the call.
    pub fn add_participant(&mut self, participant: Participant) {
        // Don't add duplicates
        if !self.participants.iter().any(|p| p.user_id == participant.user_id) {
            self.participants.push(participant);
        }
    }

}

// ============================================================================
// Verification Badge
// ============================================================================

/// E2EE verification badge for calls.
///
/// Displays safety numbers for verifying the identity of call participants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerificationBadge {
    /// The safety number for verification (typically truncated for display).
    pub safety_number: String,
    /// Whether the safety number has been verified by the user.
    pub is_verified: bool,
    /// When the safety number was last verified.
    pub last_verified_at: Option<DateTime<Utc>>,
}

impl VerificationBadge {

}


// ============================================================================
// DM Call Orchestration Types
// ============================================================================

use crate::crypto::call_key_provider::EphemeralCallKey;
use zeroize::Zeroizing;

/// Result of completing a DM call key exchange (caller or callee side).
///
/// Contains everything the UI needs to display the active call.
#[derive(Debug)]
pub struct CallSetupResult {
    /// The call ID.
    pub call_id: CallId,
    /// The channel ID.
    pub channel_id: ChannelId,
    /// Verification emojis for secure call verification (4 emojis).
    pub verification_emojis: [String; 4],
    /// Whether this is a video call.
    pub is_video: bool,
    /// Local offer/answer completion time (Unix nanos).
    /// Live duration uses the active Call's connected_at instead.
    pub started_at_ns: u64,
}

/// Result of completing an anonymous call setup (either side).
///
/// Contains everything the UI needs to display the active anonymous call.
/// `encryption_key` is wrapped in `Zeroizing` so the E2EE key is wiped
/// from memory when this struct is dropped.
pub struct AnonCallSetupResult {
    /// The room name (used as call_id for anonymous calls).
    pub room_name: String,
    /// The channel ID (synthetic for anon calls).
    pub channel_id: ChannelId,
    /// E2EE encryption key for LiveKit (32 bytes). Zeroized on drop.
    pub encryption_key: Zeroizing<Vec<u8>>,
    /// Verification emojis for secure call verification (4 emojis).
    pub verification_emojis: [String; 4],
    /// Whether this is a video call.
    pub is_video: bool,
    /// Display name of the remote peer.
    pub peer_display_name: String,
    /// Callee's answer envelope timestamp (Unix nanos).
    /// Live duration uses the active Call's local connected_at, avoiding
    /// remote wall-clock skew and signaling-to-connection latency.
    pub started_at_ns: u64,
    /// Cached avatar bytes for the remote peer, sourced from the local
    /// `AnonDm.peer_avatar` row. Threaded so the active-call tile shows the
    /// peer's picture immediately on connect without an HTTP round-trip.
    pub peer_avatar: Option<Vec<u8>>,
}

/// Outcome of `handle_anon_call_answer`.
///
/// Group anon calls fan out answers to every participant, so the caller (and
/// any already-joined callee) receives *N* answer events per call. These
/// must be distinguished from an actual decline.
#[derive(Debug)]
pub enum AnonCallAnswerOutcome {
    /// The call was accepted and this participant just connected to LiveKit.
    Connected(AnonCallSetupResult),
    /// Peer declined the call (`accepted=false`).
    Declined,
    /// We are already in this call (own echo or another participant's answer
    /// to the same room). UI should ignore — the call is already active.
    AlreadyInCall,
}

impl std::fmt::Debug for AnonCallSetupResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnonCallSetupResult")
            .field("room_name", &self.room_name)
            .field("channel_id", &self.channel_id)
            .field("encryption_key", &"<redacted 32 bytes>")
            .field("verification_emojis", &self.verification_emojis)
            .field("is_video", &self.is_video)
            .field("peer_display_name", &self.peer_display_name)
            .finish()
    }
}

// ============================================================================
// Call History (local-only record of past calls)
// ============================================================================

/// Direction of the call relative to this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

impl CallDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "incoming" => Some(Self::Incoming),
            "outgoing" => Some(Self::Outgoing),
            _ => None,
        }
    }
}

/// Outcome of a historical call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallResult {
    /// Call connected and was hung up normally.
    Completed,
    /// Incoming call that was never answered (ringing expired / user away).
    Missed,
    /// Explicitly rejected by either side before connecting.
    Declined,
    /// Setup error (network, LiveKit, signaling failure).
    Failed,
}

impl CallResult {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Missed => "missed",
            Self::Declined => "declined",
            Self::Failed => "failed",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "completed" => Some(Self::Completed),
            "missed" => Some(Self::Missed),
            "declined" => Some(Self::Declined),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A single entry in the local call history.
///
/// Written by `call_history_recorder` (incoming / connected / ended) and by
/// `ChatterClient::start_call` (outgoing). Read by the Calls screen. Purely
/// local — no server sync until a call-history RPC exists.
///
/// `conversation_id` + `conversation_type` are the same pair the sidebar
/// routes on (`ConversationType::as_str`), so a history row can reopen its
/// conversation and redial without the UI re-deriving routing rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHistoryEntry {
    /// Stable local ID: the signaling call id for DM/channel calls, the
    /// LiveKit room name for anonymous ones. Same string the call
    /// orchestrator uses as its `CallHandle`.
    pub id: String,
    /// Conversation the call happened in (`ChannelId`, peer `UserId`,
    /// `AnonDmId`, or `AnonGroupId` as a string).
    pub conversation_id: String,
    /// Which flavour of conversation `conversation_id` names.
    pub conversation_type: crate::domain::message::ConversationType,
    /// Conversation/peer display name at call time. Empty when unknown.
    pub peer_name: String,
    /// When the call started (our clock).
    pub started_at: DateTime<Utc>,
    /// When the call ended, or None if still in-progress.
    pub ended_at: Option<DateTime<Utc>>,
    /// Direction from this device's perspective.
    pub direction: CallDirection,
    /// Final outcome of the call.
    pub result: CallResult,
    /// Call length in seconds. None if the call never connected.
    pub duration_secs: Option<i64>,
    /// Whether this was a video call (vs voice-only).
    pub has_video: bool,
}

/// Filter for listing call history entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallHistoryFilter {
    /// All entries, newest first.
    #[default]
    All,
    /// Only missed calls.
    Missed,
    /// Only outgoing calls (placed by this device).
    Outgoing,
    /// Only incoming calls.
    Incoming,
}

/// Pending DM call state during offer→answer exchange.
///
/// Stored in core while waiting for the peer's ephemeral key answer.
/// The `EphemeralCallKey` is automatically zeroized on drop.
pub struct PendingDmCall {
    /// The call ID assigned by the server.
    pub call_id: CallId,
    /// The channel ID for the DM.
    pub channel_id: ChannelId,
    /// Our ephemeral X25519 keypair (zeroized on drop).
    pub ephemeral_key: EphemeralCallKey,
    /// Peer's user ID.
    pub peer_id: UserId,
    /// Call type label (e.g., "Voice Call", "Video Call").
    pub call_type: String,
    /// Whether this is a video call.
    pub is_video: bool,
}
