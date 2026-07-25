//! Errors for the store-and-forward layer.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// The durable blob could not be read or written.
    ///
    /// A *missing* blob is not this — implementations return `Ok(None)` for that,
    /// since a first run and a run after a panic wipe are both normal.
    #[error("storage {operation} failed for {key}: {reason}")]
    Storage {
        operation: &'static str,
        key: &'static str,
        reason: String,
    },

    /// A persisted blob could not be decoded. Treated as "start empty" by callers
    /// rather than as a fatal condition: losing retained mail is bad, refusing to
    /// start is worse.
    #[error("persisted {0} is malformed")]
    Malformed(&'static str),

    /// An internal lock was poisoned by a panic in another thread.
    #[error("storage lock poisoned")]
    StoragePoisoned,

    /// A courier deposit was refused. Not a failure — the pool is bounded on purpose
    /// and refusing is how the bound is enforced.
    #[error("courier deposit refused: {0}")]
    DepositRefused(&'static str),

    /// The recipient's Noise static key is unknown, so nothing can be sealed to them.
    #[error("no known static key for the recipient")]
    UnknownRecipientKey,

    /// Propagated from the wire layer.
    #[error("wire encoding failed: {0}")]
    Wire(#[from] chatter_mesh::MeshError),

    /// Propagated from the crypto layer.
    #[error("seal failed: {0}")]
    Seal(#[from] chatter_mesh_noise::NoiseError),
}
