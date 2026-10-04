//! Inbox polling configuration.
//!
//! Configuration types for controlling the inbox polling behavior
//! including intervals, batch sizes, and retry attempts.

use std::time::Duration;

/// Configuration for the inbox polling system.
#[derive(Debug, Clone)]
pub struct InboxConfig {
    /// Base polling interval when active.
    pub poll_interval: Duration,
    /// Polling interval when idle (no recent messages).
    pub poll_interval_idle: Duration,
    /// Maximum messages to fetch per poll request.
    pub max_batch_size: u32,
    /// Number of retry attempts for failed messages.
    pub retry_attempts: u32,
    /// Number of messages to acknowledge per request.
    pub ack_batch_size: u32,
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            // Active polling: 2s for fast bootstrap handshake (NDSS '21 measured ~1.5s delivery)
            poll_interval: Duration::from_secs(2),
            // Idle polling: 5s - still responsive for incoming DMs/bootstrap requests
            poll_interval_idle: Duration::from_secs(5),
            max_batch_size: 100,
            retry_attempts: 3,
            ack_batch_size: 50,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_config_defaults() {
        let config = InboxConfig::default();
        assert_eq!(config.poll_interval, Duration::from_secs(2));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(5));
        assert_eq!(config.max_batch_size, 100);
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.ack_batch_size, 50);
    }
}
