//! Typing indicator stream handler.
//!
//! Tracks who is currently typing in each channel with automatic expiry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::proto::chat_v1::TypingIndicator;

/// Default typing indicator timeout.
const TYPING_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity for typing event channel.
const CHANNEL_CAPACITY: usize = 64;

/// Typing stream handler with per-channel aggregation.
///
/// Tracks which users are typing in each channel, with automatic
/// expiry after timeout. Provides efficient queries for UI.
///
/// # Example
///
/// ```ignore
/// let typing = TypingAggregator::new();
///
/// // Track typing from stream events
/// typing.on_typing_started("channel-1", "user-123");
///
/// // Query who's typing
/// let typers = typing.get_typing_users("channel-1");
/// // Returns ["user-123"] if within timeout
///
/// // Clean up expired entries
/// typing.cleanup_expired();
/// ```
pub struct TypingAggregator {
    /// Per-channel typing state (channel_id -> user_id -> expiry time).
    state: Arc<RwLock<HashMap<String, HashMap<String, Instant>>>>,

    /// Broadcast for typing state changes.
    changes: broadcast::Sender<TypingStateChange>,

    /// Typing timeout duration.
    timeout: Duration,
}

/// Typing state change event.
#[derive(Clone, Debug)]
pub struct TypingStateChange {
    /// Channel where typing state changed.
    pub channel_id: String,
    /// Users currently typing (after the change).
    pub typing_users: Vec<String>,
}

impl TypingAggregator {
    /// Create a new TypingAggregator with default timeout.
    pub fn new() -> Self {
        Self::with_timeout(TYPING_TIMEOUT)
    }

    /// Create a new TypingAggregator with custom timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
            changes: broadcast::channel(CHANNEL_CAPACITY).0,
            timeout,
        }
    }

    /// Subscribe to typing state changes.
    pub fn on_typing_change(&self) -> broadcast::Receiver<TypingStateChange> {
        self.changes.subscribe()
    }

    /// Process a typing indicator event.
    ///
    /// The channel_id is extracted from the indicator itself.
    pub fn process(&self, indicator: TypingIndicator) {
        if indicator.is_typing {
            self.on_typing_started(&indicator.channel_id, &indicator.user_id);
        } else {
            self.on_typing_stopped(&indicator.channel_id, &indicator.user_id);
        }
    }

    /// Mark a user as typing in a channel.
    pub fn on_typing_started(&self, channel_id: &str, user_id: &str) {
        let expiry = Instant::now() + self.timeout;

        let changed = {
            let mut state = self.state.write();
            let channel = state.entry(channel_id.to_string()).or_default();
            let was_typing = channel.contains_key(user_id);
            channel.insert(user_id.to_string(), expiry);
            !was_typing
        };

        if changed {
            self.emit_change(channel_id);
        }
    }

    /// Mark a user as no longer typing in a channel.
    pub fn on_typing_stopped(&self, channel_id: &str, user_id: &str) {
        let changed = {
            let mut state = self.state.write();
            if let Some(channel) = state.get_mut(channel_id) {
                channel.remove(user_id).is_some()
            } else {
                false
            }
        };

        if changed {
            self.emit_change(channel_id);
        }
    }

    /// Get list of users currently typing in a channel.
    ///
    /// Automatically filters out expired entries.
    pub fn get_typing_users(&self, channel_id: &str) -> Vec<String> {
        let now = Instant::now();
        let state = self.state.read();

        state
            .get(channel_id)
            .map(|channel| {
                channel
                    .iter()
                    .filter(|(_, expiry)| **expiry > now)
                    .map(|(user_id, _)| user_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Check if anyone is typing in a channel.
    pub fn is_anyone_typing(&self, channel_id: &str) -> bool {
        !self.get_typing_users(channel_id).is_empty()
    }

    /// Clean up expired typing entries.
    ///
    /// Call periodically (e.g., every 5 seconds) to remove stale entries.
    pub fn cleanup_expired(&self) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        let mut channels_changed = Vec::new();

        {
            let mut state = self.state.write();

            for (channel_id, channel) in state.iter_mut() {
                let before = channel.len();
                channel.retain(|_, expiry| *expiry > now);
                let after = channel.len();

                if before != after {
                    removed += before - after;
                    channels_changed.push(channel_id.clone());
                }
            }

            // Remove empty channels
            state.retain(|_, channel| !channel.is_empty());
        }

        // Emit changes for affected channels
        for channel_id in channels_changed {
            self.emit_change(&channel_id);
        }

        removed
    }

    /// Clear all typing state.
    pub fn clear(&self) {
        self.state.write().clear();
    }

    /// Emit a typing state change event.
    fn emit_change(&self, channel_id: &str) {
        let typing_users = self.get_typing_users(channel_id);
        let _ = self.changes.send(TypingStateChange {
            channel_id: channel_id.to_string(),
            typing_users,
        });
    }
}

impl Default for TypingAggregator {
    fn default() -> Self {
        Self::new()
    }
}

/// Type alias for backwards compatibility.
pub type TypingStream = TypingAggregator;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_typing_started() {
        let agg = TypingAggregator::new();

        agg.on_typing_started("channel-1", "user-1");

        let typers = agg.get_typing_users("channel-1");
        assert_eq!(typers, vec!["user-1"]);
    }

    #[test]
    fn test_typing_stopped() {
        let agg = TypingAggregator::new();

        agg.on_typing_started("channel-1", "user-1");
        agg.on_typing_stopped("channel-1", "user-1");

        let typers = agg.get_typing_users("channel-1");
        assert!(typers.is_empty());
    }

    #[test]
    fn test_multiple_typers() {
        let agg = TypingAggregator::new();

        agg.on_typing_started("channel-1", "user-1");
        agg.on_typing_started("channel-1", "user-2");

        let typers = agg.get_typing_users("channel-1");
        assert_eq!(typers.len(), 2);
        assert!(typers.contains(&"user-1".to_string()));
        assert!(typers.contains(&"user-2".to_string()));
    }

    #[test]
    fn test_is_anyone_typing() {
        let agg = TypingAggregator::new();

        assert!(!agg.is_anyone_typing("channel-1"));

        agg.on_typing_started("channel-1", "user-1");
        assert!(agg.is_anyone_typing("channel-1"));
    }

    #[test]
    fn test_process_indicator() {
        let agg = TypingAggregator::new();

        agg.process(TypingIndicator {
            user_id: "user-1".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: true,
        });

        assert!(agg.is_anyone_typing("channel-1"));

        agg.process(TypingIndicator {
            user_id: "user-1".to_string(),
            channel_id: "channel-1".to_string(),
            is_typing: false,
        });

        assert!(!agg.is_anyone_typing("channel-1"));
    }

    #[test]
    fn test_expiry() {
        let agg = TypingAggregator::with_timeout(Duration::from_millis(50));

        agg.on_typing_started("channel-1", "user-1");
        assert!(agg.is_anyone_typing("channel-1"));

        sleep(Duration::from_millis(100));

        // Should be expired now
        assert!(!agg.is_anyone_typing("channel-1"));
    }

    #[test]
    fn test_cleanup_expired() {
        let agg = TypingAggregator::with_timeout(Duration::from_millis(50));

        agg.on_typing_started("channel-1", "user-1");
        agg.on_typing_started("channel-1", "user-2");

        sleep(Duration::from_millis(100));

        let removed = agg.cleanup_expired();
        assert_eq!(removed, 2);
    }

    #[test]
    fn test_clear() {
        let agg = TypingAggregator::new();

        agg.on_typing_started("channel-1", "user-1");
        agg.on_typing_started("channel-2", "user-2");

        agg.clear();

        assert!(!agg.is_anyone_typing("channel-1"));
        assert!(!agg.is_anyone_typing("channel-2"));
    }

    #[tokio::test]
    async fn test_on_typing_change() {
        let agg = TypingAggregator::new();
        let mut rx = agg.on_typing_change();

        agg.on_typing_started("channel-1", "user-1");

        let change = rx.try_recv().unwrap();
        assert_eq!(change.channel_id, "channel-1");
        assert_eq!(change.typing_users, vec!["user-1"]);
    }
}
