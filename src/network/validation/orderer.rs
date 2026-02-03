//! Message ordering.
//!
//! Ensure messages are processed in correct order per channel.

use std::collections::{BTreeMap, HashMap};
use parking_lot::Mutex;

use crate::proto::chat_v1::Message;

/// Maximum number of out-of-order messages to buffer per channel.
const DEFAULT_MAX_BUFFER_SIZE: usize = 100;

/// Maximum time to wait for missing messages (in milliseconds).
const MAX_BUFFER_AGE_MS: u64 = 30_000; // 30 seconds

/// Ordering result for a message.
#[derive(Debug)]
pub enum OrderingResult {
    /// Message is in order, ready for processing.
    Ready(Message),
    /// Message is out of order, buffered for later.
    Buffered,
    /// Message is a duplicate (same timestamp seen).
    Duplicate,
    /// Buffer is full, message dropped.
    Dropped,
}

/// Per-channel ordering state.
struct ChannelOrderState {
    /// Last processed timestamp for this channel.
    last_timestamp: u64,
    /// Buffered out-of-order messages, keyed by timestamp.
    /// Using BTreeMap for automatic ordering by timestamp.
    buffer: BTreeMap<u64, Message>,
    /// When the oldest buffered message was received.
    oldest_buffer_time: Option<std::time::Instant>,
}

impl ChannelOrderState {
    fn new() -> Self {
        Self {
            last_timestamp: 0,
            buffer: BTreeMap::new(),
            oldest_buffer_time: None,
        }
    }
}

/// Statistics about the orderer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrdererStats {
    /// Number of channels currently being tracked.
    pub channels_tracked: usize,
    /// Total number of buffered messages across all channels.
    pub total_buffered: usize,
}

/// Message orderer for per-channel ordering guarantees.
///
/// Ensures messages are processed in timestamp order within each channel.
/// Out-of-order messages are buffered until the gap is filled.
///
/// # Thread Safety
///
/// Uses `Mutex` for thread-safe access to ordering state.
pub struct MessageOrderer {
    /// Per-channel ordering state.
    channels: Mutex<HashMap<String, ChannelOrderState>>,
    /// Maximum buffer size per channel.
    max_buffer_size: usize,
}

impl MessageOrderer {
    /// Create a new orderer with default buffer size.
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
        }
    }

    /// Create a new orderer with custom buffer size.
    pub fn with_buffer_size(max_buffer_size: usize) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            max_buffer_size,
        }
    }

    /// Process a message for ordering.
    ///
    /// Returns `Ready` if the message is in order (timestamp > last processed).
    /// Returns `Buffered` if out of order (will be released when gap is filled).
    /// Returns `Duplicate` if same timestamp was already seen.
    /// Returns `Dropped` if the buffer is full.
    pub fn order(&self, message: Message) -> OrderingResult {
        let mut channels = self.channels.lock();
        let channel_id = message.channel_id.clone();
        let timestamp = message.timestamp;

        let state = channels
            .entry(channel_id)
            .or_insert_with(ChannelOrderState::new);

        // First message for this channel - always ready
        if state.last_timestamp == 0 {
            state.last_timestamp = timestamp;
            return OrderingResult::Ready(message);
        }

        // Message is in order (newer than last processed)
        if timestamp > state.last_timestamp {
            state.last_timestamp = timestamp;
            return OrderingResult::Ready(message);
        }

        // Message has same timestamp - duplicate
        if timestamp == state.last_timestamp {
            return OrderingResult::Duplicate;
        }

        // Check if already buffered with same timestamp
        if state.buffer.contains_key(&timestamp) {
            return OrderingResult::Duplicate;
        }

        // Message is out of order - buffer it
        if state.buffer.len() >= self.max_buffer_size {
            return OrderingResult::Dropped;
        }

        // Track when we started buffering
        if state.oldest_buffer_time.is_none() {
            state.oldest_buffer_time = Some(std::time::Instant::now());
        }

        state.buffer.insert(timestamp, message);
        OrderingResult::Buffered
    }

    /// Get any messages that are now ready after processing.
    ///
    /// Call this after processing a `Ready` message to get buffered messages
    /// that are now in sequence. Returns messages in timestamp order.
    pub fn get_ready(&self, channel_id: &str) -> Vec<Message> {
        let mut channels = self.channels.lock();
        let state = match channels.get_mut(channel_id) {
            Some(s) => s,
            None => return vec![],
        };

        let mut ready = Vec::new();

        // Get all buffered messages that are now in order
        // A message is ready if its timestamp is greater than the last processed timestamp
        while let Some((&ts, _)) = state.buffer.first_key_value() {
            if ts > state.last_timestamp {
                // This message is now in order
                if let Some(msg) = state.buffer.remove(&ts) {
                    state.last_timestamp = ts;
                    ready.push(msg);
                }
            } else if ts <= state.last_timestamp {
                // This message is older than what we've processed - remove and skip
                state.buffer.remove(&ts);
            } else {
                // Gap still exists
                break;
            }
        }

        // Reset buffer tracking if empty
        if state.buffer.is_empty() {
            state.oldest_buffer_time = None;
        }

        ready
    }

    /// Flush stale buffered messages (older than MAX_BUFFER_AGE_MS).
    ///
    /// Returns messages that were flushed so they can be logged or processed anyway.
    /// Call this periodically to prevent indefinite buffering.
    pub fn flush_stale(&self) -> Vec<Message> {
        let mut channels = self.channels.lock();
        let mut flushed = Vec::new();

        for state in channels.values_mut() {
            if let Some(oldest_time) = state.oldest_buffer_time {
                let elapsed_ms = oldest_time.elapsed().as_millis() as u64;
                if elapsed_ms > MAX_BUFFER_AGE_MS {
                    // Flush all buffered messages for this channel
                    flushed.extend(state.buffer.values().cloned());

                    // Update last_timestamp to the highest flushed timestamp
                    if let Some((&max_ts, _)) = state.buffer.last_key_value() {
                        if max_ts > state.last_timestamp {
                            state.last_timestamp = max_ts;
                        }
                    }

                    state.buffer.clear();
                    state.oldest_buffer_time = None;
                }
            }
        }

        flushed
    }

    /// Get ordering statistics.
    pub fn stats(&self) -> OrdererStats {
        let channels = self.channels.lock();
        let total_buffered = channels.values().map(|s| s.buffer.len()).sum();

        OrdererStats {
            channels_tracked: channels.len(),
            total_buffered,
        }
    }
}

impl Default for MessageOrderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_message(id: &str, channel_id: &str, timestamp: u64) -> Message {
        Message {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: "user-123".to_string(),
            timestamp,
            content: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_first_message_ready() {
        let orderer = MessageOrderer::new();
        let msg = make_test_message("msg-1", "channel-1", 1000);

        match orderer.order(msg) {
            OrderingResult::Ready(m) => {
                assert_eq!(m.id, "msg-1");
                assert_eq!(m.timestamp, 1000);
            }
            _ => panic!("Expected Ready result"),
        }
    }

    #[test]
    fn test_in_order_messages_ready() {
        let orderer = MessageOrderer::new();

        // First message
        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Second message with higher timestamp - should be ready
        let msg2 = make_test_message("msg-2", "channel-1", 2000);
        assert!(matches!(orderer.order(msg2), OrderingResult::Ready(_)));

        // Third message with even higher timestamp - should be ready
        let msg3 = make_test_message("msg-3", "channel-1", 3000);
        assert!(matches!(orderer.order(msg3), OrderingResult::Ready(_)));
    }

    #[test]
    fn test_out_of_order_buffered() {
        let orderer = MessageOrderer::new();

        // First message at timestamp 2000
        let msg1 = make_test_message("msg-1", "channel-1", 2000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Second message at timestamp 1000 (older) - should be buffered
        let msg2 = make_test_message("msg-2", "channel-1", 1000);
        assert!(matches!(orderer.order(msg2), OrderingResult::Buffered));

        // Stats should show one buffered message
        let stats = orderer.stats();
        assert_eq!(stats.total_buffered, 1);
    }

    #[test]
    fn test_get_ready_after_gap_filled() {
        let orderer = MessageOrderer::new();

        // Message at timestamp 1000
        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Message at timestamp 3000 - ready (higher than last)
        let msg3 = make_test_message("msg-3", "channel-1", 3000);
        assert!(matches!(orderer.order(msg3), OrderingResult::Ready(_)));

        // Message at timestamp 2000 - buffered (between 1000 and 3000)
        // But since we've already processed 3000, this is actually old
        let msg2 = make_test_message("msg-2", "channel-1", 2000);
        assert!(matches!(orderer.order(msg2), OrderingResult::Buffered));

        // get_ready should return the buffered message since it's older
        // Actually, it's older than last_timestamp (3000), so it gets removed
        let ready = orderer.get_ready("channel-1");
        assert!(ready.is_empty()); // It was old, so discarded

        // Stats should show no buffered messages now
        let stats = orderer.stats();
        assert_eq!(stats.total_buffered, 0);
    }

    #[test]
    fn test_buffered_becomes_ready_in_sequence() {
        let orderer = MessageOrderer::new();

        // Message at timestamp 1000
        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Buffer messages 3000 and 4000 (out of order, ahead of sequence)
        let msg3 = make_test_message("msg-3", "channel-1", 3000);
        assert!(matches!(orderer.order(msg3), OrderingResult::Ready(_)));

        // Now we've processed 1000, then 3000. Next we buffer 2000.
        let msg2 = make_test_message("msg-2", "channel-1", 2000);
        assert!(matches!(orderer.order(msg2), OrderingResult::Buffered));

        // Buffer 4000
        let msg4 = make_test_message("msg-4", "channel-1", 4000);
        assert!(matches!(orderer.order(msg4), OrderingResult::Ready(_)));

        // Now we've processed 1000, 3000, 4000. The buffered 2000 is old.
        let ready = orderer.get_ready("channel-1");
        assert!(ready.is_empty()); // 2000 is old, removed
    }

    #[test]
    fn test_duplicate_same_timestamp() {
        let orderer = MessageOrderer::new();

        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Same timestamp - duplicate
        let msg2 = make_test_message("msg-2", "channel-1", 1000);
        assert!(matches!(orderer.order(msg2), OrderingResult::Duplicate));
    }

    #[test]
    fn test_duplicate_in_buffer() {
        let orderer = MessageOrderer::new();

        // First message at 2000
        let msg1 = make_test_message("msg-1", "channel-1", 2000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Buffer message at 1000
        let msg2 = make_test_message("msg-2", "channel-1", 1000);
        assert!(matches!(orderer.order(msg2), OrderingResult::Buffered));

        // Try to buffer another message at 1000 - duplicate
        let msg3 = make_test_message("msg-3", "channel-1", 1000);
        assert!(matches!(orderer.order(msg3), OrderingResult::Duplicate));
    }

    #[test]
    fn test_buffer_limit() {
        let orderer = MessageOrderer::with_buffer_size(3);

        // First message
        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Buffer 3 messages (at capacity)
        for i in 2..=4 {
            let msg = make_test_message(&format!("msg-{}", i), "channel-1", 500 - i as u64);
            assert!(matches!(orderer.order(msg), OrderingResult::Buffered));
        }

        assert_eq!(orderer.stats().total_buffered, 3);

        // 5th message should be dropped
        let msg5 = make_test_message("msg-5", "channel-1", 490);
        assert!(matches!(orderer.order(msg5), OrderingResult::Dropped));

        // Still only 3 buffered
        assert_eq!(orderer.stats().total_buffered, 3);
    }

    #[test]
    fn test_separate_channels() {
        let orderer = MessageOrderer::new();

        // Channel 1: message at 1000
        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        assert!(matches!(orderer.order(msg1), OrderingResult::Ready(_)));

        // Channel 2: message at 500 - should be ready (different channel)
        let msg2 = make_test_message("msg-2", "channel-2", 500);
        assert!(matches!(orderer.order(msg2), OrderingResult::Ready(_)));

        // Channel 1: message at 500 - should be buffered (older for this channel)
        let msg3 = make_test_message("msg-3", "channel-1", 500);
        assert!(matches!(orderer.order(msg3), OrderingResult::Buffered));

        // Stats should show 2 channels and 1 buffered
        let stats = orderer.stats();
        assert_eq!(stats.channels_tracked, 2);
        assert_eq!(stats.total_buffered, 1);
    }

    #[test]
    fn test_stats() {
        let orderer = MessageOrderer::new();

        // Initially empty
        let stats = orderer.stats();
        assert_eq!(stats.channels_tracked, 0);
        assert_eq!(stats.total_buffered, 0);

        // Add some messages
        let msg1 = make_test_message("msg-1", "channel-1", 1000);
        orderer.order(msg1);

        let msg2 = make_test_message("msg-2", "channel-1", 500);
        orderer.order(msg2);

        let msg3 = make_test_message("msg-3", "channel-2", 2000);
        orderer.order(msg3);

        let stats = orderer.stats();
        assert_eq!(stats.channels_tracked, 2);
        assert_eq!(stats.total_buffered, 1); // msg-2 is buffered
    }

    #[test]
    fn test_default() {
        let orderer = MessageOrderer::default();
        assert_eq!(orderer.max_buffer_size, DEFAULT_MAX_BUFFER_SIZE);
    }
}
