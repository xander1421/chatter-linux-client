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

    /// Return a detached copy of the current connection state.
    pub fn get(&self) -> OfflineStateData {
        self.store.get()
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
        self.store.with(|s| s.connection_state.clone())
    }

    /// Get the current reconnection attempt number if reconnecting.
    #[must_use]
    pub fn reconnect_attempt(&self) -> Option<u32> {
        self.store.with(|s| s.reconnect_attempt)
    }

    /// Subscribe to state changes (Arc-wrapped for efficiency).
    ///
    /// Returns a receiver that will receive updates when state changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<OfflineStateData>> {
        self.store.subscribe()
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

    // ==================== Connection State Tests ====================

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

    // ==================== Clone Tests ====================

    // ==================== Default Tests ====================
}
