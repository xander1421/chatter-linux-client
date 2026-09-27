//! Reaction stream handler.
//!
//! Provides reaction events grouped by message for efficient UI updates.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::proto::chat_v1::ReactionUpdate;

/// Capacity for reaction event channel.
const CHANNEL_CAPACITY: usize = 128;

/// Maximum messages to track reactions for.
const CACHE_CAPACITY: usize = 500;

/// Maximum reactions to track per message.
/// Prevents unbounded growth from reaction spam.
const MAX_REACTIONS_PER_MESSAGE: usize = 100;

/// Reaction event with user context.
///
/// Wraps the proto ReactionUpdate with additional context
/// needed for UI display (like user ID from auth context).
#[derive(Clone, Debug)]
pub struct ReactionEvent {
    /// The underlying reaction update.
    pub update: ReactionUpdate,
    /// User who made the reaction (from message context).
    pub user_id: String,
}

/// Build a cache key from Signal-style (author, timestamp) tuple.
pub fn reaction_cache_key(target_author_aci: &str, target_sent_timestamp: u64) -> String {
    format!("{}:{}", target_author_aci, target_sent_timestamp)
}

/// Reaction stream handler with per-message aggregation.
///
/// Tracks recent reaction changes and provides efficient
/// queries for UI components.
pub struct ReactionAggregator {
    /// Recent reaction events keyed by (target_author_aci:target_sent_timestamp).
    recent: Arc<RwLock<ReactionCache>>,

    /// Broadcast for reaction added events.
    added: broadcast::Sender<ReactionEvent>,
    /// Broadcast for reaction removed events.
    removed: broadcast::Sender<ReactionEvent>,
}

/// LRU cache for recent reactions with per-message limits.
struct ReactionCache {
    /// Map of cache key -> list of recent reaction events.
    entries: HashMap<String, Vec<ReactionEvent>>,
    /// LRU order of cache keys for eviction (most recent at back).
    lru: VecDeque<String>,
    capacity: usize,
}

impl ReactionCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn add(&mut self, message_id: String, reaction: ReactionEvent) {
        // Update LRU order
        self.lru.retain(|id| id != &message_id);
        self.lru.push_back(message_id.clone());

        // Add to entries with per-message limit
        let reactions = self.entries.entry(message_id).or_default();
        reactions.push(reaction);

        // Enforce per-message limit (keep newest)
        if reactions.len() > MAX_REACTIONS_PER_MESSAGE {
            let excess = reactions.len() - MAX_REACTIONS_PER_MESSAGE;
            reactions.drain(0..excess);
        }

        // Evict oldest messages if over capacity
        while self.lru.len() > self.capacity {
            if let Some(old_id) = self.lru.pop_front() {
                self.entries.remove(&old_id);
            }
        }
    }

    fn get(&self, message_id: &str) -> Option<&Vec<ReactionEvent>> {
        self.entries.get(message_id)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }
}

impl ReactionAggregator {
    /// Create a new ReactionAggregator.
    pub fn new() -> Self {
        Self {
            recent: Arc::new(RwLock::new(ReactionCache::new(CACHE_CAPACITY))),
            added: broadcast::channel(CHANNEL_CAPACITY).0,
            removed: broadcast::channel(CHANNEL_CAPACITY).0,
        }
    }

    /// Subscribe to reaction added events.
    pub fn on_reaction_added(&self) -> broadcast::Receiver<ReactionEvent> {
        self.added.subscribe()
    }

    /// Subscribe to reaction removed events.
    pub fn on_reaction_removed(&self) -> broadcast::Receiver<ReactionEvent> {
        self.removed.subscribe()
    }

    /// Process a reaction update from a stream.
    ///
    /// The user_id comes from the stream message context (e.g., envelope sender).
    pub fn process(&self, update: ReactionUpdate, user_id: String) {
        let event = ReactionEvent {
            update: update.clone(),
            user_id,
        };

        if update.add {
            self.process_added(event);
        } else {
            self.process_removed(event);
        }
    }

    /// Process a reaction added event.
    pub fn process_added(&self, event: ReactionEvent) {
        self.recent
            .write()
            .add(
                format!("{}:{}", event.update.target_author_aci, event.update.target_sent_timestamp),
                event.clone(),
            );
        let _ = self.added.send(event);
    }

    /// Process a reaction removed event.
    pub fn process_removed(&self, event: ReactionEvent) {
        let _ = self.removed.send(event);
    }

    /// Get recent reactions for a message by cache key.
    ///
    /// Use `reaction_cache_key(author_aci, sent_timestamp)` to build the key.
    pub fn get_recent_reactions(&self, key: &str) -> Vec<ReactionEvent> {
        self.recent
            .read()
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Get the number of messages with cached reactions.
    pub fn cache_size(&self) -> usize {
        self.recent.read().len()
    }

    /// Clear the reaction cache.
    pub fn clear(&self) {
        self.recent.write().clear();
    }
}

impl Default for ReactionAggregator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_AUTHOR: &str = "550e8400-e29b-41d4-a716-446655440000";
    const TEST_TIMESTAMP: u64 = 1704067200_000_000_000;

    fn make_update(target_author_aci: &str, target_sent_timestamp: u64, emoji: &str, add: bool) -> ReactionUpdate {
        ReactionUpdate {
            target_author_aci: target_author_aci.to_string(),
            target_sent_timestamp,
            emoji: emoji.to_string(),
            add,
            proof_of_work: vec![],
            reactor_id: TEST_AUTHOR.to_string(),
        }
    }

    fn make_event(target_author_aci: &str, target_sent_timestamp: u64, user_id: &str, emoji: &str) -> ReactionEvent {
        ReactionEvent {
            update: make_update(target_author_aci, target_sent_timestamp, emoji, true),
            user_id: user_id.to_string(),
        }
    }

    #[test]
    fn test_process_added() {
        let agg = ReactionAggregator::new();
        let _rx = agg.on_reaction_added();

        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", true), "user-1".to_string());

        let key = reaction_cache_key(TEST_AUTHOR, TEST_TIMESTAMP);
        let reactions = agg.get_recent_reactions(&key);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].update.emoji, "\u{1f44d}");
        assert_eq!(reactions[0].user_id, "user-1");
    }

    #[test]
    fn test_multiple_reactions_same_message() {
        let agg = ReactionAggregator::new();
        let _rx = agg.on_reaction_added();

        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", true), "user-1".to_string());
        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{2764}", true), "user-2".to_string());

        let key = reaction_cache_key(TEST_AUTHOR, TEST_TIMESTAMP);
        let reactions = agg.get_recent_reactions(&key);
        assert_eq!(reactions.len(), 2);
    }

    #[tokio::test]
    async fn test_broadcast_added() {
        let agg = ReactionAggregator::new();
        let mut rx = agg.on_reaction_added();

        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", true), "user-1".to_string());

        let received = rx.try_recv().unwrap();
        assert_eq!(received.update.target_author_aci, TEST_AUTHOR);
        assert_eq!(received.update.target_sent_timestamp, TEST_TIMESTAMP);
        assert_eq!(received.update.emoji, "\u{1f44d}");
        assert_eq!(received.user_id, "user-1");
    }

    #[tokio::test]
    async fn test_broadcast_removed() {
        let agg = ReactionAggregator::new();
        let mut rx = agg.on_reaction_removed();

        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", false), "user-1".to_string());

        let received = rx.try_recv().unwrap();
        assert_eq!(received.update.target_author_aci, TEST_AUTHOR);
    }

    #[test]
    fn test_cache_eviction() {
        let mut cache = ReactionCache::new(2);

        let key1 = reaction_cache_key("author-1", 100);
        let key2 = reaction_cache_key("author-2", 200);
        let key3 = reaction_cache_key("author-3", 300);

        cache.add(key1.clone(), make_event("author-1", 100, "u", "\u{1f44d}"));
        cache.add(key2.clone(), make_event("author-2", 200, "u", "\u{1f44d}"));
        cache.add(key3.clone(), make_event("author-3", 300, "u", "\u{1f44d}"));

        // key1 should be evicted
        assert!(cache.get(&key1).is_none());
        assert!(cache.get(&key2).is_some());
        assert!(cache.get(&key3).is_some());
    }

    #[test]
    fn test_clear() {
        let agg = ReactionAggregator::new();
        let _rx = agg.on_reaction_added();

        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", true), "user-1".to_string());
        assert_eq!(agg.cache_size(), 1);

        agg.clear();
        assert_eq!(agg.cache_size(), 0);
    }

    #[test]
    fn test_process_routes_correctly() {
        let agg = ReactionAggregator::new();
        let mut add_rx = agg.on_reaction_added();
        let mut rem_rx = agg.on_reaction_removed();

        // Process an add
        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", true), "user-1".to_string());
        assert!(add_rx.try_recv().is_ok());
        assert!(rem_rx.try_recv().is_err());

        // Process a remove
        agg.process(make_update(TEST_AUTHOR, TEST_TIMESTAMP, "\u{1f44d}", false), "user-1".to_string());
        assert!(rem_rx.try_recv().is_ok());
    }
}
