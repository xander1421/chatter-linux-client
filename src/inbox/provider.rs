//! Device inbox provider.
//!
//! Main coordinator for device inbox operations including configuration,
//! event broadcasting, and lifecycle management.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::network::grpc::MailboxClient;
use crate::network::validation::MessageValidationPipeline;
use crate::proto::mailbox_v1::{EnvelopeType, EphemeralMessage};

use super::poller::InboxPoller;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for the inbox polling system.
#[derive(Debug, Clone)]
pub struct InboxConfig {
    /// Base polling interval when active.
    pub poll_interval: Duration,
    /// Polling interval when idle (no recent messages).
    pub poll_interval_idle: Duration,
    /// Maximum messages to fetch per poll request.
    pub max_batch_size: u32,
    /// Number of retry attempts for failed messages.
    pub retry_attempts: u32,
    /// Number of messages to acknowledge per request.
    pub ack_batch_size: u32,
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            poll_interval_idle: Duration::from_secs(30),
            max_batch_size: 100,
            retry_attempts: 3,
            ack_batch_size: 50,
        }
    }
}

impl InboxConfig {
    /// Create a new configuration builder.
    pub fn builder() -> InboxConfigBuilder {
        InboxConfigBuilder::new()
    }
}

/// Builder for `InboxConfig`.
#[derive(Debug, Clone)]
pub struct InboxConfigBuilder {
    config: InboxConfig,
}

impl InboxConfigBuilder {
    /// Create a new builder with default values.
    pub fn new() -> Self {
        Self {
            config: InboxConfig::default(),
        }
    }

    /// Set the base polling interval.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.config.poll_interval = interval;
        self
    }

    /// Set the idle polling interval.
    pub fn poll_interval_idle(mut self, interval: Duration) -> Self {
        self.config.poll_interval_idle = interval;
        self
    }

    /// Set the maximum batch size for polling.
    pub fn max_batch_size(mut self, size: u32) -> Self {
        self.config.max_batch_size = size;
        self
    }

    /// Set the number of retry attempts.
    pub fn retry_attempts(mut self, attempts: u32) -> Self {
        self.config.retry_attempts = attempts;
        self
    }

    /// Set the acknowledgment batch size.
    pub fn ack_batch_size(mut self, size: u32) -> Self {
        self.config.ack_batch_size = size;
        self
    }

    /// Build the configuration.
    pub fn build(self) -> InboxConfig {
        self.config
    }
}

impl Default for InboxConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Events
// ============================================================================

/// Events emitted by the inbox system for monitoring and coordination.
#[derive(Debug, Clone)]
pub enum InboxEvent {
    /// A new message was received from the server.
    MessageReceived {
        /// Unique message identifier.
        message_id: String,
        /// Type of envelope (SealedSender, PrekeyMessage, etc.).
        envelope_type: EnvelopeType,
    },
    /// A message was successfully processed through the pipeline.
    MessageProcessed {
        /// Unique message identifier.
        message_id: String,
    },
    /// A message failed processing.
    MessageFailed {
        /// Unique message identifier.
        message_id: String,
        /// Error description.
        error: String,
    },
    /// A poll cycle completed.
    PollCompleted {
        /// Number of messages received.
        count: u32,
        /// Whether more messages are available.
        has_more: bool,
    },
    /// A poll request failed.
    PollFailed {
        /// Error description.
        error: String,
    },
    /// Message acknowledgment failed.
    AcknowledgeFailed {
        /// Message IDs that failed to acknowledge.
        message_ids: Vec<String>,
        /// Error description.
        error: String,
    },
}

// ============================================================================
// State
// ============================================================================

/// Current state of the inbox polling system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum InboxState {
    /// Not currently polling.
    #[default]
    Idle,
    /// Actively polling the server.
    Polling,
    /// Processing received messages.
    Processing,
    /// Explicitly stopped.
    Stopped,
}


// ============================================================================
// Statistics
// ============================================================================

/// Statistics for monitoring inbox performance.
#[derive(Debug, Clone, Copy, Default)]
pub struct InboxStats {
    /// Total messages received.
    pub messages_received: u64,
    /// Total messages successfully processed.
    pub messages_processed: u64,
    /// Total messages that failed processing.
    pub messages_failed: u64,
    /// Time of last poll (if any).
    pub last_poll_at: Option<Instant>,
    /// Number of messages in last poll.
    pub last_poll_count: u32,
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during inbox operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InboxError {
    /// Poll request failed.
    #[error("poll failed: {0}")]
    PollFailed(String),

    /// Challenge-response authentication failed.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// Message acknowledgment failed.
    #[error("acknowledge failed: {0}")]
    AcknowledgeFailed(String),

    /// Ed25519 signing failed.
    #[error("signing failed: {0}")]
    SigningFailed(String),

    /// Provider is not running.
    #[error("provider not running")]
    NotRunning,

    /// Provider is already running.
    #[error("provider already running")]
    AlreadyRunning,

    /// Shutdown timed out.
    #[error("shutdown timed out")]
    ShutdownTimeout,
}

// ============================================================================
// DeviceInboxProvider
// ============================================================================

/// Event channel capacity for inbox events.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Timeout for graceful shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Main coordinator for device inbox operations.
///
/// Orchestrates the polling loop, message validation, and event broadcasting.
/// Messages flow through:
/// 1. `InboxPoller` fetches from server
/// 2. `MessageValidationPipeline` validates and orders
/// 3. Events emitted for handlers to process
///
/// # Example
///
/// ```ignore
/// let config = InboxConfig::builder()
///     .poll_interval(Duration::from_secs(5))
///     .build();
///
/// let provider = DeviceInboxProvider::new(
///     config,
///     mailbox_client,
///     (public_key, private_key),
///     pipeline,
/// );
///
/// // Subscribe to events before starting
/// let mut rx = provider.subscribe();
/// provider.start().await?;
///
/// // Handle events
/// while let Ok(event) = rx.recv().await {
///     match event {
///         InboxEvent::MessageReceived { message_id, .. } => {
///             // Process message
///         }
///         _ => {}
///     }
/// }
/// ```
pub struct DeviceInboxProvider {
    config: InboxConfig,
    poller: Arc<InboxPoller>,
    pipeline: Arc<MessageValidationPipeline>,
    event_tx: broadcast::Sender<InboxEvent>,
    shutdown_tx: watch::Sender<bool>,
    poller_handle: RwLock<Option<JoinHandle<()>>>,
    stats: Arc<RwLock<InboxStats>>,
    state: Arc<RwLock<InboxState>>,
}

impl DeviceInboxProvider {
    /// Create a new device inbox provider.
    ///
    /// # Arguments
    ///
    /// * `config` - Polling and processing configuration
    /// * `mailbox_client` - Client for mailbox RPCs
    /// * `keypair` - (public_key, private_key) for mailbox authentication
    /// * `pipeline` - Message validation pipeline
    pub fn new(
        config: InboxConfig,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        keypair: (Vec<u8>, Vec<u8>),
        pipeline: Arc<MessageValidationPipeline>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let state = Arc::new(RwLock::new(InboxState::Idle));

        let poller = Arc::new(InboxPoller::new(
            config.clone(),
            mailbox_client,
            keypair,
            shutdown_rx,
            event_tx.clone(),
            stats.clone(),
            state.clone(),
        ));

        Self {
            config,
            poller,
            pipeline,
            event_tx,
            shutdown_tx,
            poller_handle: RwLock::new(None),
            stats,
            state,
        }
    }

    /// Start the polling loop.
    ///
    /// Spawns a background task that continuously polls for messages.
    /// Does nothing if already running.
    pub async fn start(&self) -> Result<(), InboxError> {
        {
            let handle = self.poller_handle.read();
            if handle.is_some() {
                return Err(InboxError::AlreadyRunning);
            }
        }

        let poller = self.poller.clone();
        let pipeline = self.pipeline.clone();
        let event_tx = self.event_tx.clone();
        let stats = self.stats.clone();

        let handle = tokio::spawn(async move {
            Self::run_loop(poller, pipeline, event_tx, stats).await;
        });

        *self.poller_handle.write() = Some(handle);
        Ok(())
    }

    /// Stop the polling loop.
    ///
    /// Sends shutdown signal and waits for graceful termination.
    pub async fn stop(&self) -> Result<(), InboxError> {
        // Signal shutdown
        let _ = self.shutdown_tx.send(true);

        // Wait for handle with timeout
        let handle = self.poller_handle.write().take();
        if let Some(handle) = handle {
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, handle).await {
                Ok(_) => {
                    *self.state.write() = InboxState::Stopped;
                    Ok(())
                }
                Err(_) => Err(InboxError::ShutdownTimeout),
            }
        } else {
            Ok(())
        }
    }

    /// Main polling loop.
    async fn run_loop(
        poller: Arc<InboxPoller>,
        pipeline: Arc<MessageValidationPipeline>,
        event_tx: broadcast::Sender<InboxEvent>,
        stats: Arc<RwLock<InboxStats>>,
    ) {
        loop {
            match poller.run_once().await {
                Ok(Some(messages)) => {
                    for msg in messages {
                        Self::process_message(&msg, &pipeline, &event_tx, &stats);
                    }
                }
                Ok(None) => {
                    // Shutdown requested
                    break;
                }
                Err(_) => {
                    // Error already logged/emitted by poller
                    continue;
                }
            }
        }
    }

    /// Process a single message through the validation pipeline.
    fn process_message(
        msg: &EphemeralMessage,
        pipeline: &MessageValidationPipeline,
        event_tx: &broadcast::Sender<InboxEvent>,
        stats: &Arc<RwLock<InboxStats>>,
    ) {
        use crate::network::validation::PipelineResult;

        // Create a Message from the ephemeral message for pipeline processing
        // Note: The actual decryption happens in Phase 24 handlers
        // Here we just validate the envelope metadata
        let proto_msg = crate::proto::chat_v1::Message {
            id: msg.id.clone(),
            channel_id: "pending".to_string(), // Will be set after decryption
            author_id: String::new(),          // Sealed sender - unknown until decrypted
            timestamp: msg.timestamp,
            content: Some(crate::proto::chat_v1::message::Content {
                r#type: Some(crate::proto::chat_v1::message::content::Type::SealedSender(
                    crate::proto::chat_v1::SealedSenderMessage {
                        sealed_body: msg.encrypted_envelope.clone(),
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        };

        let envelope_type = EnvelopeType::try_from(msg.envelope_type).unwrap_or(EnvelopeType::Unspecified);

        match pipeline.process(proto_msg) {
            PipelineResult::Ready(_) => {
                let _ = event_tx.send(InboxEvent::MessageReceived {
                    message_id: msg.id.clone(),
                    envelope_type,
                });
                let _ = event_tx.send(InboxEvent::MessageProcessed {
                    message_id: msg.id.clone(),
                });
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_processed += 1;
            }
            PipelineResult::Buffered => {
                // Message is out of order, will be delivered later
                let _ = event_tx.send(InboxEvent::MessageReceived {
                    message_id: msg.id.clone(),
                    envelope_type,
                });
                stats.write().messages_received += 1;
            }
            PipelineResult::Duplicate => {
                // Already seen, skip
                tracing::debug!(message_id = %msg.id, "Duplicate message skipped");
            }
            PipelineResult::Invalid(err) => {
                let _ = event_tx.send(InboxEvent::MessageFailed {
                    message_id: msg.id.clone(),
                    error: err.to_string(),
                });
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
            }
            PipelineResult::Dropped => {
                tracing::warn!(message_id = %msg.id, "Message dropped (buffer full)");
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
            }
        }
    }

    /// Subscribe to inbox events.
    ///
    /// Returns a receiver for all inbox events. Multiple subscribers are supported.
    pub fn subscribe(&self) -> broadcast::Receiver<InboxEvent> {
        self.event_tx.subscribe()
    }

    /// Get current inbox statistics.
    pub fn stats(&self) -> InboxStats {
        *self.stats.read()
    }

    /// Get current inbox state.
    pub fn state(&self) -> InboxState {
        *self.state.read()
    }

    /// Check if the provider is running.
    pub fn is_running(&self) -> bool {
        self.poller_handle.read().is_some()
    }

    /// Get a reference to the configuration.
    pub fn config(&self) -> &InboxConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_config_defaults() {
        let config = InboxConfig::default();
        assert_eq!(config.poll_interval, Duration::from_secs(5));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(30));
        assert_eq!(config.max_batch_size, 100);
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.ack_batch_size, 50);
    }

    #[test]
    fn test_inbox_config_builder() {
        let config = InboxConfig::builder()
            .poll_interval(Duration::from_secs(10))
            .poll_interval_idle(Duration::from_secs(60))
            .max_batch_size(50)
            .retry_attempts(5)
            .ack_batch_size(25)
            .build();

        assert_eq!(config.poll_interval, Duration::from_secs(10));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(60));
        assert_eq!(config.max_batch_size, 50);
        assert_eq!(config.retry_attempts, 5);
        assert_eq!(config.ack_batch_size, 25);
    }

    #[test]
    fn test_inbox_state_default() {
        let state = InboxState::default();
        assert_eq!(state, InboxState::Idle);
    }

    #[test]
    fn test_inbox_stats_default() {
        let stats = InboxStats::default();
        assert_eq!(stats.messages_received, 0);
        assert_eq!(stats.messages_processed, 0);
        assert_eq!(stats.messages_failed, 0);
        assert!(stats.last_poll_at.is_none());
        assert_eq!(stats.last_poll_count, 0);
    }

    #[test]
    fn test_inbox_event_variants() {
        // Test that all event variants can be constructed
        let events = vec![
            InboxEvent::MessageReceived {
                message_id: "msg-1".to_string(),
                envelope_type: EnvelopeType::SealedSender,
            },
            InboxEvent::MessageProcessed {
                message_id: "msg-1".to_string(),
            },
            InboxEvent::MessageFailed {
                message_id: "msg-1".to_string(),
                error: "validation failed".to_string(),
            },
            InboxEvent::PollCompleted {
                count: 10,
                has_more: true,
            },
            InboxEvent::PollFailed {
                error: "network error".to_string(),
            },
            InboxEvent::AcknowledgeFailed {
                message_ids: vec!["msg-1".to_string(), "msg-2".to_string()],
                error: "server error".to_string(),
            },
        ];

        assert_eq!(events.len(), 6);
    }

    #[test]
    fn test_inbox_error_display() {
        let err = InboxError::PollFailed("timeout".to_string());
        assert_eq!(err.to_string(), "poll failed: timeout");

        let err = InboxError::AuthFailed("invalid signature".to_string());
        assert_eq!(err.to_string(), "authentication failed: invalid signature");

        let err = InboxError::NotRunning;
        assert_eq!(err.to_string(), "provider not running");
    }
}
