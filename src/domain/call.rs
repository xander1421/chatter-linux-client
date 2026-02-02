//! Call domain entity.
//!
//! Represents voice/video calls and participants.

/// Represents a voice or video call.
pub struct Call;

/// Call participant information.
pub struct Participant;

/// Call state machine.
pub enum CallState {
    /// Call is being initiated.
    Initiating,
    /// Call is ringing.
    Ringing,
    /// Call is connected.
    Connected,
    /// Call has ended.
    Ended,
}

/// E2EE verification badge for calls.
pub struct VerificationBadge;
