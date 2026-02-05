//! Connection state machine.
//!
//! Connection state management with event broadcasting for UI updates.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

/// Failure reasons for terminal connection states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// Authentication token expired, needs re-login.
    AuthExpired,
    /// Device was unlinked from account.
    DeviceDelinked,
    /// Maximum retry attempts exceeded.
    MaxRetriesExceeded,
}

/// Connection state for the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Default)]
pub enum ConnectionState {
    /// Not connected to server.
    #[default]
    Disconnected,
    /// Attempting initial connection.
    Connecting,
    /// Successfully connected and authenticated.
    Connected,
    /// Reconnecting after connection loss.
    Reconnecting {
        /// Current retry attempt number (1-indexed).
        attempt: u32,
    },
    /// Terminal failure state requiring user action.
    Failed {
        /// Reason for the failure.
        reason: FailReason,
    },
}


impl ConnectionState {
    /// Check if this is a terminal failure state.
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Check if in any connecting state (initial or reconnecting).
    pub fn is_connecting(&self) -> bool {
        matches!(self, Self::Connecting | Self::Reconnecting { .. })
    }
}

/// Connection events for subscribers.
///
/// These events are broadcast when connection state changes,
/// allowing UI and other components to react appropriately.
#[derive(Clone, Debug)]
pub enum ConnectionEvent {
    /// State transitioned from old to new.
    StateChanged {
        /// Previous state.
        old: ConnectionState,
        /// New state.
        new: ConnectionState,
    },
    /// Connection established (ready to send/receive).
    Online,
    /// Connection lost (messages will be queued).
    Offline,
    /// Authentication required (token expired or invalid).
    AuthRequired,
}

/// State machine for managing connection lifecycle.
///
/// Tracks connection state and broadcasts events when state changes.
/// Thread-safe and can be shared across tasks.
///
/// # Example
///
/// ```
/// use chatter_linux::network::connection::{ConnectionStateMachine, ConnectionEvent};
///
/// let state_machine = ConnectionStateMachine::new();
/// let mut receiver = state_machine.subscribe();
///
/// // Transition to connecting
/// state_machine.connecting();
///
/// // Later, mark as connected
/// state_machine.connected();
///
/// // UI can listen for events
/// // while let Ok(event) = receiver.recv().await {
/// //     match event {
/// //         ConnectionEvent::Online => println!("Connected!"),
/// //         ConnectionEvent::Offline => println!("Disconnected!"),
/// //         _ => {}
/// //     }
/// // }
/// ```
pub struct ConnectionStateMachine {
    state: Arc<RwLock<ConnectionState>>,
    events: broadcast::Sender<ConnectionEvent>,
}

impl ConnectionStateMachine {
    /// Broadcast channel capacity.
    const CHANNEL_CAPACITY: usize = 16;

    /// Create a new ConnectionStateMachine in Disconnected state.
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(Self::CHANNEL_CAPACITY);
        Self {
            state: Arc::new(RwLock::new(ConnectionState::Disconnected)),
            events,
        }
    }

    /// Get the current connection state.
    pub fn state(&self) -> ConnectionState {
        *self.state.read()
    }

    /// Subscribe to connection events.
    ///
    /// Returns a receiver that will get all future connection events.
    /// The receiver should be polled in a loop to handle events.
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.events.subscribe()
    }

    /// Transition to a new state, emitting appropriate events.
    ///
    /// This is the low-level transition method. Prefer using the
    /// convenience methods like `connecting()`, `connected()`, etc.
    pub fn transition(&self, new_state: ConnectionState) {
        let old_state = {
            let mut state = self.state.write();
            let old = *state;
            *state = new_state;
            old
        };

        // Don't emit events if state didn't change
        if old_state == new_state {
            return;
        }

        // Always emit StateChanged
        let _ = self.events.send(ConnectionEvent::StateChanged {
            old: old_state,
            new: new_state,
        });

        // Emit semantic events based on transition
        match (&old_state, &new_state) {
            // Going online
            (ConnectionState::Connecting, ConnectionState::Connected)
            | (ConnectionState::Reconnecting { .. }, ConnectionState::Connected) => {
                let _ = self.events.send(ConnectionEvent::Online);
            }

            // Going offline
            (ConnectionState::Connected, ConnectionState::Disconnected)
            | (ConnectionState::Connected, ConnectionState::Reconnecting { .. })
            | (ConnectionState::Connected, ConnectionState::Failed { .. }) => {
                let _ = self.events.send(ConnectionEvent::Offline);
            }

            // Auth failure from any state
            (_, ConnectionState::Failed { reason }) => {
                if *reason == FailReason::AuthExpired || *reason == FailReason::DeviceDelinked {
                    let _ = self.events.send(ConnectionEvent::AuthRequired);
                }
            }

            _ => {}
        }
    }

    /// Mark as connecting (initial connection attempt).
    pub fn connecting(&self) {
        self.transition(ConnectionState::Connecting);
    }

    /// Mark as connected.
    pub fn connected(&self) {
        self.transition(ConnectionState::Connected);
    }

    /// Mark as disconnected.
    pub fn disconnected(&self) {
        self.transition(ConnectionState::Disconnected);
    }

    /// Mark as reconnecting with attempt count.
    pub fn reconnecting(&self, attempt: u32) {
        self.transition(ConnectionState::Reconnecting { attempt });
    }

    /// Mark as failed with reason.
    pub fn failed(&self, reason: FailReason) {
        self.transition(ConnectionState::Failed { reason });
    }

    /// Reset to disconnected state (for starting fresh).
    pub fn reset(&self) {
        self.transition(ConnectionState::Disconnected);
    }
}

impl Default for ConnectionStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for ConnectionStateMachine {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            events: self.events.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_state() {
        let sm = ConnectionStateMachine::new();
        assert_eq!(sm.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn test_connecting_transition() {
        let sm = ConnectionStateMachine::new();
        sm.connecting();
        assert_eq!(sm.state(), ConnectionState::Connecting);
    }

    #[test]
    fn test_connected_transition() {
        let sm = ConnectionStateMachine::new();
        sm.connecting();
        sm.connected();
        assert_eq!(sm.state(), ConnectionState::Connected);
        assert!(sm.state().is_connected());
    }

    #[test]
    fn test_disconnected_transition() {
        let sm = ConnectionStateMachine::new();
        sm.connected();
        sm.disconnected();
        assert_eq!(sm.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn test_reconnecting_transition() {
        let sm = ConnectionStateMachine::new();
        sm.connected();
        sm.reconnecting(1);
        assert_eq!(sm.state(), ConnectionState::Reconnecting { attempt: 1 });
        assert!(sm.state().is_connecting());
    }

    #[test]
    fn test_failed_transition() {
        let sm = ConnectionStateMachine::new();
        sm.failed(FailReason::AuthExpired);
        assert_eq!(
            sm.state(),
            ConnectionState::Failed {
                reason: FailReason::AuthExpired
            }
        );
        assert!(sm.state().is_failed());
    }

    #[tokio::test]
    async fn test_transition_emits_events() {
        let sm = ConnectionStateMachine::new();
        let mut rx = sm.subscribe();

        sm.connecting();

        let event = rx.recv().await.unwrap();
        match event {
            ConnectionEvent::StateChanged { old, new } => {
                assert_eq!(old, ConnectionState::Disconnected);
                assert_eq!(new, ConnectionState::Connecting);
            }
            _ => panic!("Expected StateChanged event"),
        }
    }

    #[tokio::test]
    async fn test_online_event_on_connect() {
        let sm = ConnectionStateMachine::new();
        let mut rx = sm.subscribe();

        sm.connecting();
        let _ = rx.recv().await; // StateChanged

        sm.connected();
        let event1 = rx.recv().await.unwrap();
        let event2 = rx.recv().await.unwrap();

        // Should get StateChanged and Online
        let events = vec![event1, event2];
        assert!(events
            .iter()
            .any(|e| matches!(e, ConnectionEvent::Online)));
        assert!(events
            .iter()
            .any(|e| matches!(e, ConnectionEvent::StateChanged { .. })));
    }

    #[tokio::test]
    async fn test_offline_event_on_disconnect() {
        let sm = ConnectionStateMachine::new();
        let mut rx = sm.subscribe();

        sm.connected();
        let _ = rx.recv().await; // StateChanged + Online

        // Clear Online event
        let _ = rx.try_recv();

        sm.disconnected();

        // Collect events
        let mut saw_offline = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, ConnectionEvent::Offline) {
                saw_offline = true;
            }
        }
        assert!(saw_offline);
    }

    #[tokio::test]
    async fn test_auth_required_on_auth_failure() {
        let sm = ConnectionStateMachine::new();
        let mut rx = sm.subscribe();

        sm.failed(FailReason::AuthExpired);

        // Collect events
        let mut saw_auth_required = false;
        let event1 = rx.recv().await.unwrap();
        if matches!(event1, ConnectionEvent::AuthRequired) {
            saw_auth_required = true;
        }
        if let Ok(event2) = rx.try_recv() {
            if matches!(event2, ConnectionEvent::AuthRequired) {
                saw_auth_required = true;
            }
        }
        assert!(saw_auth_required);
    }

    #[test]
    fn test_no_event_on_same_state() {
        let sm = ConnectionStateMachine::new();
        let mut rx = sm.subscribe();

        // Already disconnected, transition to disconnected again
        sm.disconnected();

        // Should not receive any event
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_state_helper_methods() {
        assert!(ConnectionState::Connected.is_connected());
        assert!(!ConnectionState::Disconnected.is_connected());

        assert!(ConnectionState::Connecting.is_connecting());
        assert!(ConnectionState::Reconnecting { attempt: 1 }.is_connecting());
        assert!(!ConnectionState::Connected.is_connecting());

        assert!(ConnectionState::Failed {
            reason: FailReason::AuthExpired
        }
        .is_failed());
        assert!(!ConnectionState::Connected.is_failed());
    }

    #[test]
    fn test_clone_shares_state() {
        let sm1 = ConnectionStateMachine::new();
        let sm2 = sm1.clone();

        sm1.connecting();
        assert_eq!(sm2.state(), ConnectionState::Connecting);
    }

    #[test]
    fn test_reset() {
        let sm = ConnectionStateMachine::new();
        sm.connected();
        sm.reset();
        assert_eq!(sm.state(), ConnectionState::Disconnected);
    }
}
