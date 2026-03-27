//! Stream manager.
//!
//! Manages all active real-time streams including message events, presence
//! updates, and user events. Integrates with the connection state machine
//! to react to connection lifecycle changes.

use std::sync::Arc;

use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::error::NetworkError;

/// Handle for controlling a running stream.
///
/// Allows graceful shutdown and lifecycle monitoring of individual streams.
pub struct StreamHandle {
    /// Task handle for the stream processor.
    task: JoinHandle<()>,
    /// Signal to stop the stream gracefully.
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl StreamHandle {
    /// Create a new stream handle.
    pub fn new(task: JoinHandle<()>, shutdown_tx: oneshot::Sender<()>) -> Self {
        Self {
            task,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Stop the stream gracefully.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    /// Check if the stream task is still running.
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }

    /// Wait for the stream task to complete.
    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.task.await
    }
}

/// Configuration for stream behavior.
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// Capacity for internal event buffers.
    pub buffer_capacity: usize,
    /// Whether to auto-reconnect on stream errors.
    pub auto_reconnect: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            buffer_capacity: 256,
            auto_reconnect: true,
        }
    }
}

use parking_lot::Mutex;
use tokio_stream::StreamExt;

use crate::network::connection::ConnectionStateMachine;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::streaming::message_stream::MessageEventRouter;
use crate::network::streaming::presence_stream::PresenceRouter;
use crate::network::streaming::push_listener::{PushEvent, PushListener, PushListenerConfig};

/// Container for active stream handles.
#[derive(Default)]
struct StreamHandles {
    message_events: Option<StreamHandle>,
    presence: Option<StreamHandle>,
    user_events: Option<StreamHandle>,
    push_listener: Option<StreamHandle>,
}

/// Manager for all real-time gRPC streams.
///
/// Orchestrates message events, presence updates, and user events streams.
/// Integrates with ConnectionStateMachine to react to connection changes.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let conn = ConnectionStateMachine::new();
/// let manager = StreamManager::new(pool.clone(), conn.clone(), StreamConfig::default());
///
/// manager.start_all().await?;
/// // ... use streams ...
/// manager.stop_all().await;
/// ```
pub struct StreamManager {
    pool: Arc<GrpcClientPool>,
    connection: ConnectionStateMachine,
    config: StreamConfig,

    /// Active stream handles.
    handles: Mutex<StreamHandles>,

    /// Broadcast channel for shutting down all streams.
    shutdown: broadcast::Sender<()>,

    /// Message event router for typed subscriptions.
    message_router: Arc<MessageEventRouter>,

    /// Presence router for presence updates and caching.
    presence_router: Arc<PresenceRouter>,

    /// Optional push listener for self-hosted ntfy (Android background push).
    /// None on desktop (gRPC streams are sufficient).
    push_listener: Option<Arc<PushListener>>,
}

impl StreamManager {
    /// Shutdown broadcast capacity.
    const SHUTDOWN_CAPACITY: usize = 4;

    /// Create a new StreamManager.
    ///
    /// # Arguments
    ///
    /// * `pool` - gRPC client pool for creating service clients
    /// * `connection` - Connection state machine for state tracking
    /// * `config` - Stream configuration
    pub fn new(
        pool: Arc<GrpcClientPool>,
        connection: ConnectionStateMachine,
        config: StreamConfig,
    ) -> Self {
        let (shutdown, _) = broadcast::channel(Self::SHUTDOWN_CAPACITY);
        Self {
            pool,
            connection,
            config,
            handles: Mutex::new(StreamHandles::default()),
            shutdown,
            message_router: Arc::new(MessageEventRouter::new()),
            presence_router: Arc::new(PresenceRouter::new()),
            push_listener: None,
        }
    }

    /// Get the message event router for subscribing to events.
    pub fn message_events(&self) -> &MessageEventRouter {
        &self.message_router
    }

    /// Get the presence router for subscribing to presence updates.
    pub fn presence(&self) -> &PresenceRouter {
        &self.presence_router
    }

    /// Check if currently streaming.
    pub fn is_streaming(&self) -> bool {
        let handles = self.handles.lock();
        handles
            .message_events
            .as_ref()
            .is_some_and(|h| h.is_running())
            || handles.presence.as_ref().is_some_and(|h| h.is_running())
            || handles
                .user_events
                .as_ref()
                .is_some_and(|h| h.is_running())
    }

    /// Get a shutdown receiver for stream tasks.
    fn shutdown_receiver(&self) -> broadcast::Receiver<()> {
        self.shutdown.subscribe()
    }

    /// Get a reference to the connection state machine.
    pub fn connection(&self) -> &ConnectionStateMachine {
        &self.connection
    }

    /// Get the stream configuration.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Configure a self-hosted ntfy push listener for Android background push.
    ///
    /// On desktop, this is optional — gRPC streams handle real-time notifications.
    /// On Android, the OS kills background connections, so the push listener
    /// provides wake-up signals via SSE to the self-hosted ntfy instance.
    pub fn with_push_listener(mut self, config: PushListenerConfig) -> Self {
        self.push_listener = Some(Arc::new(PushListener::new(config)));
        self
    }

    /// Get a push event subscriber (if push listener is configured).
    ///
    /// Returns None on desktop where push listener isn't needed.
    pub fn push_events(&self) -> Option<broadcast::Receiver<PushEvent>> {
        self.push_listener.as_ref().map(|l| l.subscribe())
    }

    /// Start all streams.
    ///
    /// Launches message events, presence, and user events streams.
    /// Each stream runs in its own tokio task.
    /// If a push listener is configured, starts it too.
    ///
    /// # Errors
    ///
    /// Returns error if pool is not connected.
    pub async fn start_all(&self) -> Result<(), NetworkError> {
        if !self.pool.is_connected() {
            return Err(NetworkError::ConnectionFailed(
                "pool not connected".to_string(),
            ));
        }

        // Start message events stream (includes typing, reactions, read receipts)
        self.start_message_events_stream().await?;

        // Start presence stream
        self.start_presence_stream().await?;

        // Start push listener (if configured — typically Android only)
        if self.push_listener.is_some() {
            self.start_push_listener().await;
        }

        Ok(())
    }

    /// Stop all running streams.
    ///
    /// Sends shutdown signal and waits for tasks to complete.
    pub async fn stop_all(&self) {
        // Signal all streams to stop
        let _ = self.shutdown.send(());

        // Take all handles and stop them
        let mut handles = self.handles.lock();
        if let Some(mut h) = handles.message_events.take() {
            h.stop();
        }
        if let Some(mut h) = handles.presence.take() {
            h.stop();
        }
        if let Some(mut h) = handles.user_events.take() {
            h.stop();
        }
        if let Some(mut h) = handles.push_listener.take() {
            h.stop();
        }
    }

    /// Restart all streams (for reconnection).
    pub async fn restart_all(&self) -> Result<(), NetworkError> {
        self.stop_all().await;
        self.start_all().await
    }

    /// Start the message events stream.
    ///
    /// This stream receives all message-related events including:
    /// - New messages, edits, deletions
    /// - Reactions added/removed
    /// - Typing indicators
    /// - Read receipts
    async fn start_message_events_stream(&self) -> Result<(), NetworkError> {
        let mut client = self.pool.message_client();
        let mut shutdown_rx = self.shutdown_receiver();
        let router = Arc::clone(&self.message_router);

        let (shutdown_tx, mut shutdown_oneshot) = oneshot::channel();

        let task = tokio::spawn(async move {

            // Start global events stream (all user's channels)
            let stream_result = client.stream_global_events(vec![], None).await;

            let mut stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to start message events stream: {}", e);
                    return;
                }
            };

            loop {
                tokio::select! {
                    // Check for shutdown signal from broadcast
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("Message events stream shutting down (broadcast)");
                        break;
                    }
                    // Check for shutdown signal from handle
                    _ = &mut shutdown_oneshot => {
                        tracing::debug!("Message events stream stopped via handle");
                        break;
                    }
                    // Process stream events
                    item = stream.next() => {
                        match item {
                            Some(Ok(event)) => {
                                router.route(event);
                            }
                            Some(Err(e)) => {
                                tracing::warn!("Message events stream error: {}", e);
                                // Stream will likely end, break to trigger reconnect
                                break;
                            }
                            None => {
                                tracing::info!("Message events stream ended");
                                break;
                            }
                        }
                    }
                }
            }
        });

        let handle = StreamHandle::new(task, shutdown_tx);
        self.handles.lock().message_events = Some(handle);

        Ok(())
    }

    /// Start the self-hosted ntfy push listener.
    ///
    /// Connects to the ntfy SSE endpoint and broadcasts wake-up events.
    /// Used on Android where the OS kills background gRPC connections.
    async fn start_push_listener(&self) {
        let listener = match &self.push_listener {
            Some(l) => Arc::clone(l),
            None => return,
        };

        let shutdown_rx = self.shutdown_receiver();
        let (shutdown_tx, _) = oneshot::channel();

        let task = tokio::spawn(async move {
            listener.listen(shutdown_rx).await;
        });

        let handle = StreamHandle::new(task, shutdown_tx);
        self.handles.lock().push_listener = Some(handle);

        tracing::info!("Push listener started");
    }

    /// Start the presence stream.
    ///
    /// Receives real-time presence updates for friends.
    async fn start_presence_stream(&self) -> Result<(), NetworkError> {
        let mut client = self.pool.user_client();
        let mut shutdown_rx = self.shutdown_receiver();
        let router = Arc::clone(&self.presence_router);

        let (shutdown_tx, mut shutdown_oneshot) = oneshot::channel();

        let task = tokio::spawn(async move {

            let stream_result = client.stream_presence().await;

            let mut stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to start presence stream: {}", e);
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("Presence stream shutting down (broadcast)");
                        break;
                    }
                    _ = &mut shutdown_oneshot => {
                        tracing::debug!("Presence stream stopped via handle");
                        break;
                    }
                    item = stream.next() => {
                        match item {
                            Some(Ok(update)) => {
                                router.route(update);
                            }
                            Some(Err(e)) => {
                                tracing::warn!("Presence stream error: {}", e);
                                break;
                            }
                            None => {
                                tracing::info!("Presence stream ended");
                                break;
                            }
                        }
                    }
                }
            }
        });

        let handle = StreamHandle::new(task, shutdown_tx);
        self.handles.lock().presence = Some(handle);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_config_default() {
        let config = StreamConfig::default();
        assert_eq!(config.buffer_capacity, 256);
        assert!(config.auto_reconnect);
    }

    #[test]
    fn test_stream_handles_default() {
        let handles = StreamHandles::default();
        assert!(handles.message_events.is_none());
        assert!(handles.presence.is_none());
        assert!(handles.user_events.is_none());
    }

    #[test]
    fn test_stream_manager_not_streaming_initially() {
        let pool = Arc::new(GrpcClientPool::new("http://localhost:50051").unwrap());
        let conn = ConnectionStateMachine::new();
        let manager = StreamManager::new(pool, conn, StreamConfig::default());

        assert!(!manager.is_streaming());
    }

    #[tokio::test]
    async fn test_start_all_fails_when_not_connected() {
        let pool = Arc::new(GrpcClientPool::new("http://localhost:50051").unwrap());
        let conn = ConnectionStateMachine::new();
        let manager = StreamManager::new(pool, conn, StreamConfig::default());

        let result = manager.start_all().await;
        assert!(result.is_err());

        if let Err(NetworkError::ConnectionFailed(msg)) = result {
            assert!(msg.contains("not connected"));
        } else {
            panic!("Expected ConnectionFailed error");
        }
    }

    #[tokio::test]
    async fn test_stop_all_is_idempotent() {
        let pool = Arc::new(GrpcClientPool::new("http://localhost:50051").unwrap());
        let conn = ConnectionStateMachine::new();
        let manager = StreamManager::new(pool, conn, StreamConfig::default());

        // Should not panic even when nothing is running
        manager.stop_all().await;
        manager.stop_all().await;
    }

    #[test]
    fn test_stream_config_custom() {
        let config = StreamConfig {
            buffer_capacity: 512,
            auto_reconnect: false,
        };
        assert_eq!(config.buffer_capacity, 512);
        assert!(!config.auto_reconnect);
    }
}
