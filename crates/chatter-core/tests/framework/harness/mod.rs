//! Test harnesses for different testing scenarios.
//!
//! Provides pre-configured test environments for unit, integration, and E2E tests.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::framework::builders::UserBuilder;
use crate::framework::fixtures::TestUser;
use crate::framework::mocks::{
    InMemorySignalStores, InMemoryStorage, MockGrpcServer, NetworkSimulator,
};

// ============================================================================
// Configuration Types
// ============================================================================

/// Network mode for tests.
#[derive(Clone)]
pub enum NetworkMode {
    /// Use mock in-process server.
    Mock,
    /// Simulate offline mode.
    Offline,
    /// Simulate flaky network with failure rate.
    Flaky(f32),
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::Mock
    }
}

/// Crypto mode for tests.
#[derive(Clone, Default)]
pub enum CryptoMode {
    /// Use real Signal Protocol crypto.
    #[default]
    Real,
    /// Skip encryption (faster tests).
    Passthrough,
    /// Use deterministic crypto for reproducibility.
    Deterministic(u64),
}

/// Storage mode for tests.
#[derive(Clone, Default)]
pub enum StorageMode {
    /// Use in-memory storage.
    #[default]
    Memory,
}

// ============================================================================
// Test Harness
// ============================================================================

/// Main test harness providing configured test environment.
pub struct TestHarness {
    /// Storage backend.
    pub storage: Arc<InMemoryStorage>,
    /// Signal stores.
    pub signal_stores: Arc<InMemorySignalStores>,
    /// Mock gRPC server (if using mock network).
    pub mock_server: Option<Arc<MockGrpcServer>>,
    /// Network simulator.
    pub network: NetworkSimulator,
    /// Registered test users.
    users: RwLock<Vec<TestUser>>,
}

#[derive(Clone, Default)]
struct HarnessConfig {
    network_mode: NetworkMode,
    crypto_mode: CryptoMode,
    storage_mode: StorageMode,
}

impl TestHarness {
    /// Create a builder for custom configuration.
    pub fn builder() -> TestHarnessBuilder {
        TestHarnessBuilder::default()
    }

    /// Quick setup for unit tests - everything mocked.
    pub async fn unit() -> Self {
        Self::builder()
            .with_mock_network()
            .with_passthrough_crypto()
            .with_memory_storage()
            .build()
            .await
    }

    /// Get the mock server (panics if not using mock network).
    pub fn mock_server(&self) -> &Arc<MockGrpcServer> {
        self.mock_server
            .as_ref()
            .expect("Mock server not available in this configuration")
    }

    /// Register a new test user.
    pub async fn register_user(&self, name: &str) -> TestUser {
        let user = UserBuilder::named(name).build();

        // If using mock server, register the user
        if let Some(server) = &self.mock_server {
            server.register_user(user.clone());
        }

        self.users.write().await.push(user.clone());
        user
    }

    /// Create authenticated user context.
    pub async fn as_user(&self, user: &TestUser) -> UserContext<'_> {
        UserContext {
            user: user.clone(),
            harness: self,
        }
    }

    /// Create two users for conversation tests.
    pub async fn conversation_pair(&self) -> (UserContext<'_>, UserContext<'_>) {
        let alice = self.register_user("Alice").await;
        let bob = self.register_user("Bob").await;
        (self.as_user(&alice).await, self.as_user(&bob).await)
    }

    /// Create three users for group tests.
    pub async fn conversation_trio(&self) -> (UserContext<'_>, UserContext<'_>, UserContext<'_>) {
        let alice = self.register_user("Alice").await;
        let bob = self.register_user("Bob").await;
        let charlie = self.register_user("Charlie").await;
        (
            self.as_user(&alice).await,
            self.as_user(&bob).await,
            self.as_user(&charlie).await,
        )
    }

    /// Get all registered users.
    pub async fn get_users(&self) -> Vec<TestUser> {
        self.users.read().await.clone()
    }

    /// Reset harness state.
    pub async fn reset(&self) {
        self.storage.clear();
        if let Some(server) = &self.mock_server {
            server.clear();
        }
        self.users.write().await.clear();
    }
}

// ============================================================================
// Test Harness Builder
// ============================================================================

/// Builder for configuring test harness.
#[derive(Default)]
pub struct TestHarnessBuilder {
    config: HarnessConfig,
    network_latency: Option<std::time::Duration>,
    failure_rate: Option<f32>,
}

impl TestHarnessBuilder {
    /// Use mock in-process server.
    pub fn with_mock_network(mut self) -> Self {
        self.config.network_mode = NetworkMode::Mock;
        self
    }

    /// Simulate offline mode.
    pub fn offline(mut self) -> Self {
        self.config.network_mode = NetworkMode::Offline;
        self
    }

    /// Simulate flaky network.
    pub fn with_flaky_network(mut self, failure_rate: f32) -> Self {
        self.config.network_mode = NetworkMode::Flaky(failure_rate);
        self.failure_rate = Some(failure_rate);
        self
    }

    /// Add network latency.
    pub fn with_latency(mut self, latency: std::time::Duration) -> Self {
        self.network_latency = Some(latency);
        self
    }

    /// Skip encryption for faster tests.
    pub fn with_passthrough_crypto(mut self) -> Self {
        self.config.crypto_mode = CryptoMode::Passthrough;
        self
    }

    /// Use deterministic crypto for reproducibility.
    pub fn with_deterministic_crypto(mut self, seed: u64) -> Self {
        self.config.crypto_mode = CryptoMode::Deterministic(seed);
        self
    }

    /// Use in-memory storage.
    pub fn with_memory_storage(mut self) -> Self {
        self.config.storage_mode = StorageMode::Memory;
        self
    }

    /// Build the test harness.
    pub async fn build(self) -> TestHarness {
        // Create storage
        let storage = Arc::new(InMemoryStorage::new());

        // Create signal stores
        let signal_stores = match &self.config.crypto_mode {
            CryptoMode::Deterministic(seed) => Arc::new(InMemorySignalStores::deterministic(*seed)),
            _ => Arc::new(InMemorySignalStores::new()),
        };

        // Create network simulator
        let mut network = match &self.config.network_mode {
            NetworkMode::Mock => NetworkSimulator::perfect(),
            NetworkMode::Offline => NetworkSimulator::offline(),
            NetworkMode::Flaky(rate) => NetworkSimulator::flaky(*rate),
        };

        if let Some(latency) = self.network_latency {
            network = network.with_latency(latency);
        }

        // Create mock server if needed
        let mock_server = match &self.config.network_mode {
            NetworkMode::Mock | NetworkMode::Flaky(_) => Some(Arc::new(
                MockGrpcServer::new().with_network(network.clone()),
            )),
            _ => None,
        };

        TestHarness {
            storage,
            signal_stores,
            mock_server,
            network,
            users: RwLock::new(Vec::new()),
        }
    }
}

// ============================================================================
// User Context
// ============================================================================

/// Context for a specific user in tests.
pub struct UserContext<'a> {
    /// The test user.
    pub user: TestUser,
    /// Reference to the harness.
    harness: &'a TestHarness,
}

impl<'a> UserContext<'a> {
    /// Get user ID.
    pub fn user_id(&self) -> &str {
        &self.user.user_id
    }

    /// Get device ID.
    pub fn device_id(&self) -> u32 {
        self.user.device_id
    }

    /// Get display name.
    pub fn display_name(&self) -> &str {
        &self.user.display_name
    }

    /// Check if session exists with another user.
    pub fn has_session_with(&self, other: &UserContext) -> bool {
        let address = format!("{}.{}", other.user_id(), other.device_id());
        self.harness.signal_stores.has_session(&address)
    }

    /// Get the storage.
    pub fn storage(&self) -> &InMemoryStorage {
        &self.harness.storage
    }

    /// Get the signal stores.
    pub fn signal_stores(&self) -> &InMemorySignalStores {
        &self.harness.signal_stores
    }
}
