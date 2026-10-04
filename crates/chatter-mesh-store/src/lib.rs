//! Store and forward for the bitchat mesh.
//!
//! Everything that answers "the recipient is not here right now": a retained sender
//! outbox, couriers that deposit and take envelopes, gossip-synced public history,
//! and sealed drops parked on Nostr relays.
//!
//! The primitives live elsewhere — [`chatter_mesh`] has the GCS filter, the courier
//! envelope and the rotating recipient tag; [`chatter_mesh_noise`] has the seals.
//! This crate is the stateful layer: quotas, budgets, retries, and the decision tree
//! that picks between the mechanisms.
//!
//! # Guarantees this crate is responsible for
//!
//! These are stated properties of the protocol, not implementation preferences.
//! Breaking one turns a privacy claim into a false one:
//!
//! - **Nothing plaintext reaches disk.** [`storage::Storage`] takes sealed bytes and
//!   the sealing key never enters this crate, so the guarantee is structural rather
//!   than a matter of remembering.
//! - **The courier pool is bounded** per depositor and in total, and only
//!   signature-verified peers may deposit. A full pool refuses new mail rather
//!   than evicting what it already carries.
//! - **Couriers never multiply mail.** The store is deposit and take only: no
//!   spray to other couriers, no speculative relayed handover.
//! - **Handover requires an authenticated ingress link**, not merely a session for
//!   the claimed peer ID — a session outlives its link, and a replayed announce can
//!   rebind that ID to someone else's link.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod archive;
pub mod bridge_drop;
pub mod courier;
pub mod error;
pub mod outbox;
pub mod router;
pub mod storage;
pub mod sync;
pub mod transport;

pub use error::StoreError;
pub use storage::{MemoryStorage, Storage, StorageKey};
pub use transport::{PeerSnapshot, RouterTransport};

/// Retained mail expires after this long, matching the courier envelope lifetime.
pub const MESSAGE_TTL_SECS: u64 = 24 * 60 * 60;
/// Per-peer retained-message cap, FIFO-evicted.
pub const MAX_MESSAGES_PER_PEER: usize = 100;
/// Resend attempts against a weak reachability signal before giving up visibly.
pub const MAX_SEND_ATTEMPTS: u32 = 8;
/// Distinct couriers a single message may ride with. Receivers dedup by message ID,
/// so redundancy costs airtime but never correctness.
pub const MAX_COURIERS_PER_MESSAGE: usize = 3;
