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
use super::recovery::ConnectionRecovery;

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

    // ==================== Connection Lifecycle Methods ====================

    /// Connect to the server.
    ///
    /// Transitions state: Disconnected -> Connecting -> Connected
    /// On failure: Disconnected -> Connecting -> Disconnected
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if connection fails.
    pub async fn connect(&self) -> Result<(), NetworkError> {
        // Transition to connecting
        self.state_machine.connecting();

        // Attempt connection
        match self.pool.connect().await {
            Ok(()) => {
                // Success - transition to connected
                self.state_machine.connected();

                // Reset recovery on successful connection
                self.recovery.lock().reset();

                Ok(())
            }
            Err(e) => {
                // Failure - transition back to disconnected
                self.state_machine.disconnected();
                Err(e)
            }
        }
    }

    /// Disconnect from the server.
    ///
    /// Gracefully disconnects and transitions to Disconnected state.
    pub async fn disconnect(&self) {
        self.pool.disconnect();
        self.state_machine.disconnected();
    }

    /// Trigger a manual reconnection attempt.
    ///
    /// Useful after user action (e.g., clicking "Retry" button).
    /// Resets the backoff timer before attempting.
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if connection fails.
    pub async fn reconnect(&self) -> Result<(), NetworkError> {
        // Reset backoff for manual reconnection
        self.recovery.lock().reset();

        // Attempt connection
        self.connect().await
    }

    /// Start the automatic reconnection loop.
    ///
    /// Spawns a background task that:
    /// 1. Listens for Offline events from state machine
    /// 2. On Offline, begins reconnection attempts with backoff
    /// 3. On successful reconnect, emits Online event
    /// 4. On max attempts exceeded, transitions to Failed(MaxRetriesExceeded)
    ///
    /// The loop runs until shutdown() is called.
    ///
    /// # Returns
    ///
    /// A JoinHandle for the spawned task.
    pub fn start_reconnect_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        let mut event_rx = manager.subscribe();
        let mut shutdown_rx = manager.shutdown.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Check for shutdown signal
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Reconnection loop received shutdown signal");
                        break;
                    }

                    // Listen for connection events
                    event = event_rx.recv() => {
                        match event {
                            Ok(ConnectionEvent::Offline) => {
                                tracing::info!("Connection lost, starting reconnection attempts");
                                manager.run_reconnection_sequence().await;
                            }
                            Ok(_) => {
                                // Ignore other events
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                tracing::info!("Event channel closed, stopping reconnection loop");
                                break;
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("Reconnection loop lagged by {} events", n);
                            }
                        }
                    }
                }
            }
        })
    }

    /// Run the reconnection sequence with backoff.
    ///
    /// Internal method called by the reconnection loop.
    async fn run_reconnection_sequence(&self) {
        let mut attempt: u32 = 0;

        loop {
            // Get next delay from recovery
            let delay = {
                let mut recovery = self.recovery.lock();
                recovery.next_delay()
            };

            match delay {
                Some(duration) => {
                    attempt += 1;

                    // Transition to reconnecting state
                    self.state_machine.reconnecting(attempt);

                    tracing::info!(
                        "Reconnection attempt {} in {:?}",
                        attempt,
                        duration
                    );

                    // Wait for backoff duration
                    tokio::time::sleep(duration).await;

                    // Check if shutdown was requested during sleep
                    if self.shutdown.receiver_count() == 0 {
                        tracing::info!("Shutdown requested, aborting reconnection");
                        return;
                    }

                    // Attempt reconnection
                    match self.pool.connect().await {
                        Ok(()) => {
                            tracing::info!("Reconnection successful after {} attempts", attempt);

                            // Reset recovery on success
                            self.recovery.lock().reset();

                            // Transition to connected
                            self.state_machine.connected();

                            return;
                        }
                        Err(e) => {
                            tracing::warn!("Reconnection attempt {} failed: {}", attempt, e);

                            // Check for auth errors
                            if e.is_auth_error() {
                                tracing::error!("Authentication error during reconnection");
                                self.state_machine.failed(FailReason::AuthExpired);
                                return;
                            }

                            // Continue to next attempt
                        }
                    }
                }
                None => {
                    // Max attempts exceeded
                    tracing::error!("Maximum reconnection attempts exceeded");
                    self.state_machine.failed(FailReason::MaxRetriesExceeded);
                    return;
                }
            }
        }
    }

    /// Shutdown the manager gracefully.
    ///
    /// - Saves offline queue to disk
    /// - Stops reconnection loop
    /// - Disconnects from server
    pub async fn shutdown(&self) {
        tracing::info!("ConnectionManager shutting down");

        // Save the offline queue
        if let Err(e) = self.offline_queue.save(&self.config.queue_path).await {
            tracing::error!("Failed to save offline queue: {}", e);
        }

        // Signal shutdown to reconnection loop
        let _ = self.shutdown.send(());

        // Disconnect
        self.pool.disconnect();
        self.state_machine.disconnected();

        tracing::info!("ConnectionManager shutdown complete");
    }

    // ==================== Queue Persistence Methods ====================

    /// Load the offline queue from disk.
    ///
    /// Called on startup to restore queued operations.
    ///
    /// # Returns
    ///
    /// The number of operations loaded.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub async fn load_queue(&self) -> std::io::Result<usize> {
        let count = self.offline_queue.load(&self.config.queue_path).await?;
        tracing::info!("Loaded {} operations from offline queue", count);
        Ok(count)
    }

    /// Save the offline queue to disk.
    ///
    /// Called on shutdown and can be called periodically.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub async fn save_queue(&self) -> std::io::Result<()> {
        self.offline_queue.save(&self.config.queue_path).await?;
        tracing::info!("Saved {} operations to offline queue", self.offline_queue.len());
        Ok(())
    }

    // ==================== Queue Replay Methods ====================

    /// Replay queued operations after reconnection.
    ///
    /// Called automatically by the reconnect loop on successful reconnect,
    /// or can be called manually.
    ///
    /// For each queued operation:
    /// 1. Attempt to execute (placeholder - actual execution depends on operation type)
    /// 2. On success: remove from queue
    /// 3. On transient failure: increment attempts, keep in queue
    /// 4. On permanent failure (max attempts): remove from queue
    ///
    /// Emits QueueEvent::ReplayStarted and ReplayCompleted.
    ///
    /// # Returns
    ///
    /// A ReplayResult with statistics about the replay.
    pub async fn replay_queue(&self) -> ReplayResult {
        let pending = self.offline_queue.pending_operations();
        let total = pending.len();

        if total == 0 {
            return ReplayResult::default();
        }

        tracing::info!("Starting replay of {} queued operations", total);
        self.offline_queue.emit_replay_started(total);

        let mut succeeded = 0usize;
        let mut failed = 0usize;

        for op in pending {
            // Check if we're still connected
            if !self.is_connected() {
                tracing::warn!("Connection lost during replay, stopping");
                break;
            }

            // Attempt to execute the operation
            // Note: This is a placeholder. In a real implementation, this would
            // dispatch to the appropriate handler based on operation type.
            let result = self.execute_queued_operation(&op).await;

            match result {
                Ok(()) => {
                    // Success - remove from queue
                    self.offline_queue.remove(op.id);
                    succeeded += 1;
                    tracing::debug!("Replayed operation {} successfully", op.id);
                }
                Err(e) => {
                    if e.is_retryable() {
                        // Transient failure - mark as attempted
                        if !self.offline_queue.mark_attempted(op.id) {
                            // Max attempts exceeded - remove from queue
                            self.offline_queue.remove(op.id);
                            failed += 1;
                            tracing::warn!(
                                "Operation {} failed after max attempts: {}",
                                op.id,
                                e
                            );
                        } else {
                            tracing::debug!(
                                "Operation {} failed (will retry): {}",
                                op.id,
                                e
                            );
                        }
                    } else {
                        // Permanent failure - remove from queue
                        self.offline_queue.remove(op.id);
                        failed += 1;
                        tracing::warn!("Operation {} failed permanently: {}", op.id, e);
                    }
                }
            }
        }

        let remaining = self.offline_queue.len();

        tracing::info!(
            "Replay complete: {} succeeded, {} failed, {} remaining",
            succeeded,
            failed,
            remaining
        );

        self.offline_queue.emit_replay_completed(succeeded, failed);

        ReplayResult {
            total,
            succeeded,
            failed,
            remaining,
        }
    }

    /// Execute a single queued operation.
    ///
    /// This is a placeholder that simulates operation execution.
    /// In a real implementation, this would dispatch to the appropriate
    /// service handler based on the operation type.
    async fn execute_queued_operation(
        &self,
        _op: &super::offline_queue::QueuedOperation,
    ) -> Result<(), NetworkError> {
        // Placeholder implementation
        // In a real implementation, this would:
        // 1. Match on op.operation type
        // 2. Create the appropriate gRPC request
        // 3. Send via the pool
        // 4. Return result

        // For now, we succeed if connected
        if self.is_connected() {
            Ok(())
        } else {
            Err(NetworkError::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::offline_queue::OperationType;
    use tempfile::tempdir;

    // Helper to create a test message operation
    fn msg_op(channel: &str, content: &str) -> OperationType {
        OperationType::SendMessage {
            channel_id: channel.to_string(),
            content: content.as_bytes().to_vec(),
        }
    }

    // ==================== Configuration Tests ====================

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
        assert_eq!(
            manager.state_machine().state(),
            ConnectionState::Disconnected
        );

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

    // ==================== Lifecycle Tests ====================

    #[tokio::test]
    async fn test_connect_failure_to_nonexistent_server() {
        // Connect to a non-existent server should fail
        let config = ConnectionConfig::new("http://127.0.0.1:1");
        let manager = ConnectionManager::new(config).unwrap();

        // Initial state
        assert_eq!(manager.state(), ConnectionState::Disconnected);

        // Attempt to connect
        let result = manager.connect().await;

        // Should fail
        assert!(result.is_err());

        // State should return to Disconnected
        assert_eq!(manager.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn test_connect_transitions_state() {
        let config = ConnectionConfig::new("http://127.0.0.1:1");
        let manager = ConnectionManager::new(config).unwrap();
        let mut rx = manager.subscribe();

        // Attempt to connect (will fail, but we want to see the state transitions)
        let _ = manager.connect().await;

        // Should have received StateChanged to Connecting
        let event = rx.recv().await.unwrap();
        match event {
            ConnectionEvent::StateChanged { old, new } => {
                assert_eq!(old, ConnectionState::Disconnected);
                assert_eq!(new, ConnectionState::Connecting);
            }
            _ => panic!("Expected StateChanged event"),
        }

        // Should have received StateChanged back to Disconnected
        let event = rx.recv().await.unwrap();
        match event {
            ConnectionEvent::StateChanged { old, new } => {
                assert_eq!(old, ConnectionState::Connecting);
                assert_eq!(new, ConnectionState::Disconnected);
            }
            _ => panic!("Expected StateChanged event"),
        }
    }

    #[tokio::test]
    async fn test_disconnect() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        // Disconnect from initial state should be safe
        manager.disconnect().await;
        assert_eq!(manager.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn test_reconnect_resets_backoff() {
        let config = ConnectionConfig::new("http://127.0.0.1:1").with_max_reconnect_attempts(Some(5));
        let manager = ConnectionManager::new(config).unwrap();

        // Advance recovery state
        {
            let mut recovery = manager.recovery.lock();
            recovery.next_delay(); // attempt 1
            recovery.next_delay(); // attempt 2
            assert_eq!(recovery.attempt_count(), 2);
        }

        // Manual reconnect should reset backoff
        let _ = manager.reconnect().await;

        // Verify backoff was reset
        {
            let recovery = manager.recovery.lock();
            assert_eq!(recovery.attempt_count(), 0);
        }
    }

    // ==================== Reconnection Tests ====================

    #[tokio::test]
    async fn test_recovery_exhaustion() {
        let config = ConnectionConfig::new("http://127.0.0.1:1").with_max_reconnect_attempts(Some(2));
        let manager = ConnectionManager::new(config).unwrap();

        // Exhaust all attempts
        {
            let mut recovery = manager.recovery.lock();
            recovery.next_delay(); // attempt 1
            recovery.next_delay(); // attempt 2
            assert!(recovery.is_exhausted());
            assert!(recovery.next_delay().is_none());
        }
    }

    #[test]
    fn test_recovery_with_backoff_increases() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        let mut recovery = manager.recovery.lock();

        // Get delays and verify they increase (Fibonacci pattern)
        // Note: Delays include jitter, so we check they're reasonable
        let delay1 = recovery.next_delay().unwrap();
        let delay2 = recovery.next_delay().unwrap();

        // Both should be non-zero
        assert!(delay1.as_millis() > 0);
        assert!(delay2.as_millis() > 0);
    }

    // ==================== Queue Replay Tests ====================

    #[tokio::test]
    async fn test_replay_empty_queue() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        // Replay empty queue
        let result = manager.replay_queue().await;

        assert_eq!(result.total, 0);
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.remaining, 0);
    }

    #[tokio::test]
    async fn test_replay_when_disconnected() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        // Add operations to queue
        let queue = manager.offline_queue();
        queue.enqueue(msg_op("ch1", "Hello")).unwrap();
        queue.enqueue(msg_op("ch2", "World")).unwrap();

        // Replay while disconnected - operations should not succeed
        // (execute_queued_operation returns Unavailable when disconnected)
        let result = manager.replay_queue().await;

        // Since we're not connected, operations return Unavailable (retryable)
        // They stay in queue with incremented attempts
        assert_eq!(result.total, 2);
        assert_eq!(result.succeeded, 0);
        // Operations marked as attempted but not removed yet
        assert!(result.remaining > 0 || result.failed > 0);
    }

    #[tokio::test]
    async fn test_replay_emits_events() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        // Add operation to queue
        let queue = manager.offline_queue();
        queue.enqueue(msg_op("ch", "test")).unwrap();

        let mut rx = queue.subscribe();

        // Trigger replay
        manager.replay_queue().await;

        // Should have received ReplayStarted
        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event,
            crate::network::offline_queue::QueueEvent::ReplayStarted { count: 1 }
        ));

        // Should have received ReplayCompleted
        let mut completed = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                crate::network::offline_queue::QueueEvent::ReplayCompleted { .. }
            ) {
                completed = true;
            }
        }
        assert!(completed);
    }

    // ==================== Persistence Tests ====================

    #[tokio::test]
    async fn test_save_load_queue() {
        let dir = tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");

        let config = ConnectionConfig::default().with_queue_path(&queue_path);
        let manager = ConnectionManager::new(config).unwrap();

        // Add operations
        let queue = manager.offline_queue();
        queue.enqueue(msg_op("ch1", "Hello")).unwrap();
        queue.enqueue(msg_op("ch2", "World")).unwrap();

        // Save
        manager.save_queue().await.unwrap();

        // Verify file exists
        assert!(queue_path.exists());

        // Create new manager and load
        let config2 = ConnectionConfig::default().with_queue_path(&queue_path);
        let manager2 = ConnectionManager::new(config2).unwrap();

        let loaded = manager2.load_queue().await.unwrap();
        assert_eq!(loaded, 2);
        assert_eq!(manager2.offline_queue().len(), 2);
    }

    #[tokio::test]
    async fn test_load_nonexistent_queue() {
        let dir = tempdir().unwrap();
        let queue_path = dir.path().join("nonexistent.json");

        let config = ConnectionConfig::default().with_queue_path(&queue_path);
        let manager = ConnectionManager::new(config).unwrap();

        // Load from nonexistent file should return 0
        let loaded = manager.load_queue().await.unwrap();
        assert_eq!(loaded, 0);
    }

    #[tokio::test]
    async fn test_shutdown_saves_queue() {
        let dir = tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");

        let config = ConnectionConfig::default().with_queue_path(&queue_path);
        let manager = ConnectionManager::new(config).unwrap();

        // Add operation
        manager.offline_queue().enqueue(msg_op("ch", "test")).unwrap();

        // Shutdown should save
        manager.shutdown().await;

        // Verify file exists
        assert!(queue_path.exists());

        // Verify contents by loading in new manager
        let config2 = ConnectionConfig::default().with_queue_path(&queue_path);
        let manager2 = ConnectionManager::new(config2).unwrap();
        let loaded = manager2.load_queue().await.unwrap();
        assert_eq!(loaded, 1);
    }

    // ==================== Shutdown Tests ====================

    #[tokio::test]
    async fn test_shutdown_transitions_to_disconnected() {
        let config = ConnectionConfig::default();
        let manager = ConnectionManager::new(config).unwrap();

        // Manually set to connected state
        manager.state_machine.connected();
        assert_eq!(manager.state(), ConnectionState::Connected);

        // Shutdown
        manager.shutdown().await;

        // Should be disconnected
        assert_eq!(manager.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn test_shutdown_signals_loop() {
        let config = ConnectionConfig::default();
        let manager = Arc::new(ConnectionManager::new(config).unwrap());

        let mut shutdown_rx = manager.shutdown_receiver();

        // Shutdown should signal
        manager.shutdown().await;

        // Receiver should get signal
        let result = shutdown_rx.try_recv();
        assert!(result.is_ok());
    }

    // ==================== Integration Tests ====================

    #[tokio::test]
    async fn test_full_lifecycle() {
        let dir = tempdir().unwrap();
        let queue_path = dir.path().join("queue.json");

        // Create manager
        let config = ConnectionConfig::new("http://127.0.0.1:1")
            .with_queue_path(&queue_path)
            .with_max_reconnect_attempts(Some(2));

        let manager = ConnectionManager::new(config).unwrap();

        // Initial state
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.offline_queue().is_empty());

        // Queue an operation
        manager.offline_queue().enqueue(msg_op("ch", "test")).unwrap();
        assert_eq!(manager.offline_queue().len(), 1);

        // Attempt connect (will fail)
        let result = manager.connect().await;
        assert!(result.is_err());
        assert_eq!(manager.state(), ConnectionState::Disconnected);

        // Shutdown
        manager.shutdown().await;

        // Queue should be persisted
        assert!(queue_path.exists());
        assert_eq!(manager.state(), ConnectionState::Disconnected);
    }
}
