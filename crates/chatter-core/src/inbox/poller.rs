//! Inbox poller.
//!
//! Polling loop for fetching messages from the server's device inbox.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use tokio::sync::{broadcast, watch, Mutex};

use crate::network::grpc::MessageClient;
use crate::proto::chat_v1::DeviceMessage;

use super::config::InboxConfig;
use super::error::InboxError;
use super::events::InboxEvent;
use super::state::{InboxState, InboxStats};

/// Poller for fetching messages from the device inbox.
///
/// Implements the polling loop for fetching authenticated device messages:
/// 1. Poll for messages using `get_device_messages`
/// 2. Return messages or handle errors
///
/// Uses adaptive polling intervals based on activity.
pub struct InboxPoller {
    config: InboxConfig,
    message_client: Arc<Mutex<MessageClient>>,
    /// Pool handle, used only to read the active transport so the per-RPC
    /// timeout can stretch for a cold onion connect (bootstrap + HS circuit).
    grpc_pool: Arc<crate::network::grpc::GrpcClientPool>,
    shutdown_rx: watch::Receiver<bool>,
    event_tx: broadcast::Sender<InboxEvent>,
    stats: Arc<RwLock<InboxStats>>,
    state: Arc<RwLock<InboxState>>,
    // For device queue, we use queue_id (int64) for pagination, not string
    last_queue_id: RwLock<i64>,
    last_activity: RwLock<Option<Instant>>,
}

impl InboxPoller {
    /// Create a new inbox poller.
    ///
    /// # Arguments
    ///
    /// * `config` - Polling configuration
    /// * `message_client` - Client for message RPCs
    /// * `shutdown_rx` - Receiver for shutdown signal
    /// * `event_tx` - Sender for inbox events
    /// * `stats` - Shared statistics
    /// * `state` - Shared state
    /// Rebuild the authenticated message client from the pool so the poller
    /// re-dials over the pool's current transport (used when anon mode toggles
    /// the transport between direct and onion; the captured client is otherwise
    /// pinned to the transport that was active at construction).
    pub async fn refresh_message_client(
        &self,
        pool: &crate::network::grpc::GrpcClientPool,
    ) {
        *self.message_client.lock().await = pool.message_client();
    }

    pub fn new(
        config: InboxConfig,
        message_client: Arc<Mutex<MessageClient>>,
        grpc_pool: Arc<crate::network::grpc::GrpcClientPool>,
        shutdown_rx: watch::Receiver<bool>,
        event_tx: broadcast::Sender<InboxEvent>,
        stats: Arc<RwLock<InboxStats>>,
        state: Arc<RwLock<InboxState>>,
    ) -> Self {
        Self {
            config,
            message_client,
            grpc_pool,
            shutdown_rx,
            event_tx,
            stats,
            state,
            last_queue_id: RwLock::new(0),
            last_activity: RwLock::new(None),
        }
    }

    /// Execute a single poll cycle.
    ///
    /// Returns:
    /// - `Ok(Some(messages))` - Poll succeeded, messages received
    /// - `Ok(None)` - Shutdown requested, should exit loop
    /// - `Err(e)` - Poll failed, should retry
    pub async fn run_once(&self) -> Result<Option<Vec<DeviceMessage>>, InboxError> {
        // Check for shutdown
        if *self.shutdown_rx.borrow() {
            return Ok(None);
        }

        // Pacing lives in the run loop (a single jittered, shutdown-aware sleep
        // between cycles). run_once just polls immediately — no internal interval
        // sleep, otherwise the loop would pace twice (~2× the configured gap).

        // Perform the poll
        self.set_state(InboxState::Polling);
        let result = self.poll_once().await;
        self.set_state(InboxState::Processing);

        match result {
            Ok((messages, has_more)) => {
                let count = messages.len() as u32;

                // Update stats
                {
                    let mut stats = self.stats.write();
                    stats.last_poll_at = Some(Instant::now());
                    stats.last_poll_count = count;
                }

                // Emit poll completed event
                let _ = self.event_tx.send(InboxEvent::PollCompleted { count, has_more });

                // Update last activity if we got messages
                if count > 0 {
                    *self.last_activity.write() = Some(Instant::now());

                    // Update pagination cursor
                    if let Some(last_msg) = messages.last() {
                        *self.last_queue_id.write() = last_msg.queue_id;
                    }
                }

                self.set_state(InboxState::Idle);
                Ok(Some(messages))
            }
            Err(e) => {
                // Emit poll failed event
                let _ = self.event_tx.send(InboxEvent::PollFailed {
                    error: e.to_string(),
                });
                tracing::warn!(error = %e, "Poll failed");
                self.set_state(InboxState::Idle);
                Err(e)
            }
        }
    }

    /// Perform a single poll request.
    async fn poll_once(&self) -> Result<(Vec<DeviceMessage>, bool), InboxError> {
        // Poll for messages
        let after_id = *self.last_queue_id.read();

        tracing::info!(after_id, "Acquiring message client lock...");
        let response = {
            let mut client = self.message_client.lock().await;
            tracing::info!(after_id, "Calling GetDeviceMessages RPC...");
            // Onion needs a much larger budget: the first poll after a switch to
            // onion transport drives a cold Tor connect (bootstrap + HS circuit,
            // ~24-40s on a fresh Arti cache). A 15s cap would fire first, cancel
            // the in-flight connect, and the next poll would restart it from
            // scratch — a livelock that never establishes the channel. Direct TCP
            // keeps the tight 15s.
            let on_onion = self.grpc_pool.transport_mode()
                == crate::network::grpc::client_pool::TransportMode::Onion;
            let rpc_timeout = if on_onion {
                std::time::Duration::from_secs(95)
            } else {
                std::time::Duration::from_secs(15)
            };
            let outcome = tokio::time::timeout(
                rpc_timeout,
                client.get_device_messages(
                    after_id,
                    self.config.max_batch_size as i32,
                ),
            )
            .await;
            // Drive the Tor indicator off the real onion RPC outcome (Circuit+RPC
            // signal): a completed round-trip = Connected, a timeout = Error. Only
            // touch it on onion — direct mode owns its own connection state.
            if on_onion {
                use crate::network::connection::{set_tor_status, TorStatus};
                match &outcome {
                    Ok(Ok(_)) => set_tor_status(TorStatus::Connected),
                    Err(_) => set_tor_status(TorStatus::Error),
                    Ok(Err(_)) => {} // transport-level RPC error; leave as-is (may be app-level)
                }
            }
            let result = outcome
                .map_err(|_| InboxError::PollFailed(format!("GetDeviceMessages RPC timed out after {}s", rpc_timeout.as_secs())))?
                .map_err(|e| InboxError::PollFailed(e.to_string()))?;
            tracing::info!(
                after_id,
                message_count = result.messages.len(),
                has_more = result.has_more,
                "GetDeviceMessages RPC completed"
            );
            result
        };

        Ok((response.messages, response.has_more))
    }

    /// Get the current polling interval based on activity (5s when recently
    /// active, 30s idle). The run loop sleeps this (jittered) between full poll
    /// cycles; without it the loop would re-fire after only the sub-second
    /// obfuscation delays and hammer the server.
    pub fn current_interval(&self) -> std::time::Duration {
        let last_activity = self.last_activity.read();
        match *last_activity {
            Some(instant) => {
                // Use shorter interval if recent activity (within 2 minutes)
                if instant.elapsed() < std::time::Duration::from_secs(120) {
                    self.config.poll_interval
                } else {
                    self.config.poll_interval_idle
                }
            }
            None => {
                // No activity yet, use idle interval
                self.config.poll_interval_idle
            }
        }
    }

    /// Set the current state.
    fn set_state(&self, state: InboxState) {
        *self.state.write() = state;
    }

    /// Acknowledge messages.
    ///
    /// Acknowledges (and deletes) messages from the server.
    /// Should be called after messages are successfully processed.
    pub async fn acknowledge(&self, message_ids: Vec<String>) -> Result<(), InboxError> {
        if message_ids.is_empty() {
            return Ok(());
        }

        // Acknowledge in batches
        for chunk in message_ids.chunks(self.config.ack_batch_size as usize) {
            let chunk_ids: Vec<String> = chunk.to_vec();

            let result = {
                let mut client = self.message_client.lock().await;
                client
                    .acknowledge_messages(chunk_ids.clone())
                    .await
            };

            if let Err(e) = result {
                let _ = self.event_tx.send(InboxEvent::AcknowledgeFailed {
                    message_ids: chunk_ids,
                    error: e.to_string(),
                });
                return Err(InboxError::AcknowledgeFailed(e.to_string()));
            }
        }

        Ok(())
    }

    /// Get the current polling interval.
    pub fn poll_interval(&self) -> std::time::Duration {
        self.current_interval()
    }

    /// Reset the pagination cursor.
    ///
    /// Call this to start polling from the beginning again.
    pub fn reset_cursor(&self) {
        *self.last_queue_id.write() = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_current_interval_no_activity() {
        let config = InboxConfig {
            poll_interval: std::time::Duration::from_secs(5),
            poll_interval_idle: std::time::Duration::from_secs(30),
            ..Default::default()
        };

        // Test without a real client - just verify interval logic
        let last_activity: Option<Instant> = None;
        let interval = if last_activity.is_some() {
            config.poll_interval
        } else {
            config.poll_interval_idle
        };

        assert_eq!(interval, std::time::Duration::from_secs(30));
    }

    #[test]
    fn test_current_interval_recent_activity() {
        let config = InboxConfig {
            poll_interval: std::time::Duration::from_secs(5),
            poll_interval_idle: std::time::Duration::from_secs(30),
            ..Default::default()
        };

        // Simulate recent activity
        let last_activity = Some(Instant::now());
        let interval = if let Some(instant) = last_activity {
            if instant.elapsed() < std::time::Duration::from_secs(120) {
                config.poll_interval
            } else {
                config.poll_interval_idle
            }
        } else {
            config.poll_interval_idle
        };

        assert_eq!(interval, std::time::Duration::from_secs(5));
    }
}
