//! Connection manager.
//!
//! Orchestrates connection lifecycle, automatic reconnection, and offline queue replay.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::error::NetworkError;

use super::connection::{ConnectionEvent, ConnectionState, ConnectionStateMachine, FailReason};
use super::grpc::client_pool::GrpcClientPool;
use super::offline_queue::OfflineQueue;
use super::recovery::{ConnectionRecovery, FibonacciBackoff};

/// Configuration for the ConnectionManager.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    /// Server URL (e.g., "http://localhost:50051")
    pub server_url: String,
    /// Path for offline queue persistence
    pub queue_path: PathBuf,
    /// Maximum offline queue size
    pub max_queue_size: usize,
    /// Maximum reconnection attempts (None = unlimited)
    pub max_reconnect_attempts: Option<usize>,
    /// Use extended timeouts for prolonged offline
    pub use_extended_timeouts: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            server_url: "http://localhost:50051".to_string(),
            queue_path: PathBuf::from("offline_queue.json"),
            max_queue_size: 1000,
            max_reconnect_attempts: None, // Unlimited
            use_extended_timeouts: false,
        }
    }
}

impl ConnectionConfig {
    /// Create a new config with the given server URL.
    pub fn new(server_url: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            ..Default::default()
        }
    }

    /// Set the queue path.
    pub fn with_queue_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.queue_path = path.into();
        self
    }

    /// Set the maximum queue size.
    pub fn with_max_queue_size(mut self, size: usize) -> Self {
        self.max_queue_size = size;
        self
    }

    /// Set the maximum reconnection attempts.
    pub fn with_max_reconnect_attempts(mut self, attempts: Option<usize>) -> Self {
        self.max_reconnect_attempts = attempts;
        self
    }

    /// Enable extended timeouts for prolonged offline scenarios.
    pub fn with_extended_timeouts(mut self, enabled: bool) -> Self {
        self.use_extended_timeouts = enabled;
        self
    }
}

/// Result of replaying queued operations.
#[derive(Clone, Debug, Default)]
pub struct ReplayResult {
    /// Total operations attempted
    pub total: usize,
    /// Operations that succeeded
    pub succeeded: usize,
    /// Operations that failed permanently
    pub failed: usize,
    /// Operations remaining in queue (transient failures)
    pub remaining: usize,
}

/// Manages connection lifecycle including reconnection and offline queue.
///
/// ConnectionManager orchestrates:
/// - Initial connection establishment
/// - Automatic reconnection with Fibonacci backoff
/// - Offline queue management and replay
/// - Event broadcasting for UI updates
///
/// # Example
///
/// ```ignore
/// use chatter_linux::network::connection_manager::{ConnectionManager, ConnectionConfig};
///
/// let config = ConnectionConfig::new("http://localhost:50051");
/// let manager = ConnectionManager::new(config)?;
///
/// // Subscribe to events
/// let mut rx = manager.subscribe();
///
/// // Connect
/// manager.connect().await?;
///
/// // Start automatic reconnection loop
/// let handle = manager.start_reconnect_loop();
///
/// // On shutdown
/// manager.shutdown().await;
/// ```
pub struct ConnectionManager {
    config: ConnectionConfig,
    state_machine: ConnectionStateMachine,
    recovery: Mutex<ConnectionRecovery>,
    offline_queue: Arc<OfflineQueue>,
    pool: Arc<GrpcClientPool>,
    shutdown: broadcast::Sender<()>,
}

impl ConnectionManager {
    /// Broadcast channel capacity.
    const CHANNEL_CAPACITY: usize = 16;

    /// Create a new ConnectionManager.
    ///
    /// # Arguments
    ///
    /// * `config` - Connection configuration
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the server URL is invalid.
    pub fn new(config: ConnectionConfig) -> Result<Self, NetworkError> {
        let pool = GrpcClientPool::new(&config.server_url)?;

        let recovery = if let Some(max) = config.max_reconnect_attempts {
            ConnectionRecovery::with_max_attempts(max)
        } else if config.use_extended_timeouts {
            ConnectionRecovery::extended()
        } else {
            ConnectionRecovery::new()
        };

        let (shutdown, _) = broadcast::channel(Self::CHANNEL_CAPACITY);

        Ok(Self {
            offline_queue: Arc::new(OfflineQueue::new(config.max_queue_size)),
            pool: Arc::new(pool),
            state_machine: ConnectionStateMachine::new(),
            recovery: Mutex::new(recovery),
            shutdown,
            config,
        })
    }

    /// Get the connection state machine for subscribing to events.
    pub fn state_machine(&self) -> &ConnectionStateMachine {
        &self.state_machine
    }

    /// Get the current connection state.
    pub fn state(&self) -> ConnectionState {
        self.state_machine.state()
    }

    /// Get the offline queue for queueing operations.
    pub fn offline_queue(&self) -> Arc<OfflineQueue> {
        Arc::clone(&self.offline_queue)
    }

    /// Get the gRPC client pool.
    pub fn pool(&self) -> Arc<GrpcClientPool> {
        Arc::clone(&self.pool)
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.state_machine.state().is_connected()
    }

    /// Subscribe to connection events.
    ///
    /// Returns a receiver that will get all future connection events.
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.state_machine.subscribe()
    }

    /// Get the configuration.
    pub fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    /// Subscribe to shutdown signal.
    pub fn shutdown_receiver(&self) -> broadcast::Receiver<()> {
        self.shutdown.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ConnectionConfig::default();

        assert_eq!(config.server_url, "http://localhost:50051");
        assert_eq!(config.queue_path, PathBuf::from("offline_queue.json"));
        assert_eq!(config.max_queue_size, 1000);
        assert!(config.max_reconnect_attempts.is_none());
        assert!(!config.use_extended_timeouts);
    }

    #[test]
    fn test_config_builder() {
        let config = ConnectionConfig::new("http://example.com:50051")
            .with_queue_path("/tmp/queue.json")
            .with_max_queue_size(500)
            .with_max_reconnect_attempts(Some(10))
            .with_extended_timeouts(true);

        assert_eq!(config.server_url, "http://example.com:50051");
        assert_eq!(config.queue_path, PathBuf::from("/tmp/queue.json"));
        assert_eq!(config.max_queue_size, 500);
        assert_eq!(config.max_reconnect_attempts, Some(10));
        assert!(config.use_extended_timeouts);
    }

    #[test]
    fn test_new_manager() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        assert!(!manager.is_connected());
        assert_eq!(manager.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn test_new_manager_invalid_url() {
        let config = ConnectionConfig::new("not a valid url");
        let result = ConnectionManager::new(config);

        assert!(result.is_err());
    }

    #[test]
    fn test_accessors() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config.clone()).unwrap();

        // State machine access
        assert_eq!(manager.state_machine().state(), ConnectionState::Disconnected);

        // State access
        assert_eq!(manager.state(), ConnectionState::Disconnected);

        // Offline queue access
        let queue = manager.offline_queue();
        assert!(queue.is_empty());

        // Pool access
        let pool = manager.pool();
        assert!(!pool.is_connected());

        // Config access
        assert_eq!(manager.config().server_url, config.server_url);
    }

    #[test]
    fn test_subscribe() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        // Should be able to subscribe
        let _rx = manager.subscribe();
    }

    #[test]
    fn test_replay_result_default() {
        let result = ReplayResult::default();

        assert_eq!(result.total, 0);
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.remaining, 0);
    }

    #[test]
    fn test_manager_with_max_attempts() {
        let config = ConnectionConfig::default().with_max_reconnect_attempts(Some(5));
        let manager = ConnectionManager::new(config).unwrap();

        // Verify recovery is configured with max attempts
        let recovery = manager.recovery.lock();
        assert!(!recovery.is_exhausted()); // Not exhausted initially
    }

    #[test]
    fn test_manager_with_extended_timeouts() {
        let config = ConnectionConfig::default().with_extended_timeouts(true);
        let manager = ConnectionManager::new(config).unwrap();

        // Should create successfully with extended timeouts
        assert!(!manager.is_connected());
    }
}
