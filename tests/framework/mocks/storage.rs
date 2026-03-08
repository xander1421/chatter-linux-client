//! Mock storage implementations.
//!
//! Provides in-memory storage for testing without SQLite.
//! Note: Uses owned types instead of references for mockall compatibility.

use mockall::automock;
use std::collections::HashMap;
use std::sync::RwLock;
use crate::framework::fixtures::{TestMessage, TestChannel, TestUser};
use super::services::{MockError, MockResult};

// ============================================================================
// Storage Trait
// ============================================================================

/// Mockable storage trait.
#[automock]
pub trait StorageTrait: Send + Sync {
    // Messages
    fn save_message(
        &self,
        message: TestMessage,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn get_message(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<Option<TestMessage>>> + Send;

    fn get_messages_by_channel(
        &self,
        channel_id: String,
        limit: u32,
        before: Option<String>,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestMessage>>> + Send;

    fn delete_message(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    // Channels
    fn save_channel(
        &self,
        channel: TestChannel,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn get_channel(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<Option<TestChannel>>> + Send;

    fn get_all_channels(
        &self,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestChannel>>> + Send;

    fn delete_channel(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    // Users
    fn save_user(
        &self,
        user: TestUser,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn get_user(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<Option<TestUser>>> + Send;

    // Settings
    fn get_setting(
        &self,
        key: String,
    ) -> impl std::future::Future<Output = MockResult<Option<String>>> + Send;

    fn set_setting(
        &self,
        key: String,
        value: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}

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
        self.messages.write().unwrap().insert(message.id.clone(), message);
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
        self.channels.write().unwrap().insert(channel.id.clone(), channel);
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

    /// Save a user.
    pub fn save_user(&self, user: TestUser) {
        self.users.write().unwrap().insert(user.user_id.clone(), user);
    }

    /// Get a user by ID.
    pub fn get_user(&self, id: &str) -> Option<TestUser> {
        self.users.read().unwrap().get(id).cloned()
    }

    /// Get a setting.
    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.settings.read().unwrap().get(key).cloned()
    }

    /// Set a setting.
    pub fn set_setting(&self, key: &str, value: &str) {
        self.settings.write().unwrap().insert(key.to_string(), value.to_string());
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

    /// Get user count.
    pub fn user_count(&self) -> usize {
        self.users.read().unwrap().len()
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Repository Traits
// ============================================================================

/// Mockable message repository.
#[automock]
pub trait MessageRepositoryTrait: Send + Sync {
    fn insert(
        &self,
        message: TestMessage,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn find_by_id(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<Option<TestMessage>>> + Send;

    fn find_by_channel(
        &self,
        channel_id: String,
        limit: u32,
        offset: u32,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestMessage>>> + Send;

    fn update(
        &self,
        message: TestMessage,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn delete(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn count_by_channel(
        &self,
        channel_id: String,
    ) -> impl std::future::Future<Output = MockResult<u64>> + Send;
}

/// Mockable channel repository.
#[automock]
pub trait ChannelRepositoryTrait: Send + Sync {
    fn insert(
        &self,
        channel: TestChannel,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn find_by_id(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<Option<TestChannel>>> + Send;

    fn find_all(
        &self,
    ) -> impl std::future::Future<Output = MockResult<Vec<TestChannel>>> + Send;

    fn update(
        &self,
        channel: TestChannel,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;

    fn delete(
        &self,
        id: String,
    ) -> impl std::future::Future<Output = MockResult<()>> + Send;
}
