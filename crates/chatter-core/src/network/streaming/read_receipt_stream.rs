//! Read receipt stream handler.
//!
//! Tracks read receipt high-water marks per user per channel.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::proto::chat_v1::ReadReceipt;

/// Capacity for read receipt event channel.
const CHANNEL_CAPACITY: usize = 64;

/// Maximum number of channels to track read receipts for.
/// Oldest channels are evicted when this limit is exceeded.
const MAX_TRACKED_CHANNELS: usize = 100;

/// Maximum number of users to track per channel.
/// Oldest users are evicted when this limit is exceeded.
const MAX_USERS_PER_CHANNEL: usize = 500;

/// Read receipt aggregator with high-water mark tracking.
///
/// Tracks the last read message for each user in each channel.
/// Only the highest (most recent) read position is kept.
/// Uses LRU eviction to bound memory usage.
pub struct ReadReceiptAggregator {
    /// Per-channel read states (channel_id -> user_id -> last read message).
    state: Arc<RwLock<HashMap<String, HashMap<String, ReadState>>>>,

    /// LRU order for channel eviction (most recent at back).
    channel_lru: Arc<RwLock<VecDeque<String>>>,

    /// Broadcast for read receipt events.
    events: broadcast::Sender<ReadReceipt>,
}

/// Read state for a user in a channel.
#[derive(Clone, Debug)]
pub struct ReadState {
    /// Last read message ID.
    pub message_id: String,
    /// Timestamp of the read receipt.
    pub timestamp: u64,
}

impl ReadReceiptAggregator {
    /// Create a new ReadReceiptAggregator.
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
            channel_lru: Arc::new(RwLock::new(VecDeque::new())),
            events: broadcast::channel(CHANNEL_CAPACITY).0,
        }
    }

    /// Subscribe to read receipt events.
    pub fn on_read_receipt(&self) -> broadcast::Receiver<ReadReceipt> {
        self.events.subscribe()
    }

    /// Process a read receipt event.
    pub fn process(&self, receipt: ReadReceipt) {
        // Update state (only if newer)
        {
            let mut state = self.state.write();
            let mut lru = self.channel_lru.write();

            // Touch channel in LRU
            lru.retain(|id| id != &receipt.channel_id);
            lru.push_back(receipt.channel_id.clone());

            // Evict oldest channels if over limit
            while lru.len() > MAX_TRACKED_CHANNELS {
                if let Some(oldest) = lru.pop_front() {
                    state.remove(&oldest);
                }
            }

            let channel = state.entry(receipt.channel_id.clone()).or_default();

            let should_update = channel
                .get(&receipt.user_id)
                .map(|existing| receipt.timestamp > existing.timestamp)
                .unwrap_or(true);

            if should_update {
                channel.insert(
                    receipt.user_id.clone(),
                    ReadState {
                        message_id: receipt.last_read_id.clone(),
                        timestamp: receipt.timestamp,
                    },
                );

                // Evict oldest users if channel is over limit
                if channel.len() > MAX_USERS_PER_CHANNEL {
                    // Find and remove user with oldest timestamp
                    if let Some(oldest_user) = channel
                        .iter()
                        .min_by_key(|(_, s)| s.timestamp)
                        .map(|(u, _)| u.clone())
                    {
                        channel.remove(&oldest_user);
                    }
                }
            }
        }

        // Broadcast the event
        let _ = self.events.send(receipt);
    }

    /// Get the last read message ID for a user in a channel.
    pub fn get_last_read(&self, channel_id: &str, user_id: &str) -> Option<String> {
        self.state
            .read()
            .get(channel_id)
            .and_then(|channel| channel.get(user_id))
            .map(|state| state.message_id.clone())
    }

    /// Get all read states for a channel.
    pub fn get_channel_read_states(&self, channel_id: &str) -> HashMap<String, ReadState> {
        self.state
            .read()
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Check if a specific message has been read by a user.
    ///
    /// Note: This only checks the high-water mark, not individual messages.
    /// A message is considered "read" if the user's last read is at or after it.
    pub fn has_user_read_message(
        &self,
        channel_id: &str,
        user_id: &str,
        message_id: &str,
    ) -> Option<bool> {
        // We can't easily compare message IDs without ordering info.
        // This returns Some(true) if the exact message is the last read.
        // For proper comparison, callers should use message timestamps.
        self.get_last_read(channel_id, user_id)
            .map(|last| last == message_id)
    }

    /// Get users who have read up to a specific message.
    ///
    /// Returns users whose last_read_id matches the given message.
    pub fn get_readers_of_message(&self, channel_id: &str, message_id: &str) -> Vec<String> {
        self.state
            .read()
            .get(channel_id)
            .map(|channel| {
                channel
                    .iter()
                    .filter(|(_, state)| state.message_id == message_id)
                    .map(|(user_id, _)| user_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clear all read states.
    pub fn clear(&self) {
        self.state.write().clear();
        self.channel_lru.write().clear();
    }

    /// Clear read states for a specific channel.
    pub fn clear_channel(&self, channel_id: &str) {
        self.state.write().remove(channel_id);
        self.channel_lru.write().retain(|id| id != channel_id);
    }
}

impl Default for ReadReceiptAggregator {
    fn default() -> Self {
        Self::new()
    }
}

/// Type alias for backwards compatibility.
pub type ReadReceiptStream = ReadReceiptAggregator;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_receipt(channel_id: &str, user_id: &str, message_id: &str, timestamp: u64) -> ReadReceipt {
        ReadReceipt {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            last_read_id: message_id.to_string(),
            timestamp,
        }
    }

    #[test]
    fn test_process_receipt() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));

        let last_read = agg.get_last_read("ch-1", "user-1");
        assert_eq!(last_read, Some("msg-100".to_string()));
    }

    #[test]
    fn test_newer_receipt_overwrites() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));
        agg.process(make_receipt("ch-1", "user-1", "msg-200", 2000));

        let last_read = agg.get_last_read("ch-1", "user-1");
        assert_eq!(last_read, Some("msg-200".to_string()));
    }

    #[test]
    fn test_older_receipt_ignored() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-200", 2000));
        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000)); // Older

        let last_read = agg.get_last_read("ch-1", "user-1");
        assert_eq!(last_read, Some("msg-200".to_string())); // Still newer
    }

    #[test]
    fn test_multiple_users() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));
        agg.process(make_receipt("ch-1", "user-2", "msg-50", 900));

        assert_eq!(agg.get_last_read("ch-1", "user-1"), Some("msg-100".to_string()));
        assert_eq!(agg.get_last_read("ch-1", "user-2"), Some("msg-50".to_string()));
    }

    #[test]
    fn test_get_channel_read_states() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));
        agg.process(make_receipt("ch-1", "user-2", "msg-50", 900));

        let states = agg.get_channel_read_states("ch-1");
        assert_eq!(states.len(), 2);
        assert!(states.contains_key("user-1"));
        assert!(states.contains_key("user-2"));
    }

    #[test]
    fn test_get_readers_of_message() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));
        agg.process(make_receipt("ch-1", "user-2", "msg-100", 1100));
        agg.process(make_receipt("ch-1", "user-3", "msg-50", 900));

        let readers = agg.get_readers_of_message("ch-1", "msg-100");
        assert_eq!(readers.len(), 2);
        assert!(readers.contains(&"user-1".to_string()));
        assert!(readers.contains(&"user-2".to_string()));
    }

    #[test]
    fn test_clear() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));
        agg.clear();

        assert!(agg.get_last_read("ch-1", "user-1").is_none());
    }

    #[test]
    fn test_clear_channel() {
        let agg = ReadReceiptAggregator::new();
        let _rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));
        agg.process(make_receipt("ch-2", "user-1", "msg-200", 2000));

        agg.clear_channel("ch-1");

        assert!(agg.get_last_read("ch-1", "user-1").is_none());
        assert!(agg.get_last_read("ch-2", "user-1").is_some());
    }

    #[tokio::test]
    async fn test_broadcast() {
        let agg = ReadReceiptAggregator::new();
        let mut rx = agg.on_read_receipt();

        agg.process(make_receipt("ch-1", "user-1", "msg-100", 1000));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.channel_id, "ch-1");
        assert_eq!(received.user_id, "user-1");
    }
}
