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
    /// Check if the call is in an active state (connected or reconnecting).
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Connected | Self::Reconnecting)
    }

    /// Check if the call has ended.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        matches!(self, Self::Ended { .. })
    }

    /// Get the end reason if the call has ended.
    #[must_use]
    pub fn end_reason(&self) -> Option<CallEndReason> {
        match self {
            Self::Ended { reason } => Some(*reason),
            _ => None,
        }
    }
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

    /// Toggle the mute state.
    pub fn toggle_mute(&mut self) {
        self.is_muted = !self.is_muted;
    }

    /// Toggle the video state.
    pub fn toggle_video(&mut self) {
        self.is_video_enabled = !self.is_video_enabled;
    }

    /// Start screen sharing.
    pub fn start_screen_share(&mut self) {
        self.is_screen_sharing = true;
    }

    /// Stop screen sharing.
    pub fn stop_screen_share(&mut self) {
        self.is_screen_sharing = false;
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

    /// Transition to the Connecting state.
    pub fn connect(&mut self) {
        self.state = CallState::Connecting;
    }

    /// Transition to the Connected state and set connected_at timestamp.
    pub fn start(&mut self) {
        self.state = CallState::Connected;
        self.connected_at = Some(Utc::now());
    }

    /// Transition to the Reconnecting state.
    pub fn reconnect(&mut self) {
        self.state = CallState::Reconnecting;
    }

    /// End the call with the given reason.
    pub fn end(&mut self, reason: CallEndReason) {
        self.state = CallState::Ended { reason };
        self.ended_at = Some(Utc::now());
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

    /// Check if the call is currently active (connected or reconnecting).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state.is_active()
    }

    /// Add a participant to the call.
    pub fn add_participant(&mut self, participant: Participant) {
        // Don't add duplicates
        if !self.participants.iter().any(|p| p.user_id == participant.user_id) {
            self.participants.push(participant);
        }
    }

    /// Remove a participant from the call by user ID.
    pub fn remove_participant(&mut self, user_id: UserId) {
        self.participants.retain(|p| p.user_id != user_id);
    }

    /// Get a participant by user ID.
    #[must_use]
    pub fn get_participant(&self, user_id: UserId) -> Option<&Participant> {
        self.participants.iter().find(|p| p.user_id == user_id)
    }

    /// Get a mutable reference to a participant by user ID.
    #[must_use]
    pub fn get_participant_mut(&mut self, user_id: UserId) -> Option<&mut Participant> {
        self.participants.iter_mut().find(|p| p.user_id == user_id)
    }

    /// Get the number of participants in the call.
    #[must_use]
    pub fn participant_count(&self) -> usize {
        self.participants.len()
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
    /// Create a new unverified verification badge.
    #[must_use]
    pub fn new(safety_number: String) -> Self {
        Self {
            safety_number,
            is_verified: false,
            last_verified_at: None,
        }
    }

    /// Mark the safety number as verified.
    pub fn verify(&mut self) {
        self.is_verified = true;
        self.last_verified_at = Some(Utc::now());
    }

    /// Mark the safety number as unverified.
    pub fn unverify(&mut self) {
        self.is_verified = false;
    }

    /// Get the safety number formatted for display.
    ///
    /// Formats the safety number in space-separated groups of 5 digits for readability.
    #[must_use]
    pub fn display_code(&self) -> String {
        self.safety_number
            .chars()
            .collect::<Vec<_>>()
            .chunks(5)
            .map(|chunk| chunk.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_call_lifecycle() {
        let channel_id = ChannelId::new();
        let initiator_id = UserId::new();

        let mut call = Call::new(channel_id, initiator_id, CallType::Voice);
        assert!(matches!(call.state, CallState::Initiating));
        assert!(call.started_at.is_some());

        call.ring();
        assert!(matches!(call.state, CallState::Ringing));

        call.start();
        assert!(matches!(call.state, CallState::Connected));
        assert!(call.connected_at.is_some());
        assert!(call.is_active());

        call.end(CallEndReason::Completed);
        assert!(matches!(call.state, CallState::Ended { reason: CallEndReason::Completed }));
        assert!(call.ended_at.is_some());
        assert!(!call.is_active());
    }

    #[test]
    fn test_call_duration() {
        let channel_id = ChannelId::new();
        let initiator_id = UserId::new();

        let mut call = Call::new(channel_id, initiator_id, CallType::Video);

        // No duration before connected
        assert!(call.duration().is_none());

        call.start();
        // Duration exists once connected
        assert!(call.duration().is_some());

        call.end(CallEndReason::Completed);
        // Duration still exists after ended
        assert!(call.duration().is_some());
    }

    #[test]
    fn test_participant() {
        let user_id = UserId::new();
        let mut participant = Participant::new(user_id);

        assert!(!participant.is_muted);
        assert!(!participant.is_video_enabled);
        assert!(!participant.is_screen_sharing);

        participant.toggle_mute();
        assert!(participant.is_muted);

        participant.toggle_video();
        assert!(participant.is_video_enabled);

        participant.start_screen_share();
        assert!(participant.is_screen_sharing);
    }

    #[test]
    fn test_call_participants() {
        let channel_id = ChannelId::new();
        let initiator_id = UserId::new();
        let mut call = Call::new(channel_id, initiator_id, CallType::Group);

        let user1 = UserId::new();
        let user2 = UserId::new();

        call.add_participant(Participant::new(user1));
        call.add_participant(Participant::new(user2));
        assert_eq!(call.participant_count(), 2);

        // Adding duplicate should not increase count
        call.add_participant(Participant::new(user1));
        assert_eq!(call.participant_count(), 2);

        call.remove_participant(user1);
        assert_eq!(call.participant_count(), 1);
        assert!(call.get_participant(user1).is_none());
        assert!(call.get_participant(user2).is_some());
    }

    #[test]
    fn test_verification_badge() {
        let mut badge = VerificationBadge::new("123456789012345".to_string());

        assert!(!badge.is_verified);
        assert!(badge.last_verified_at.is_none());

        badge.verify();
        assert!(badge.is_verified);
        assert!(badge.last_verified_at.is_some());

        // Test display formatting
        assert_eq!(badge.display_code(), "12345 67890 12345");
    }

    #[test]
    fn test_call_state_helpers() {
        let initiating = CallState::Initiating;
        assert!(!initiating.is_active());
        assert!(!initiating.is_ended());
        assert!(initiating.end_reason().is_none());

        let connected = CallState::Connected;
        assert!(connected.is_active());
        assert!(!connected.is_ended());

        let ended = CallState::Ended { reason: CallEndReason::Missed };
        assert!(!ended.is_active());
        assert!(ended.is_ended());
        assert_eq!(ended.end_reason(), Some(CallEndReason::Missed));
    }
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
    /// Shared anchor for the duration timer: ns since epoch from the callee's
    /// `AnonCallAnswer.timestamp`. Both peers anchor on the same value so
    /// their displayed durations match regardless of wall-clock skew.
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

/// Call setup data for mobile clients (Android/iOS).
///
/// Contains LiveKit connection info and encryption key so the mobile client
/// can connect via the platform-native LiveKit SDK instead of the desktop
/// GStreamer-based media stack. `encryption_key` is wrapped in `Zeroizing`
/// so the E2EE key is wiped from memory when this struct is dropped; the
/// custom `Debug` impl redacts the key + LiveKit token so they cannot leak
/// through log statements or `format!("{:?}", ...)`.
#[derive(Clone)]
pub struct MobileCallSetup {
    /// The call ID.
    pub call_id: CallId,
    /// The channel ID.
    pub channel_id: ChannelId,
    /// Verification emojis for secure call verification (4 emojis).
    pub verification_emojis: [String; 4],
    /// Whether this is a video call.
    pub is_video: bool,
    /// LiveKit server URL.
    pub livekit_url: String,
    /// LiveKit room access token.
    pub livekit_token: String,
    /// E2EE encryption key (32 bytes) for LiveKit frame encryption. Zeroized on drop.
    pub encryption_key: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for MobileCallSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileCallSetup")
            .field("call_id", &self.call_id)
            .field("channel_id", &self.channel_id)
            .field("verification_emojis", &self.verification_emojis)
            .field("is_video", &self.is_video)
            .field("livekit_url", &self.livekit_url)
            .field("livekit_token", &"<redacted>")
            .field("encryption_key", &"<redacted 32 bytes>")
            .finish()
    }
}

// ============================================================================
// Call History (local-only record of past calls)
// ============================================================================

/// Kind of peer a call was with.
///
/// Maps directly to the `peer_type` column in the `call_history` table
/// (`'user' | 'anon' | 'group'`). Kept distinct from `CallType` because a
/// video group call and a voice group call share a peer_type but differ
/// in `has_video`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallHistoryPeerType {
    /// A known user (DM / channel call).
    User,
    /// An anonymous-DM peer (onion-style).
    Anon,
    /// A group call (anon group or regular channel group call).
    Group,
}

impl CallHistoryPeerType {
    /// Serialize for the `peer_type` TEXT column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Anon => "anon",
            Self::Group => "group",
        }
    }

    /// Parse a `peer_type` TEXT column value.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "anon" => Some(Self::Anon),
            "group" => Some(Self::Group),
            _ => None,
        }
    }
}

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
/// Written by the call signaling service when observable events occur
/// (start, accept, decline, end, missed). Read by the UI via the bridge's
/// `get_call_history` RPC. Purely local — no server sync until a call-history
/// RPC exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHistoryEntry {
    /// Stable local ID (typically the signaling `call_id` / room name).
    pub id: String,
    /// Remote peer identifier (user id, anon id, or group id — opaque to repo).
    pub peer_id: String,
    /// Peer entity kind.
    pub peer_type: CallHistoryPeerType,
    /// Peer display name at call time. Empty string when unknown.
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
