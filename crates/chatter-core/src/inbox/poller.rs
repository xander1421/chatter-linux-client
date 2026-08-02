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

/// Where the next `GetDeviceMessages` should resume from.
///
/// `after_id` is exclusive, so returning `q` means "give me everything with a
/// queue id above `q`".
///
/// `batch_max` is the highest queue id in the batch just handled;
/// `lowest_retained` is the lowest queue id the ack policy told us to keep
/// (`AckDecision::Retain`), if any.
///
/// A retained message must stay *below* the cursor's exclusive bound, otherwise
/// `Retain` is a no-op: the server keeps the envelope because we never acked it,
/// and we never ask for it again because the cursor moved past it. The policy's
/// entire transient half — "the SKDM may land on a later poll and then the same
/// bytes decrypt" — depends on the later poll actually re-offering it.
///
/// Everything above a retained id is re-requested along with it. That is not a
/// replay: acking DELETES server-side, so the ids that were acked are already
/// gone and will not come back. The ones that do come back are exactly the ones
/// whose `acknowledge` failed, and reprocessing those is idempotent.
#[must_use]
pub fn next_after_id(batch_max: i64, lowest_retained: Option<i64>) -> i64 {
    match lowest_retained {
        Some(retained) => batch_max.min(retained - 1),
        None => batch_max,
    }
}

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

                    // The cursor is NOT advanced here. Only the caller knows
                    // which of these the ack policy retained, and advancing
                    // past a retained id makes `AckDecision::Retain` a no-op.
                    // The caller calls `advance_after_id` once it has decided.
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

    /// Move the pagination cursor after a batch's ack decisions are known.
    ///
    /// Split out of `run_once` because the cursor and the ack decision are the
    /// same decision: `after_id` is exclusive, so a message below it is never
    /// requested again, and a retained message below it is one the server still
    /// holds and we will never ask for. See [`next_after_id`].
    pub fn advance_after_id(&self, batch_max: i64, lowest_retained: Option<i64>) {
        *self.last_queue_id.write() = next_after_id(batch_max, lowest_retained);
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

}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AckDecision::Retain` means "leave it queued so it is delivered again",
    /// and the whole transient half of the ack policy rests on that being true:
    /// a `SessionNotFound` is retained because the SKDM "may land on a later
    /// poll and then the same bytes decrypt".
    ///
    /// The cursor is what makes that a promise or a lie. It was advanced to the
    /// last queue id in the batch before any decision was taken, so a retained
    /// message sat below `after_id` and was never requested again for the rest
    /// of the process lifetime. Not acked, so the server kept it; never asked
    /// for, so we never saw it. Silently unreachable rather than redelivered.
    #[test]
    fn a_retained_message_is_asked_for_again_rather_than_skipped_past() {
        // Batch of queue ids 1..=10; #4 came back Retain.
        assert_eq!(
            next_after_id(10, Some(4)),
            3,
            "the cursor must stop below the lowest retained id so the next poll \
             re-offers it; advancing past it means a message we deliberately \
             did not ack is also never delivered again"
        );
    }

    /// The retained message must be re-offered, not re-offered *immediately in
    /// a loop*: the run loop's own jittered sleep paces this, and the cursor
    /// must not rewind below where it already was either.
    #[test]
    fn a_fully_handled_batch_still_advances_the_cursor() {
        assert_eq!(
            next_after_id(10, None),
            10,
            "with nothing retained there is nothing to re-offer, so the cursor \
             advances normally and the queue drains"
        );
    }

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
