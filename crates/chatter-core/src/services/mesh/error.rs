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
