//! Inbox state types.
//!
//! Types for tracking the current state of the inbox polling system
//! and collecting statistics.

use std::time::Instant;

/// Current state of the inbox polling system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InboxState {
    /// Not currently polling.
    #[default]
    Idle,
    /// Actively polling the server.
    Polling,
    /// Processing received messages.
    Processing,
    /// Explicitly stopped.
    Stopped,
}

/// Statistics for monitoring inbox performance.
#[derive(Debug, Clone, Copy, Default)]
pub struct InboxStats {
    /// Total messages received.
    pub messages_received: u64,
    /// Total messages successfully processed.
    pub messages_processed: u64,
    /// Total messages that failed processing.
    pub messages_failed: u64,
    /// Time of last poll (if any).
    pub last_poll_at: Option<Instant>,
    /// Number of messages in last poll.
    pub last_poll_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_state_default() {
        let state = InboxState::default();
        assert_eq!(state, InboxState::Idle);
    }

    #[test]
    fn test_inbox_stats_default() {
        let stats = InboxStats::default();
        assert_eq!(stats.messages_received, 0);
        assert_eq!(stats.messages_processed, 0);
        assert_eq!(stats.messages_failed, 0);
        assert!(stats.last_poll_at.is_none());
        assert_eq!(stats.last_poll_count, 0);
    }
}
