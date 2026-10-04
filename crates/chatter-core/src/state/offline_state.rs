//! Offline state.
//!
//! Connection status for UI binding.

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::network::connection::ConnectionState;
use crate::state::Store;

/// Offline state data for connection status.
#[derive(Clone, Debug)]
pub struct OfflineStateData {
    /// Current connection state.
    pub connection_state: ConnectionState,
    /// Derived convenience: whether currently connected to server.
    pub is_online: bool,
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
            last_connected: None,
            reconnect_attempt: None,
        }
    }
}

/// Reactive state for connection status.
///
/// Provides a unified view of network connectivity for UI components
/// to display connection status and offline indicators.
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

    /// Clear state to disconnected.
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

    // ==================== Connection State Tests ====================

    #[test]
    fn test_initial_state() {
        let state = OfflineState::new();
        assert_eq!(state.connection_state(), ConnectionState::Disconnected);
        assert!(!state.is_online());
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

    // ==================== Clear Tests ====================

    #[test]
    fn test_clear() {
        let state = OfflineState::new();

        // Set up some state
        state.set_connection_state(ConnectionState::Connected);
        state.set_last_connected(Utc::now());

        // Clear everything
        state.clear();

        assert_eq!(state.connection_state(), ConnectionState::Disconnected);
        assert!(!state.is_online());
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
