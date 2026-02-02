//! Connection state machine.
//!
//! Connection state management and transitions.

/// Connection state.
pub enum ConnectionState {
    /// Not connected.
    Disconnected,
    /// Attempting to connect.
    Connecting,
    /// Successfully connected.
    Connected,
    /// Reconnecting after disconnection.
    Reconnecting,
}

/// Connection state machine.
pub struct ConnectionStateMachine;
