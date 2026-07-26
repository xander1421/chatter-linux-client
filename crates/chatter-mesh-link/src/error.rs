//! Errors for the link layer.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LinkError {
    /// The adapter is missing, powered off, or was taken away mid-session.
    #[error("bluetooth adapter unavailable: {0}")]
    AdapterUnavailable(String),

    /// A connect attempt ran past the radio layer's connect deadline.
    ///
    /// Kept apart from [`Self::ConnectFailed`] because the scheduler treats them
    /// very differently: an outright failure costs the candidate score, while a
    /// timeout earns [`TIMEOUT_DISCOVERY_IGNORE_MS`] of being left alone. On
    /// BlueZ the difference is load-bearing — abandoning a pending connect makes
    /// it abort, and the abort cascades into the *next* attempt as
    /// `le-connection-abort-by-local`, so retrying quickly after a timeout is
    /// how one slow peer becomes a run of failures.
    ///
    /// [`TIMEOUT_DISCOVERY_IGNORE_MS`]: crate::scheduler::TIMEOUT_DISCOVERY_IGNORE_MS
    #[error("connect to {address} timed out")]
    ConnectTimedOut { address: String },

    /// A connect attempt failed outright.
    #[error("connect to {address} failed: {reason}")]
    ConnectFailed { address: String, reason: String },

    /// The link existed once but is gone now. Distinct from a write failure: the
    /// caller should drop its state for this link rather than retry.
    #[error("link {0} is no longer connected")]
    LinkGone(String),

    /// A write was rejected by the controller. Retryable — BLE queues fill
    /// routinely, and there is no retransmission underneath us.
    #[error("write rejected on link {0}")]
    WriteRejected(String),

    /// A payload exceeded what this link can carry in one write.
    #[error("payload of {actual} bytes exceeds the {max}-byte link limit")]
    PayloadTooLarge { actual: usize, max: usize },

    /// Advertising or GATT server registration failed.
    #[error("could not publish the peripheral role: {0}")]
    PeripheralSetup(String),

    /// The underlying stack reported something we cannot classify.
    #[error("link layer error: {0}")]
    Backend(String),
}
