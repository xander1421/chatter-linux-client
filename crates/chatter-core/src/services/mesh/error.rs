//! Errors surfaced by [`MeshService`](super::MeshService).
//!
//! One error type for the whole composition root, in the shape the rest of the
//! service layer uses (`thiserror`, `#[from]` for the layers underneath). The
//! mesh stack is seven crates deep, so most variants are pass-throughs — the
//! ones that are not describe a decision this service makes.

use chatter_mesh::MeshError as WireError;
use chatter_mesh_link::LinkError;
use chatter_mesh_noise::NoiseError;
use chatter_mesh_nostr::NostrError;
use chatter_mesh_store::StoreError;

use crate::error::StorageError;

/// Everything [`MeshService`](super::MeshService) can fail with.
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    /// An operation was attempted before `start()` or after `shutdown()`.
    #[error("mesh service is not running")]
    NotRunning,

    /// This build has no radio: neither Linux BlueZ nor Android JNI. The
    /// service still constructs and the Nostr half still works, so this is a
    /// per-operation error rather than a construction failure.
    #[error("no mesh radio on this platform")]
    NoRadio,

    /// The seal key could not be read, so nothing the mesh retains could be
    /// written safely.
    ///
    /// Distinct from a hard failure on purpose: it is usually a locked keyring,
    /// which opens later. The mesh declines to start and keeps the existing
    /// sealed outbox intact rather than generating a new key over it.
    #[error("the mesh cannot seal what it retains: {0}")]
    SealerUnavailable(String),

    /// Nostr was not configured — no [`RelayConnector`] was supplied, so there
    /// is no way to reach a relay.
    ///
    /// [`RelayConnector`]: chatter_mesh_nostr::relay::RelayConnector
    #[error("no nostr relay connector configured")]
    NoRelayConnector,

    /// A geohash string that is not a geohash.
    #[error("invalid geohash: {0}")]
    InvalidGeohash(String),

    /// A DM was addressed to a peer whose transport cannot be determined from
    /// its ID namespace.
    #[error("cannot route to peer: {0}")]
    Unroutable(String),

    /// Empty message bodies never reach the wire: they cost airtime, cannot be
    /// rendered, and are indistinguishable from a decode bug on the far side.
    #[error("message content is empty")]
    EmptyContent,

    /// A file bigger than one mesh file transfer can carry.
    ///
    /// There is no chunking protocol above the fragmenter and no resume, so the
    /// TLV's own ceiling is the whole budget — and it is the *receiver's* decode
    /// limit too, which is why this is a refusal rather than a truncation:
    /// a byte over it produces a packet every peer drops.
    #[error("that file is {size} bytes; a mesh file transfer carries at most {max}")]
    FileTooLarge { size: u64, max: usize },

    /// A file could not be read from, or written to, the local disk.
    ///
    /// Carries the path because the two sides fail for opposite reasons — a send
    /// cannot read what the user picked, a receive cannot write to the
    /// attachments directory — and the error alone cannot tell them apart.
    #[error("mesh file i/o failed at {path}: {source}")]
    FileIo {
        path: String,
        source: std::io::Error,
    },

    /// A mesh file could not be encrypted for storage.
    #[error("mesh attachment encryption failed: {0}")]
    Crypto(#[from] crate::error::CryptoError),

    /// The background workers are gone — the service was shut down, or a task
    /// ended.
    #[error("mesh worker channel closed")]
    WorkerGone,

    #[error("mesh identity storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("mesh store error: {0}")]
    Store(#[from] StoreError),

    #[error("mesh wire error: {0}")]
    Wire(#[from] WireError),

    #[error("mesh noise error: {0}")]
    Noise(#[from] NoiseError),

    #[error("nostr error: {0}")]
    Nostr(#[from] NostrError),

    #[error("radio error: {0}")]
    Link(#[from] LinkError),
}
