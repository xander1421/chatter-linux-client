//! Nostr transport for the bitchat mesh.
//!
//! Two jobs. Public geohash channels (kinds 20000/20001) are plaintext and
//! interoperate with any Nostr client. Private messages are gift-wrapped per
//! NIP-17/59 and interoperate with **bitchat only** — see below.
//!
//! # This crate does not implement NIP-44 v2
//!
//! Despite the `v2:` prefix and bitchat's own naming, its DM encryption diverges
//! from the published NIP-44 v2 specification on every axis: XChaCha20-Poly1305
//! instead of ChaCha20 + HMAC-SHA256, a 24-byte nonce instead of 32, a `"v2:"`
//! base64url payload instead of the versioned binary one, HKDF with an empty salt
//! and `info = "nip44-v2"` instead of the reverse, and no padding.
//!
//! We match bitchat, because talking to bitchat is the point. A spec-compliant
//! implementation would interoperate with every Nostr client except the one we are
//! targeting. This is not a bug and must not be "fixed".
//!
//! # Privacy invariant
//!
//! Presence heartbeats are broadcast only at geohash precision ≤ 5. Finer cells
//! would publish the user's location to anyone watching a relay. The builder
//! enforces it; call sites cannot opt out.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod builders;
pub mod crypto;
pub mod embed;
pub mod error;
pub mod event;
pub mod geohash;
pub mod identity;
pub mod types;
pub mod wrap;

#[cfg(feature = "client")]
pub mod relay;

pub use error::NostrError;
pub use types::{EventKind, GeohashLevel, NostrEvent};

/// HKDF info string for the DM key derivation. Note this is the *info*, not the
/// salt — the published NIP-44 v2 spec uses it the other way round.
pub const NIP44_HKDF_INFO: &[u8] = b"nip44-v2";
/// Prefix on every encrypted DM payload produced by bitchat.
pub const CIPHERTEXT_PREFIX: &str = "v2:";
/// Prefix marking a base64url-encoded `BitchatPacket` embedded in a DM rumor.
pub const EMBEDDED_PACKET_PREFIX: &str = "bitchat1:";
/// Label prefix that makes a mesh-bridge rendezvous identity unlinkable from the
/// geohash-chat identity for the same cell.
pub const BRIDGE_IDENTITY_LABEL: &str = "bridge|";
