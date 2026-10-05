//! Message state.
//!
//! Message cache, pagination, drafts.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};

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

impl PaginationCursor {}

/// Message state data.
///
/// Caches messages per channel and tracks pagination state.
/// Uses LRU eviction to bound memory usage.
#[derive(Clone, Debug, Default)]
pub struct MessageStateData {
    /// Cached messages per channel.
    pub messages: HashMap<ChannelId, Vec<Message>>,
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
                        && !matches!(
                            msg.conversation_type,
                            crate::domain::message::ConversationType::AnonDm
                                | crate::domain::message::ConversationType::AnonGroup
                        )
                    {
                        continue;
                    }
                    if timestamps.contains(&msg.sent_at)
                        && status_rank(&msg.delivery_status) < new_rank
                    {
                        msg.delivery_status = new_status.clone();
                    }
                }
            }
        });
    }

    // ========================================================================
    // Pagination Operations
    // ========================================================================

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
        Message::new_text(
            channel_id,
            ConversationType::Channel,
            Some(UserId::new()),
            "Test message".to_string(),
            0,
        )
    }

    #[test]
    fn test_message_state_default() {
        let state = MessageState::new();
        let data = state.get();

        assert!(data.messages.is_empty());
        assert!(data.cursors.is_empty());
        assert!(data.loading_channels.is_empty());
    }

    #[test]
    fn test_get_messages_empty_channel() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        let messages = state.get_messages(&channel_id);
        assert!(messages.is_empty());
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
    fn test_loading_state() {
        let state = MessageState::new();
        let channel_id = ChannelId::new();

        assert!(!state.is_loading(&channel_id));

        state.set_loading(channel_id, true);
        assert!(state.is_loading(&channel_id));

        state.set_loading(channel_id, false);
        assert!(!state.is_loading(&channel_id));
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
