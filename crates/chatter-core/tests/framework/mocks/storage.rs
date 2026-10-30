//! In-memory storage for testing without SQLite.

use crate::framework::fixtures::{TestChannel, TestMessage, TestUser};
use std::collections::HashMap;
use std::sync::RwLock;

// ============================================================================
// In-Memory Storage
// ============================================================================

/// In-memory storage implementation for testing.
pub struct InMemoryStorage {
    messages: RwLock<HashMap<String, TestMessage>>,
    channels: RwLock<HashMap<String, TestChannel>>,
    users: RwLock<HashMap<String, TestUser>>,
    settings: RwLock<HashMap<String, String>>,
}

impl InMemoryStorage {
    /// Create a new empty in-memory storage.
    pub fn new() -> Self {
        Self {
            messages: RwLock::new(HashMap::new()),
            channels: RwLock::new(HashMap::new()),
            users: RwLock::new(HashMap::new()),
            settings: RwLock::new(HashMap::new()),
        }
    }

    /// Save a message.
    pub fn save_message(&self, message: TestMessage) {
        self.messages
            .write()
            .unwrap()
            .insert(message.id.clone(), message);
    }

    /// Get a message by ID.
    pub fn get_message(&self, id: &str) -> Option<TestMessage> {
        self.messages.read().unwrap().get(id).cloned()
    }

    /// Get messages by channel, sorted by timestamp descending.
    pub fn get_messages_by_channel(
        &self,
        channel_id: &str,
        limit: usize,
        before: Option<&str>,
    ) -> Vec<TestMessage> {
        let messages = self.messages.read().unwrap();
        let before_timestamp = before
            .and_then(|id| messages.get(id))
            .map(|m| m.timestamp)
            .unwrap_or(i64::MAX);

        let mut result: Vec<_> = messages
            .values()
            .filter(|m| m.channel_id == channel_id && m.timestamp < before_timestamp)
            .cloned()
            .collect();

        result.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        result.truncate(limit);
        result
    }

    /// Delete a message.
    pub fn delete_message(&self, id: &str) -> bool {
        self.messages.write().unwrap().remove(id).is_some()
    }

    /// Save a channel.
    pub fn save_channel(&self, channel: TestChannel) {
        self.channels
            .write()
            .unwrap()
            .insert(channel.id.clone(), channel);
    }

    /// Get a channel by ID.
    pub fn get_channel(&self, id: &str) -> Option<TestChannel> {
        self.channels.read().unwrap().get(id).cloned()
    }

    /// Get all channels.
    pub fn get_all_channels(&self) -> Vec<TestChannel> {
        self.channels.read().unwrap().values().cloned().collect()
    }

    /// Delete a channel.
    pub fn delete_channel(&self, id: &str) -> bool {
        self.channels.write().unwrap().remove(id).is_some()
    }

    /// Clear all data.
    pub fn clear(&self) {
        self.messages.write().unwrap().clear();
        self.channels.write().unwrap().clear();
        self.users.write().unwrap().clear();
        self.settings.write().unwrap().clear();
    }

    /// Get message count.
    pub fn message_count(&self) -> usize {
        self.messages.read().unwrap().len()
    }

    /// Get channel count.
    pub fn channel_count(&self) -> usize {
        self.channels.read().unwrap().len()
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}
