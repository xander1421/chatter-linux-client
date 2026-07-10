//! Offline state.
//!
//! Connection status and offline queue state for UI binding.

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::network::connection::ConnectionState;
use crate::network::offline_queue::QueueStats;
use crate::state::Store;

/// Offline state data for connection and queue status.
#[derive(Clone, Debug)]
pub struct OfflineStateData {
    /// Current connection state.
    pub connection_state: ConnectionState,
    /// Derived convenience: whether currently connected to server.
    pub is_online: bool,
    /// Number of pending operations in the offline queue.
    pub queue_count: usize,
    /// Detailed queue statistics when available.
    pub queue_stats: Option<QueueStats>,
    /// When last successfully connected.
    pub last_connected: Option<DateTime<Utc>>,
    /// Current reconnection attempt number if reconnecting.
    pub reconnect_attempt: Option<u32>,
}

impl Default for OfflineStateData {
    fn default() -> Self {
        Self {
            connection_state: ConnectionState::Disconnected,
            is_online: false,
            queue_count: 0,
            queue_stats: None,
            last_connected: None,
            reconnect_attempt: None,
        }
    }
}

/// Reactive state for connection and offline queue status.
///
/// Provides a unified view of network connectivity and pending
/// operations for UI components to display connection status,
/// offline indicators, and queue information.
///
/// # Example
///
/// ```ignore
/// use chatter_core::state::OfflineState;
/// use chatter_core::network::connection::ConnectionState;
///
/// let state = OfflineState::new();
///
/// // Subscribe to changes
/// let mut rx = state.subscribe();
///
/// // Update connection state
/// state.set_connection_state(ConnectionState::Connected);
///
/// // Check status
/// assert!(state.is_online());
/// ```
#[derive(Clone)]
pub struct OfflineState {
    store: Store<OfflineStateData>,
}

impl OfflineState {
    /// Create a new OfflineState in disconnected state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(OfflineStateData::default()),
        }
    }

    /// Set the connection state.
    ///
    /// This updates both the connection_state and derived is_online fields,
    /// as well as reconnect_attempt if reconnecting.
    pub fn set_connection_state(&self, state: ConnectionState) {
        self.store.update(|data| {
            data.connection_state = state;
            data.is_online = state.is_connected();

            // Update reconnect attempt based on state
            match state {
                ConnectionState::Reconnecting { attempt } => {
                    data.reconnect_attempt = Some(attempt);
                }
                ConnectionState::Connected => {
                    data.reconnect_attempt = None;
                    // Update last_connected time when connection succeeds
                    data.last_connected = Some(Utc::now());
                }
                _ => {
                    data.reconnect_attempt = None;
                }
            }
        });
    }

    /// Get the current connection state.
    #[must_use]
    pub fn connection_state(&self) -> ConnectionState {
        self.store.get().connection_state
    }


    /// Check if currently connected (online).
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.store.get().is_online
    }

    /// Set the number of pending operations in the queue.
    pub fn set_queue_count(&self, count: usize) {
        self.store.update(|data| {
            data.queue_count = count;
        });
    }

    /// Get the number of pending operations in the queue.
    #[must_use]
    pub fn queue_count(&self) -> usize {
        self.store.get().queue_count
    }

    /// Set the detailed queue statistics.
    pub fn set_queue_stats(&self, stats: QueueStats) {
        self.store.update(|data| {
            data.queue_count = stats.total_operations;
            data.queue_stats = Some(stats);
        });
    }

    /// Get the detailed queue statistics if available.
    #[must_use]
    pub fn queue_stats(&self) -> Option<QueueStats> {
        self.store.get().queue_stats
    }

    /// Set the last connected time.
    pub fn set_last_connected(&self, time: DateTime<Utc>) {
        self.store.update(|data| {
            data.last_connected = Some(time);
        });
    }

    /// Get the last connected time.
    #[must_use]
    pub fn last_connected(&self) -> Option<DateTime<Utc>> {
        self.store.get().last_connected
    }

    /// Get the current reconnection attempt number if reconnecting.
    #[must_use]
    pub fn reconnect_attempt(&self) -> Option<u32> {
        self.store.get().reconnect_attempt
    }

    /// Subscribe to state changes (Arc-wrapped for efficiency).
    ///
    /// Returns a receiver that will receive updates when state changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<OfflineStateData>> {
        self.store.subscribe()
    }

    /// Clear state to disconnected with empty queue.
    pub fn clear(&self) {
        self.store.set(OfflineStateData::default());
    }
}

impl Default for OfflineState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::connection::FailReason;
    use std::collections::HashMap;
    use std::time::Duration;

    // ==================== Connection State Tests ====================

    #[test]
    fn test_initial_state() {
        let state = OfflineState::new();
        assert_eq!(state.connection_state(), ConnectionState::Disconnected);
        assert!(!state.is_online());
        assert_eq!(state.queue_count(), 0);
        assert!(state.queue_stats().is_none());
        assert!(state.last_connected().is_none());
        assert!(state.reconnect_attempt().is_none());
    }

    #[test]
    fn test_set_connection_state_connected() {
        let state = OfflineState::new();

        state.set_connection_state(ConnectionState::Connected);

        assert_eq!(state.connection_state(), ConnectionState::Connected);
        assert!(state.is_online());
        assert!(state.last_connected().is_some());
        assert!(state.reconnect_attempt().is_none());
    }

    #[test]
    fn test_set_connection_state_disconnected() {
        let state = OfflineState::new();

        // First connect
        state.set_connection_state(ConnectionState::Connected);
        assert!(state.is_online());

        // Then disconnect
        state.set_connection_state(ConnectionState::Disconnected);
        assert!(!state.is_online());
        assert_eq!(state.connection_state(), ConnectionState::Disconnected);
        // last_connected should still have the previous connection time
        assert!(state.last_connected().is_some());
    }

    #[test]
    fn test_set_connection_state_connecting() {
        let state = OfflineState::new();

        state.set_connection_state(ConnectionState::Connecting);

        assert_eq!(state.connection_state(), ConnectionState::Connecting);
        assert!(!state.is_online());
        assert!(state.reconnect_attempt().is_none());
    }

    #[test]
    fn test_set_connection_state_reconnecting() {
        let state = OfflineState::new();

        state.set_connection_state(ConnectionState::Reconnecting { attempt: 3 });

        assert_eq!(
            state.connection_state(),
            ConnectionState::Reconnecting { attempt: 3 }
        );
        assert!(!state.is_online());
        assert_eq!(state.reconnect_attempt(), Some(3));
    }

    #[test]
    fn test_set_connection_state_failed() {
        let state = OfflineState::new();

        state.set_connection_state(ConnectionState::Failed {
            reason: FailReason::AuthExpired,
        });

        assert!(state.connection_state().is_failed());
        assert!(!state.is_online());
    }

    #[test]
    fn test_reconnect_attempt_clears_on_connect() {
        let state = OfflineState::new();

        // Start reconnecting
        state.set_connection_state(ConnectionState::Reconnecting { attempt: 5 });
        assert_eq!(state.reconnect_attempt(), Some(5));

        // Successfully connect
        state.set_connection_state(ConnectionState::Connected);
        assert!(state.reconnect_attempt().is_none());
    }

    // ==================== Queue Tests ====================

    #[test]
    fn test_set_queue_count() {
        let state = OfflineState::new();

        assert_eq!(state.queue_count(), 0);

        state.set_queue_count(5);
        assert_eq!(state.queue_count(), 5);

        state.set_queue_count(0);
        assert_eq!(state.queue_count(), 0);
    }

    #[test]
    fn test_set_queue_stats() {
        let state = OfflineState::new();

        let mut ops_by_type = HashMap::new();
        ops_by_type.insert("SendMessage".to_string(), 3);
        ops_by_type.insert("SendReaction".to_string(), 2);

        let stats = QueueStats {
            total_operations: 5,
            oldest_operation_age: Some(Duration::from_secs(120)),
            operations_by_type: ops_by_type,
        };

        state.set_queue_stats(stats.clone());

        // queue_count should be updated from stats
        assert_eq!(state.queue_count(), 5);

        let retrieved_stats = state.queue_stats().unwrap();
        assert_eq!(retrieved_stats.total_operations, 5);
        assert_eq!(retrieved_stats.operations_by_type.get("SendMessage"), Some(&3));
    }

    // ==================== Last Connected Tests ====================

    #[test]
    fn test_set_last_connected() {
        let state = OfflineState::new();

        let time = Utc::now();
        state.set_last_connected(time);

        assert_eq!(state.last_connected(), Some(time));
    }

    #[test]
    fn test_last_connected_updated_on_connect() {
        let state = OfflineState::new();

        let before = Utc::now();
        state.set_connection_state(ConnectionState::Connected);
        let after = Utc::now();

        let last = state.last_connected().unwrap();
        assert!(last >= before);
        assert!(last <= after);
    }

    // ==================== Subscription Tests ====================

    #[tokio::test]
    async fn test_subscribe_receives_connection_updates() {
        let state = OfflineState::new();
        let mut rx = state.subscribe();

        state.set_connection_state(ConnectionState::Connected);

        let received = rx.recv().await.unwrap();
        assert!(received.is_online);
        assert_eq!(received.connection_state, ConnectionState::Connected);
    }

    #[tokio::test]
    async fn test_subscribe_receives_queue_updates() {
        let state = OfflineState::new();
        let mut rx = state.subscribe();

        state.set_queue_count(10);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.queue_count, 10);
    }

    // ==================== Clear Tests ====================

    #[test]
    fn test_clear() {
        let state = OfflineState::new();

        // Set up some state
        state.set_connection_state(ConnectionState::Connected);
        state.set_queue_count(5);
        state.set_last_connected(Utc::now());

        // Clear everything
        state.clear();

        assert_eq!(state.connection_state(), ConnectionState::Disconnected);
        assert!(!state.is_online());
        assert_eq!(state.queue_count(), 0);
        assert!(state.queue_stats().is_none());
        assert!(state.last_connected().is_none());
        assert!(state.reconnect_attempt().is_none());
    }

    // ==================== Clone Tests ====================

    #[test]
    fn test_clone_shares_state() {
        let state1 = OfflineState::new();
        let state2 = state1.clone();

        state1.set_connection_state(ConnectionState::Connected);

        assert!(state2.is_online());
        assert_eq!(state2.connection_state(), ConnectionState::Connected);
    }

    // ==================== Default Tests ====================

    #[test]
    fn test_default_impl() {
        let state = OfflineState::default();
        assert_eq!(state.connection_state(), ConnectionState::Disconnected);
        assert!(!state.is_online());
    }
}
