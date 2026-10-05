//! Ephemeral message domain entity.
//!
//! Represents self-destructing messages with TTL configuration.
//! Ephemeral messages automatically expire after a set duration,
//! typically starting their countdown when the recipient reads them.

use crate::domain::ids::MessageId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ============================================================================
// MessageTTL
// ============================================================================

/// Message TTL (time-to-live) options.
///
/// Predefined durations for ephemeral messages. These match common
/// patterns in secure messaging apps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageTTL {
    /// 30 seconds.
    Seconds30,
    /// 1 minute.
    Minutes1,
    /// 5 minutes.
    Minutes5,
    /// 1 hour.
    Hours1,
    /// 24 hours.
    Hours24,
}

impl MessageTTL {
    /// Convert the TTL to a standard Duration.
    #[must_use]
    pub fn as_duration(&self) -> Duration {
        match self {
            Self::Seconds30 => Duration::from_secs(30),
            Self::Minutes1 => Duration::from_secs(60),
            Self::Minutes5 => Duration::from_secs(5 * 60),
            Self::Hours1 => Duration::from_secs(60 * 60),
            Self::Hours24 => Duration::from_secs(24 * 60 * 60),
        }
    }

    /// Parse a TTL from seconds (e.g., from server response).
    ///
    /// Returns None if the value doesn't match a known TTL variant.
    #[must_use]
    pub fn from_secs(secs: u64) -> Option<Self> {
        match secs {
            30 => Some(Self::Seconds30),
            60 => Some(Self::Minutes1),
            300 => Some(Self::Minutes5),
            3600 => Some(Self::Hours1),
            86400 => Some(Self::Hours24),
            _ => None,
        }
    }

    /// Get the TTL as total seconds.
    #[must_use]
    pub fn as_secs(&self) -> u64 {
        self.as_duration().as_secs()
    }

    /// Get a human-readable display string for the TTL.
    #[must_use]
    pub fn display(&self) -> &'static str {
        match self {
            Self::Seconds30 => "30 seconds",
            Self::Minutes1 => "1 minute",
            Self::Minutes5 => "5 minutes",
            Self::Hours1 => "1 hour",
            Self::Hours24 => "24 hours",
        }
    }

    /// Get all available TTL options (useful for UI dropdowns).
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Seconds30,
            Self::Minutes1,
            Self::Minutes5,
            Self::Hours1,
            Self::Hours24,
        ]
    }
}

// ============================================================================
// EphemeralState
// ============================================================================

/// State of an ephemeral message in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EphemeralState {
    /// Message has been sent but not yet read by recipient.
    Pending,
    /// Message has been read and countdown is active.
    Countdown {
        /// Time remaining before expiration.
        remaining: Duration,
    },
    /// Message has expired and should be deleted.
    Expired,
}

impl EphemeralState {
    /// Check if the message is in countdown state.
    #[must_use]
    pub fn is_countdown(&self) -> bool {
        matches!(self, Self::Countdown { .. })
    }

    /// Check if the message has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        matches!(self, Self::Expired)
    }

    /// Check if the message is still pending (not yet read).
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Get remaining time if in countdown, None otherwise.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        match self {
            Self::Countdown { remaining } => Some(*remaining),
            _ => None,
        }
    }
}

// ============================================================================
// EphemeralMessage
// ============================================================================

/// An ephemeral (self-destructing) message.
///
/// Ephemeral messages have a TTL that starts counting down when the
/// recipient reads the message. Once the TTL expires, the message
/// should be permanently deleted from both sender and recipient devices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EphemeralMessage {
    /// The underlying message this ephemeral data tracks.
    pub message_id: MessageId,
    /// Time-to-live configuration.
    pub ttl: MessageTTL,
    /// When the message will expire (computed when countdown starts).
    pub expires_at: DateTime<Utc>,
    /// When the recipient first read the message.
    pub read_at: Option<DateTime<Utc>>,
    /// Whether the countdown has started.
    pub countdown_started: bool,
}

impl EphemeralMessage {
    /// Create a new ephemeral message with the given TTL.
    ///
    /// The expiration time is initially set based on the TTL from creation,
    /// but will be recalculated when `mark_read` is called.
    #[must_use]
    pub fn new(message_id: MessageId, ttl: MessageTTL) -> Self {
        let now = Utc::now();
        let ttl_chrono = chrono::Duration::seconds(ttl.as_secs() as i64);
        Self {
            message_id,
            ttl,
            expires_at: now + ttl_chrono,
            read_at: None,
            countdown_started: false,
        }
    }

    /// Mark the message as read and start the countdown.
    ///
    /// This sets `read_at` to the current time and recalculates `expires_at`
    /// based on the TTL from the read time.
    pub fn mark_read(&mut self) {
        if !self.countdown_started {
            let now = Utc::now();
            self.read_at = Some(now);
            self.countdown_started = true;
            let ttl_chrono = chrono::Duration::seconds(self.ttl.as_secs() as i64);
            self.expires_at = now + ttl_chrono;
        }
    }

    /// Compute the current state of the ephemeral message.
    #[must_use]
    pub fn state(&self) -> EphemeralState {
        if !self.countdown_started {
            return EphemeralState::Pending;
        }

        let now = Utc::now();
        if now >= self.expires_at {
            EphemeralState::Expired
        } else {
            let remaining_chrono = self.expires_at - now;
            let remaining_secs = remaining_chrono.num_seconds().max(0) as u64;
            EphemeralState::Countdown {
                remaining: Duration::from_secs(remaining_secs),
            }
        }
    }

    /// Get the remaining time if countdown has started.
    ///
    /// Returns None if:
    /// - Message hasn't been read yet (countdown not started)
    /// - Message has already expired (no time remaining)
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        if !self.countdown_started {
            return None;
        }

        let now = Utc::now();
        if now >= self.expires_at {
            return None;
        }

        let remaining_chrono = self.expires_at - now;
        let remaining_secs = remaining_chrono.num_seconds().max(0) as u64;
        Some(Duration::from_secs(remaining_secs))
    }

    /// Check if the message has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.countdown_started && Utc::now() >= self.expires_at
    }

    /// Check if the countdown is currently active (read but not expired).
    #[must_use]
    pub fn is_countdown_active(&self) -> bool {
        self.countdown_started && !self.is_expired()
    }

    /// Check if the message is still pending (not yet read).
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !self.countdown_started
    }

    /// Get a formatted string of remaining time for display.
    ///
    /// Returns a human-readable format like "29s", "4:30", "59:59".
    #[must_use]
    pub fn remaining_display(&self) -> Option<String> {
        self.remaining().map(|d| {
            let secs = d.as_secs();
            if secs < 60 {
                format!("{}s", secs)
            } else if secs < 3600 {
                format!("{}:{:02}", secs / 60, secs % 60)
            } else {
                format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_ttl_as_duration() {
        assert_eq!(MessageTTL::Seconds30.as_duration(), Duration::from_secs(30));
        assert_eq!(MessageTTL::Minutes1.as_duration(), Duration::from_secs(60));
        assert_eq!(MessageTTL::Minutes5.as_duration(), Duration::from_secs(300));
        assert_eq!(MessageTTL::Hours1.as_duration(), Duration::from_secs(3600));
        assert_eq!(
            MessageTTL::Hours24.as_duration(),
            Duration::from_secs(86400)
        );
    }

    #[test]
    fn test_message_ttl_from_secs() {
        assert_eq!(MessageTTL::from_secs(30), Some(MessageTTL::Seconds30));
        assert_eq!(MessageTTL::from_secs(60), Some(MessageTTL::Minutes1));
        assert_eq!(MessageTTL::from_secs(300), Some(MessageTTL::Minutes5));
        assert_eq!(MessageTTL::from_secs(3600), Some(MessageTTL::Hours1));
        assert_eq!(MessageTTL::from_secs(86400), Some(MessageTTL::Hours24));
        assert_eq!(MessageTTL::from_secs(45), None);
        assert_eq!(MessageTTL::from_secs(0), None);
    }

    #[test]
    fn test_message_ttl_as_secs() {
        assert_eq!(MessageTTL::Seconds30.as_secs(), 30);
        assert_eq!(MessageTTL::Minutes1.as_secs(), 60);
        assert_eq!(MessageTTL::Hours24.as_secs(), 86400);
    }

    #[test]
    fn test_message_ttl_display() {
        assert_eq!(MessageTTL::Seconds30.display(), "30 seconds");
        assert_eq!(MessageTTL::Minutes1.display(), "1 minute");
        assert_eq!(MessageTTL::Minutes5.display(), "5 minutes");
        assert_eq!(MessageTTL::Hours1.display(), "1 hour");
        assert_eq!(MessageTTL::Hours24.display(), "24 hours");
    }

    #[test]
    fn test_message_ttl_all() {
        let all = MessageTTL::all();
        assert_eq!(all.len(), 5);
        assert!(all.contains(&MessageTTL::Seconds30));
        assert!(all.contains(&MessageTTL::Hours24));
    }

    #[test]
    fn test_ephemeral_message_new() {
        let message_id = MessageId::new();
        let ephemeral = EphemeralMessage::new(message_id, MessageTTL::Minutes5);

        assert_eq!(ephemeral.message_id, message_id);
        assert_eq!(ephemeral.ttl, MessageTTL::Minutes5);
        assert!(ephemeral.read_at.is_none());
        assert!(!ephemeral.countdown_started);
    }

    #[test]
    fn test_ephemeral_message_mark_read() {
        let message_id = MessageId::new();
        let mut ephemeral = EphemeralMessage::new(message_id, MessageTTL::Seconds30);

        assert!(ephemeral.is_pending());
        assert!(!ephemeral.countdown_started);

        ephemeral.mark_read();

        assert!(ephemeral.countdown_started);
        assert!(ephemeral.read_at.is_some());
        assert!(!ephemeral.is_pending());
    }

    #[test]
    fn test_ephemeral_message_mark_read_idempotent() {
        let message_id = MessageId::new();
        let mut ephemeral = EphemeralMessage::new(message_id, MessageTTL::Seconds30);

        ephemeral.mark_read();
        let first_read_at = ephemeral.read_at;
        let first_expires_at = ephemeral.expires_at;

        // Small delay
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Second mark_read should not change anything
        ephemeral.mark_read();

        assert_eq!(ephemeral.read_at, first_read_at);
        assert_eq!(ephemeral.expires_at, first_expires_at);
    }

    #[test]
    fn test_ephemeral_message_state_pending() {
        let message_id = MessageId::new();
        let ephemeral = EphemeralMessage::new(message_id, MessageTTL::Minutes5);

        let state = ephemeral.state();
        assert!(state.is_pending());
        assert!(!state.is_countdown());
        assert!(!state.is_expired());
    }

    #[test]
    fn test_ephemeral_message_state_countdown() {
        let message_id = MessageId::new();
        let mut ephemeral = EphemeralMessage::new(message_id, MessageTTL::Hours1);

        ephemeral.mark_read();

        let state = ephemeral.state();
        assert!(state.is_countdown());
        assert!(!state.is_pending());
        assert!(!state.is_expired());

        // Should have remaining time close to 1 hour
        if let EphemeralState::Countdown { remaining } = state {
            assert!(remaining.as_secs() > 3590); // Give 10 seconds tolerance
        } else {
            panic!("Expected Countdown state");
        }
    }

    #[test]
    fn test_ephemeral_message_remaining() {
        let message_id = MessageId::new();
        let mut ephemeral = EphemeralMessage::new(message_id, MessageTTL::Hours1);

        // No remaining before read
        assert!(ephemeral.remaining().is_none());

        ephemeral.mark_read();

        // Should have remaining after read
        let remaining = ephemeral.remaining();
        assert!(remaining.is_some());
        assert!(remaining.unwrap().as_secs() > 3590);
    }

    #[test]
    fn test_ephemeral_message_is_countdown_active() {
        let message_id = MessageId::new();
        let mut ephemeral = EphemeralMessage::new(message_id, MessageTTL::Minutes5);

        assert!(!ephemeral.is_countdown_active());

        ephemeral.mark_read();
        assert!(ephemeral.is_countdown_active());
    }

    #[test]
    fn test_ephemeral_message_remaining_display() {
        let message_id = MessageId::new();
        let mut ephemeral = EphemeralMessage::new(message_id, MessageTTL::Hours1);

        // No display before read
        assert!(ephemeral.remaining_display().is_none());

        ephemeral.mark_read();

        // Should have a display string
        let display = ephemeral.remaining_display();
        assert!(display.is_some());
        // Format should be H:MM:SS for hours
        let display_str = display.unwrap();
        assert!(display_str.contains(":"));
    }

    #[test]
    fn test_ephemeral_state_remaining() {
        let state_pending = EphemeralState::Pending;
        assert!(state_pending.remaining().is_none());

        let state_countdown = EphemeralState::Countdown {
            remaining: Duration::from_secs(120),
        };
        assert_eq!(state_countdown.remaining(), Some(Duration::from_secs(120)));

        let state_expired = EphemeralState::Expired;
        assert!(state_expired.remaining().is_none());
    }
}
