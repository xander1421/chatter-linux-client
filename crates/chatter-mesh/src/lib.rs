//! bitchat mesh wire protocol — sans-IO.
//!
//! Bytes in, typed packet out, policy decision, bytes back. Nothing in this crate
//! opens a socket, touches a radio, reads a clock it wasn't handed, or spawns a
//! task. Callers in the transport and store-and-forward layers own all of that;
//! this crate owns the format and the rules.
//!
//! That split is deliberate. It makes the protocol testable without a Bluetooth
//! adapter, keeps the conformance suite pure data, and means the piece most exposed
//! to hostile input — the parser — has no ambient state to corrupt.
//!
//! # Wire compatibility
//!
//! The target is byte-exact interoperability with shipping bitchat clients, not a
//! protocol that merely resembles theirs. Where their source and their
//! documentation disagree, the source wins; several such disagreements are called
//! out in comments at the site where they matter.
//!
//! # Layout
//!
//! - [`packet`], [`codec`], [`padding`], [`compress`] — the frame itself
//! - [`tlv`] — the payload formats carried inside frames
//! - [`relay`], [`dedup`], [`fragment`], [`topology`] — forwarding policy
//! - [`gcs`], [`sync_flags`] — gossip-sync set reconciliation
//! - [`ids`] — peer and packet identity

#![forbid(unsafe_code)]
// The TLV decoders all share one shape: check a length or range, accept the
// field, and otherwise ignore it silently so an unknown or malformed optional
// never fails the whole packet. Collapsing those into match guards would move
// each condition away from the comment explaining which bitchat behaviour it
// reproduces, so the nested form is deliberate here.
#![allow(clippy::collapsible_match)]
// Production code must never panic on radio input. Tests are exempt: asserting a
// well-formed fixture decodes is exactly what `expect` is for.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod codec;
pub mod compress;
pub mod dedup;
pub mod error;
pub mod fragment;
pub mod gcs;
pub mod ids;
pub mod packet;
pub mod padding;
pub mod relay;
pub mod sync_flags;
pub mod tlv;
pub mod topology;

pub use error::MeshError;
pub use packet::{
    BROADCAST_RECIPIENT, BitchatPacket, MessageType, NoisePayloadType, SENDER_ID_LEN,
    SIGNATURE_LEN, decode_noise_payload, encode_noise_payload,
};
