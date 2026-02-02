//! Ephemeral message domain entity.
//!
//! Represents self-destructing messages with TTL configuration.

/// Ephemeral (self-destructing) message.
pub struct EphemeralMessage;

/// Message TTL (time-to-live) options.
pub enum MessageTTL {
    /// 30 seconds.
    Seconds30,
    /// 1 minute.
    Minutes1,
    /// 5 minutes.
    Minutes5,
    /// 1 hour.
    Hours1,
    /// 24 hours.
    Hours24,
}
