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
            poll_interval: Duration::from_secs(5),
            poll_interval_idle: Duration::from_secs(30),
            max_batch_size: 100,
            retry_attempts: 3,
            ack_batch_size: 50,
        }
    }
}

impl InboxConfig {
    /// Create a new configuration builder.
    pub fn builder() -> InboxConfigBuilder {
        InboxConfigBuilder::new()
    }
}

/// Builder for `InboxConfig`.
#[derive(Debug, Clone)]
pub struct InboxConfigBuilder {
    config: InboxConfig,
}

impl InboxConfigBuilder {
    /// Create a new builder with default values.
    pub fn new() -> Self {
        Self {
            config: InboxConfig::default(),
        }
    }

    /// Set the base polling interval.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.config.poll_interval = interval;
        self
    }

    /// Set the idle polling interval.
    pub fn poll_interval_idle(mut self, interval: Duration) -> Self {
        self.config.poll_interval_idle = interval;
        self
    }

    /// Set the maximum batch size for polling.
    pub fn max_batch_size(mut self, size: u32) -> Self {
        self.config.max_batch_size = size;
        self
    }

    /// Set the number of retry attempts.
    pub fn retry_attempts(mut self, attempts: u32) -> Self {
        self.config.retry_attempts = attempts;
        self
    }

    /// Set the acknowledgment batch size.
    pub fn ack_batch_size(mut self, size: u32) -> Self {
        self.config.ack_batch_size = size;
        self
    }

    /// Build the configuration.
    pub fn build(self) -> InboxConfig {
        self.config
    }
}

impl Default for InboxConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_config_defaults() {
        let config = InboxConfig::default();
        assert_eq!(config.poll_interval, Duration::from_secs(5));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(30));
        assert_eq!(config.max_batch_size, 100);
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.ack_batch_size, 50);
    }

    #[test]
    fn test_inbox_config_builder() {
        let config = InboxConfig::builder()
            .poll_interval(Duration::from_secs(10))
            .poll_interval_idle(Duration::from_secs(60))
            .max_batch_size(50)
            .retry_attempts(5)
            .ack_batch_size(25)
            .build();

        assert_eq!(config.poll_interval, Duration::from_secs(10));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(60));
        assert_eq!(config.max_batch_size, 50);
        assert_eq!(config.retry_attempts, 5);
        assert_eq!(config.ack_batch_size, 25);
    }
}
