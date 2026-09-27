//! Message state.
//!
//! Message cache, pagination, drafts.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::domain::draft::{Draft, DraftKey};
use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::Message;
use crate::state::store::Store;

/// Maximum number of messages to cache per channel.
/// Older messages are evicted when this limit is exceeded.
const MAX_MESSAGES_PER_CHANNEL: usize = 200;

/// Maximum number of channels to keep in cache.
/// Least recently used channels are evicted when this limit is exceeded.
const MAX_CACHED_CHANNELS: usize = 20;

/// Pagination cursor for loading more messages.
#[derive(Clone, Debug, Default)]
pub struct PaginationCursor {
    /// Whether more messages exist.
    pub has_more: bool,
    /// ID of the oldest message (for fetching older messages).
    pub oldest_message_id: Option<MessageId>,
    /// Timestamp of the oldest message (for cursor-based pagination).
    pub oldest_timestamp: Option<DateTime<Utc>>,
}

impl PaginationCursor {
    /// Create a new pagination cursor.
    #[must_use]
    pub fn new(has_more: bool, oldest_message_id: Option<MessageId>, oldest_timestamp: Option<DateTime<Utc>>) -> Self {
        Self {
            has_more,
            oldest_message_id,
            oldest_timestamp,
        }
    }

    /// Create a cursor indicating no more messages.
    #[must_use]
    pub fn exhausted() -> Self {
        Self {
            has_more: false,
            oldest_message_id: None,
            oldest_timestamp: None,
        }
    }
}

/// Message state data.
///
/// Caches messages per channel, tracks drafts and pagination state.
/// Uses LRU eviction to bound memory usage.
#[derive(Clone, Debug, Default)]
pub struct MessageStateData {
    /// Cached messages per channel.
    pub messages: HashMap<ChannelId, Vec<Message>>,
    /// Drafts keyed by channel + optional thread.
    pub drafts: HashMap<DraftKey, Draft>,
    /// Pagination cursors per channel.
    pub cursors: HashMap<ChannelId, PaginationCursor>,
    /// Channels currently loading messages.
    pub loading_channels: HashSet<ChannelId>,
    /// LRU order for channel eviction (most recent at back).
    channel_lru: VecDeque<ChannelId>,
}

impl MessageStateData {
    /// Touch a channel to mark it as recently used.
    fn touch_channel(&mut self, channel_id: ChannelId) {
        // Remove from current position if present
        self.channel_lru.retain(|id| *id != channel_id);
        // Add to back (most recent)
        self.channel_lru.push_back(channel_id);
    }

    /// Evict least recently used channels if over limit.
    fn evict_lru_channels(&mut self) {
        while self.channel_lru.len() > MAX_CACHED_CHANNELS {
            if let Some(oldest) = self.channel_lru.pop_front() {
                self.messages.remove(&oldest);
                self.cursors.remove(&oldest);
                self.loading_channels.remove(&oldest);
            }
        }
    }

    /// Trim messages in a channel to MAX_MESSAGES_PER_CHANNEL.
    /// Keeps the most recent messages (at the end of the Vec).
    fn trim_channel_messages(&mut self, channel_id: &ChannelId) {
        if let Some(messages) = self.messages.get_mut(channel_id) {
            if messages.len() > MAX_MESSAGES_PER_CHANNEL {
                // Keep the most recent messages (tail of Vec)
                let excess = messages.len() - MAX_MESSAGES_PER_CHANNEL;
                messages.drain(0..excess);
            }
        }
    }
}

/// Reactive message state manager.
///
/// Manages message cache, drafts, and pagination state for all channels.
///
/// # Example
///
/// ```ignore
/// let message_state = MessageState::new();
///
/// // Cache messages for a channel
/// message_state.set_messages(channel_id, messages);
///
/// // Add a new incoming message
/// message_state.append_message(channel_id, new_message);
///
/// // Manage drafts
/// message_state.set_draft(draft);
/// let draft = message_state.get_draft(&draft_key);
/// ```
#[derive(Clone)]
pub struct MessageState {
    store: Store<MessageStateData>,
}

impl MessageState {
    /// Create a new message state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(MessageStateData::default()),
        }
    }

    /// Get the current message state.
    #[must_use]
    pub fn get(&self) -> MessageStateData {
        self.store.get()
    }

    /// Subscribe to message state changes (Arc-wrapped for efficiency).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<MessageStateData>> {
        self.store.subscribe()
    }

    // ========================================================================
    // Message Operations
    // ========================================================================

    /// Get cached messages for a channel.
    #[must_use]
    pub fn get_messages(&self, channel_id: &ChannelId) -> Vec<Message> {
        self.store
            .get()
            .messages
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace all messages for a channel.
    pub fn set_messages(&self, channel_id: ChannelId, messages: Vec<Message>) {
        self.store.update(|state| {
            state.messages.insert(channel_id, messages);
            state.touch_channel(channel_id);
            state.trim_channel_messages(&channel_id);
            state.evict_lru_channels();
        });
    }

    /// Prepend older messages to a channel (for pagination).
    pub fn prepend_messages(&self, channel_id: ChannelId, messages: Vec<Message>) {
        self.store.update(|state| {
            let entry = state.messages.entry(channel_id).or_default();
            let mut combined = messages;
            combined.append(entry);
            *entry = combined;
            state.touch_channel(channel_id);
            // After prepending older messages, trim to keep within limits
            state.trim_channel_messages(&channel_id);
            state.evict_lru_channels();
        });
    }

    /// Append a new message to a channel.
    pub fn append_message(&self, channel_id: ChannelId, message: Message) {
        self.store.update(|state| {
            state.messages.entry(channel_id).or_default().push(message);
            state.touch_channel(channel_id);
            state.trim_channel_messages(&channel_id);
            state.evict_lru_channels();
        });
    }

    /// Update a message in the cache.
    ///
    /// The update function receives a mutable reference to the message.
    /// If the message is not found, this is a no-op.
    pub fn update_message<F>(&self, channel_id: &ChannelId, message_id: &MessageId, f: F)
    where
        F: FnOnce(&mut Message),
    {
        self.store.update(|state| {
            if let Some(messages) = state.messages.get_mut(channel_id) {
                if let Some(message) = messages.iter_mut().find(|m| &m.id == message_id) {
                    f(message);
                }
            }
        });
    }

    /// Remove a message from the cache.
    pub fn remove_message(&self, channel_id: &ChannelId, message_id: &MessageId) {
        self.store.update(|state| {
            if let Some(messages) = state.messages.get_mut(channel_id) {
                messages.retain(|m| &m.id != message_id);
            }
        });
    }

    /// Update a message by Signal-style (sender_id, sent_at_ns) composite key.
    ///
    /// Searches all cached channels for a message matching the composite key.
    /// This is the preferred lookup method per Signal Protocol spec.
    pub fn update_message_by_sender_timestamp<F>(
        &self,
        sender_id: &crate::domain::ids::UserId,
        sent_at_ns: u64,
        f: F,
    ) where
        F: FnOnce(&mut Message),
    {
        self.store.update(|state| {
            for messages in state.messages.values_mut() {
                if let Some(message) = messages.iter_mut().find(|m| {
                    m.sender_id.as_ref() == Some(sender_id) && m.sent_at == sent_at_ns
                }) {
                    f(message);
                    return;
                }
            }
        });
    }

    /// Find which channel contains a message.
    ///
    /// Searches all cached channels for a message with the given ID.
    /// Returns None if the message is not in any cached channel.
    #[must_use]
    pub fn find_channel_for_message(&self, message_id: &MessageId) -> Option<ChannelId> {
        let state = self.store.get();
        for (channel_id, messages) in &state.messages {
            if messages.iter().any(|m| &m.id == message_id) {
                return Some(*channel_id);
            }
        }
        None
    }

    /// Update delivery status for messages matching the given timestamps.
    ///
    /// Searches all cached channels for messages with matching `sent_at` timestamps.
    /// Only upgrades status: "" / "sending" → "sent" or "failed" → "delivered"
    /// → "read" (never downgrades).
    pub fn update_delivery_status_by_timestamps(&self, timestamps: &[u64], new_status: &str) {
        let status_rank = |s: &str| match s {
            "read" => 3,
            "delivered" => 2,
            "sent" | "failed" => 1,
            _ => 0,
        };
        let new_rank = status_rank(new_status);
        let new_status = new_status.to_string();

        self.store.update(|state| {
            for messages in state.messages.values_mut() {
                for msg in messages.iter_mut() {
                    // Server receipt evidence arrives by MessageDeliveryChanged,
                    // keyed by ID. Timestamp-only anonymous events cannot touch it.
                    if matches!(new_status.as_str(), "read" | "delivered")
                        && !matches!(msg.conversation_type,
                            crate::domain::message::ConversationType::AnonDm |
                            crate::domain::message::ConversationType::AnonGroup)
                    {
                        continue;
                    }
                    if timestamps.contains(&msg.sent_at) && status_rank(&msg.delivery_status) < new_rank {
                        msg.delivery_status = new_status.clone();
                    }
                }
            }
        });
    }

    /// Clear all messages for a channel.
    pub fn clear_channel(&self, channel_id: &ChannelId) {
        self.store.update(|state| {
            state.messages.remove(channel_id);
            state.cursors.remove(channel_id);
            state.loading_channels.remove(channel_id);
            state.channel_lru.retain(|id| id != channel_id);
        });
    }

    // ========================================================================
    // Draft Operations
    // ========================================================================

    /// Get a draft by its key.
    #[must_use]
    pub fn get_draft(&self, key: &DraftKey) -> Option<Draft> {
        self.store.get().drafts.get(key).cloned()
    }

    /// Set or update a draft.
    pub fn set_draft(&self, draft: Draft) {
        self.store.update(|state| {
            state.drafts.insert(draft.key.clone(), draft);
        });
    }

    /// Remove a draft and return it if it existed.
    pub fn remove_draft(&self, key: &DraftKey) -> Option<Draft> {
        let mut removed = None;
        self.store.update(|state| {
            removed = state.drafts.remove(key);
        });
        removed
    }

    /// Get all drafts for a specific channel (main channel and all threads).
    #[must_use]
    pub fn get_drafts_for_channel(&self, channel_id: &ChannelId) -> Vec<Draft> {
        self.store
            .get()
            .drafts
            .values()
            .filter(|d| &d.key.channel_id == channel_id)
            .cloned()
            .collect()
    }

    // ========================================================================
    // Pagination Operations
    // ========================================================================

    /// Set the pagination cursor for a channel.
    pub fn set_cursor(&self, channel_id: ChannelId, cursor: PaginationCursor) {
        self.store.update(|state| {
            state.cursors.insert(channel_id, cursor);
        });
    }

    /// Get the pagination cursor for a channel.
    #[must_use]
    pub fn get_cursor(&self, channel_id: &ChannelId) -> Option<PaginationCursor> {
        self.store.get().cursors.get(channel_id).cloned()
    }

    // ========================================================================
    // Loading State
    // ========================================================================

    /// Set the loading state for a channel.
    pub fn set_loading(&self, channel_id: ChannelId, loading: bool) {
        self.store.update(|state| {
            if loading {
                state.loading_channels.insert(channel_id);
            } else {
                state.loading_channels.remove(&channel_id);
            }
        });
    }

    /// Check if a channel is currently loading messages.
    #[must_use]
    pub fn is_loading(&self, channel_id: &ChannelId) -> bool {
        self.store.get().loading_channels.contains(channel_id)
    }
}

impl Default for MessageState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MessageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let data = self.get();
        f.debug_struct("MessageState")
            .field("channel_count", &data.messages.len())
            .field("draft_count", &data.drafts.len())
            .field("loading_channels", &data.loading_channels.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::UserId;
    use crate::domain::message::ConversationType;

    fn create_test_message(channel_id: ChannelId) -> Message {
        Message::new_text(channel_id, ConversationType::Channel, Some(UserId::new()), "Test message".to_string(), 0)
    }

    #[test]
    fn test_message_state_default() {
        let state = MessageState::new();
        let data = state.get();

        assert!(data.messages.is_empty());
        assert!(data.drafts.is_empty());
        assert!(data.cursors.is_empty());
        assert!(data.loading_channels.is_empty());
    }

    #[test]
    fn test_set_and_get_messages() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();
        let msg1 = create_test_message(channel_id);
        let msg2 = create_test_message(channel_id);

        state.set_messages(channel_id, vec![msg1.clone(), msg2.clone()]);

        let messages = state.get_messages(&channel_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, msg1.id);
        assert_eq!(messages[1].id, msg2.id);
    }

    #[test]
    fn test_get_messages_empty_channel() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        let messages = state.get_messages(&channel_id);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_prepend_messages() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        let newer = create_test_message(channel_id);
        let older1 = create_test_message(channel_id);
        let older2 = create_test_message(channel_id);

        state.set_messages(channel_id, vec![newer.clone()]);
        state.prepend_messages(channel_id, vec![older1.clone(), older2.clone()]);

        let messages = state.get_messages(&channel_id);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].id, older1.id);
        assert_eq!(messages[1].id, older2.id);
        assert_eq!(messages[2].id, newer.id);
    }

    #[test]
    fn test_append_message() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        let msg1 = create_test_message(channel_id);
        let msg2 = create_test_message(channel_id);

        state.append_message(channel_id, msg1.clone());
        state.append_message(channel_id, msg2.clone());

        let messages = state.get_messages(&channel_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, msg1.id);
        assert_eq!(messages[1].id, msg2.id);
    }

    #[test]
    fn test_update_message() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();
        let msg = create_test_message(channel_id);
        let msg_id = msg.id;

        state.set_messages(channel_id, vec![msg]);

        state.update_message(&channel_id, &msg_id, |m| {
            m.is_pinned = true;
        });

        let messages = state.get_messages(&channel_id);
        assert!(messages[0].is_pinned);
    }

    #[test]
    fn test_update_message_not_found() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();
        let nonexistent_id = MessageId::new();

        // Should not panic, just no-op
        state.update_message(&channel_id, &nonexistent_id, |m| {
            m.is_pinned = true;
        });
    }

    #[test]
    fn test_remove_message() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();
        let msg1 = create_test_message(channel_id);
        let msg2 = create_test_message(channel_id);
        let msg1_id = msg1.id;

        state.set_messages(channel_id, vec![msg1, msg2.clone()]);
        state.remove_message(&channel_id, &msg1_id);

        let messages = state.get_messages(&channel_id);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, msg2.id);
    }

    #[test]
    fn test_clear_channel() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        state.set_messages(channel_id, vec![create_test_message(channel_id)]);
        state.set_cursor(channel_id, PaginationCursor::new(true, None, None));
        state.set_loading(channel_id, true);

        state.clear_channel(&channel_id);

        assert!(state.get_messages(&channel_id).is_empty());
        assert!(state.get_cursor(&channel_id).is_none());
        assert!(!state.is_loading(&channel_id));
    }

    #[test]
    fn test_draft_operations() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();
        let key = DraftKey::for_channel(channel_id);
        let draft = Draft::new(key.clone(), "Hello world".to_string());

        // Set draft
        state.set_draft(draft.clone());
        let retrieved = state.get_draft(&key);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().content, "Hello world");

        // Remove draft
        let removed = state.remove_draft(&key);
        assert!(removed.is_some());
        assert!(state.get_draft(&key).is_none());
    }

    #[test]
    fn test_get_drafts_for_channel() {
        let state = MessageState::new();
        let channel1 = ChannelId::new();
        let channel2 = ChannelId::new();
        let thread_id = MessageId::new();

        let draft1 = Draft::new(DraftKey::for_channel(channel1), "Draft 1".to_string());
        let draft2 = Draft::new(DraftKey::for_thread(channel1, thread_id), "Draft 2".to_string());
        let draft3 = Draft::new(DraftKey::for_channel(channel2), "Draft 3".to_string());

        state.set_draft(draft1);
        state.set_draft(draft2);
        state.set_draft(draft3);

        let channel1_drafts = state.get_drafts_for_channel(&channel1);
        assert_eq!(channel1_drafts.len(), 2);

        let channel2_drafts = state.get_drafts_for_channel(&channel2);
        assert_eq!(channel2_drafts.len(), 1);
    }

    #[test]
    fn test_pagination_cursor() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();
        let msg_id = MessageId::new();
        let timestamp = Utc::now();

        let cursor = PaginationCursor::new(true, Some(msg_id), Some(timestamp));
        state.set_cursor(channel_id, cursor);

        let retrieved = state.get_cursor(&channel_id);
        assert!(retrieved.is_some());
        let cursor = retrieved.unwrap();
        assert!(cursor.has_more);
        assert_eq!(cursor.oldest_message_id, Some(msg_id));
    }

    #[test]
    fn test_pagination_cursor_exhausted() {
        let cursor = PaginationCursor::exhausted();
        assert!(!cursor.has_more);
        assert!(cursor.oldest_message_id.is_none());
        assert!(cursor.oldest_timestamp.is_none());
    }

    #[test]
    fn test_loading_state() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        assert!(!state.is_loading(&channel_id));

        state.set_loading(channel_id, true);
        assert!(state.is_loading(&channel_id));

        state.set_loading(channel_id, false);
        assert!(!state.is_loading(&channel_id));
    }

    #[tokio::test]
    async fn test_message_state_subscribe() {
        let state = MessageState::new();
        let mut rx = state.subscribe();
        let channel_id = ChannelId::new();
        let msg = create_test_message(channel_id);

        state.append_message(channel_id, msg.clone());

        let received = rx.recv().await.unwrap();
        assert_eq!(received.messages.get(&channel_id).unwrap().len(), 1);
    }

    #[test]
    fn test_message_state_clone_shares_state() {
        let state1 = MessageState::new();
        let state2 = state1.clone();
        let channel_id = ChannelId::new();

        state1.append_message(channel_id, create_test_message(channel_id));
        assert_eq!(state2.get_messages(&channel_id).len(), 1);
    }
}
