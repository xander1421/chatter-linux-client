//! Mock network implementations.
//!
//! Provides a mock gRPC server and network simulation.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use crate::framework::fixtures::{TestMessage, TestChannel, TestUser};
use super::services::{MockError, MockResult};

// ============================================================================
// Network Simulation
// ============================================================================

/// Network condition simulation for testing.
#[derive(Clone)]
pub struct NetworkSimulator {
    /// Simulated latency.
    latency: Option<Duration>,
    /// Probability of request failure (0.0 - 1.0).
    failure_rate: f32,
    /// Whether network is offline.
    offline: bool,
    /// Bandwidth limit in bytes per second.
    bandwidth_limit: Option<u64>,
}

impl NetworkSimulator {
    /// Create a perfect network (no latency, no failures).
    pub fn perfect() -> Self {
        Self {
            latency: None,
            failure_rate: 0.0,
            offline: false,
            bandwidth_limit: None,
        }
    }

    /// Create a slow network.
    pub fn slow(latency: Duration) -> Self {
        Self {
            latency: Some(latency),
            failure_rate: 0.0,
            offline: false,
            bandwidth_limit: None,
        }
    }

    /// Create a flaky network.
    pub fn flaky(failure_rate: f32) -> Self {
        Self {
            latency: None,
            failure_rate: failure_rate.clamp(0.0, 1.0),
            offline: false,
            bandwidth_limit: None,
        }
    }

    /// Create an offline network.
    pub fn offline() -> Self {
        Self {
            latency: None,
            failure_rate: 1.0,
            offline: true,
            bandwidth_limit: None,
        }
    }

    /// Set latency.
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = Some(latency);
        self
    }

    /// Set failure rate.
    pub fn with_failure_rate(mut self, rate: f32) -> Self {
        self.failure_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Set bandwidth limit.
    pub fn with_bandwidth_limit(mut self, bytes_per_sec: u64) -> Self {
        self.bandwidth_limit = Some(bytes_per_sec);
        self
    }

    /// Check if request should fail.
    pub fn should_fail(&self) -> bool {
        if self.offline {
            return true;
        }
        rand::random::<f32>() < self.failure_rate
    }

    /// Simulate network delay.
    pub async fn simulate_latency(&self) {
        if let Some(latency) = self.latency {
            tokio::time::sleep(latency).await;
        }
    }

    /// Simulate bandwidth-limited transfer.
    pub async fn simulate_transfer(&self, bytes: usize) {
        if let Some(limit) = self.bandwidth_limit {
            let duration = Duration::from_secs_f64(bytes as f64 / limit as f64);
            tokio::time::sleep(duration).await;
        }
    }

    /// Execute with network conditions applied.
    pub async fn execute<T, F, Fut>(&self, f: F) -> MockResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = MockResult<T>>,
    {
        if self.should_fail() {
            return Err(MockError::NetworkError("Simulated network failure".into()));
        }

        self.simulate_latency().await;
        f().await
    }
}

impl Default for NetworkSimulator {
    fn default() -> Self {
        Self::perfect()
    }
}

// ============================================================================
// Mock gRPC Server
// ============================================================================

/// In-process mock gRPC server for testing.
pub struct MockGrpcServer {
    users: RwLock<HashMap<String, TestUser>>,
    channels: RwLock<HashMap<String, TestChannel>>,
    messages: RwLock<HashMap<String, TestMessage>>,
    sessions: RwLock<HashMap<String, (String, u32)>>, // token -> (user_id, device_id)
    network: NetworkSimulator,
}

impl MockGrpcServer {
    /// Create a new mock server.
    pub fn new() -> Self {
        Self {
            users: RwLock::new(HashMap::new()),
            channels: RwLock::new(HashMap::new()),
            messages: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            network: NetworkSimulator::perfect(),
        }
    }

    /// Set network conditions.
    pub fn with_network(mut self, network: NetworkSimulator) -> Self {
        self.network = network;
        self
    }

    /// Register a user.
    pub fn register_user(&self, user: TestUser) -> String {
        let user_id = user.user_id.clone();
        self.users.write().unwrap().insert(user_id.clone(), user);
        user_id
    }

    /// Get a user.
    pub fn get_user(&self, user_id: &str) -> Option<TestUser> {
        self.users.read().unwrap().get(user_id).cloned()
    }

    /// Create a channel.
    pub fn create_channel(&self, channel: TestChannel) -> String {
        let channel_id = channel.id.clone();
        self.channels.write().unwrap().insert(channel_id.clone(), channel);
        channel_id
    }

    /// Get a channel.
    pub fn get_channel(&self, channel_id: &str) -> Option<TestChannel> {
        self.channels.read().unwrap().get(channel_id).cloned()
    }

    /// Store a message.
    pub fn store_message(&self, message: TestMessage) -> String {
        let message_id = message.id.clone();
        self.messages.write().unwrap().insert(message_id.clone(), message);
        message_id
    }

    /// Get messages for a channel.
    pub fn get_messages(&self, channel_id: &str, limit: usize) -> Vec<TestMessage> {
        let messages = self.messages.read().unwrap();
        let mut result: Vec<_> = messages
            .values()
            .filter(|m| m.channel_id == channel_id)
            .cloned()
            .collect();
        result.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        result.truncate(limit);
        result
    }

    /// Create a session token.
    pub fn create_session(&self, user_id: &str, device_id: u32) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.sessions.write().unwrap().insert(
            token.clone(),
            (user_id.to_string(), device_id),
        );
        token
    }

    /// Validate session token.
    pub fn validate_session(&self, token: &str) -> Option<(String, u32)> {
        self.sessions.read().unwrap().get(token).cloned()
    }

    /// Clear all data.
    pub fn clear(&self) {
        self.users.write().unwrap().clear();
        self.channels.write().unwrap().clear();
        self.messages.write().unwrap().clear();
        self.sessions.write().unwrap().clear();
    }
}

impl Default for MockGrpcServer {
    fn default() -> Self {
        Self::new()
    }
}
