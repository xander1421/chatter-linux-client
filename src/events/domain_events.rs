//! Domain events.
//!
//! Business domain events.

/// Application event types.
pub enum AppEvent {
    /// User logged in.
    LoggedIn,
    /// User logged out.
    LoggedOut,
    /// Session expired.
    SessionExpired,
    /// Message received.
    MessageReceived,
    /// Message sent.
    MessageSent,
    /// Connection established.
    Connected,
    /// Connection lost.
    Disconnected,
    /// Reconnecting.
    Reconnecting,
}
