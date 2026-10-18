//! The bitchat binary packet framing.
//!
//! Ported from `BitFoundation/BinaryProtocol.swift`. This is the crate's entire
//! attack surface: [`decode`] runs on unauthenticated bytes handed to us by a
//! stranger's radio, before any signature or Noise session has vouched for
//! anything. It therefore length-checks every read, bounds every allocation on a
//! constant rather than on a field the sender chose, and never panics.
//!
//! # Wire format
//!
//! ```text
//! Header — 14 bytes (v1) / 16 bytes (v2), all integers big-endian:
//!   version:1  type:1  ttl:1  timestamp:8  flags:1  payloadLength:2 (v1) | 4 (v2)
//!
//! Body, in order, each present only when its flag is set:
//!   senderID:8
//!   recipientID:8      HAS_RECIPIENT
//!   routeCount:1 + hops:8*n   HAS_ROUTE   (v2 only)
//!   originalSize:2|4   IS_COMPRESSED
//!   payload:*
//!   signature:64       HAS_SIGNATURE
//! ```
//!
//! # Details that silently break interop
//!
//! * **`payloadLength` excludes the route bytes but includes the original-size
//!   field** when `IS_COMPRESSED` is set. Route bytes are framed but unaccounted.
//! * **`HAS_ROUTE` is v2-only.** A v1 frame that sets the bit must have the bit
//!   *ignored* — not rejected, and not parsed as a route. Rejecting partitions the
//!   mesh; parsing desynchronises the cursor and corrupts the payload.
//! * **Compression is raw DEFLATE at a 100-byte threshold** — see [`crate::compress`].
//! * **Trailing bytes are tolerated.** Swift's final bounds check is
//!   `offset <= count`, never `==`. This is what lets a *padded* frame decode on
//!   the first pass, with the pad bytes simply sitting unread past the signature.
//! * **[`decode`] parses the raw bytes first and only then retries after
//!   unpadding.** Reversing that order corrupts any payload whose final byte
//!   happens to look like a PKCS#7 marker.

use crate::packet::{SENDER_ID_LEN, SIGNATURE_LEN};

/// v1 header: version, type, ttl, timestamp(8), flags, payloadLength(2).
pub const V1_HEADER_SIZE: usize = 14;
/// v2 header: as v1 but with a 4-byte payloadLength.
pub const V2_HEADER_SIZE: usize = 16;
/// Recipient IDs are the same fixed width as sender IDs.
pub const RECIPIENT_ID_LEN: usize = SENDER_ID_LEN;
/// Byte offset of the flags field, identical in both versions.
///
/// Mirrors `BinaryProtocol.Offsets.flags`: after version(1) + type(1) + ttl(1) +
/// timestamp(8).
pub const FLAGS_OFFSET: usize = 11;
/// The hop count is a single byte, so a route cannot exceed 255 hops.
pub const MAX_ROUTE_HOPS: usize = 255;
/// Decompression above this ratio is treated as a bomb and rejected.
pub const MAX_COMPRESSION_RATIO: f64 = 50_000.0;

/// Header flag bits.
///
/// Bits 5-7 are unassigned. They are preserved by neither encoder nor decoder —
/// bitchat masks the bits it knows and drops the rest, and so do we.
pub mod flags {
    /// A recipient ID field follows the sender ID.
    pub const HAS_RECIPIENT: u8 = 0x01;
    /// A 64-byte Ed25519 signature trails the payload.
    pub const HAS_SIGNATURE: u8 = 0x02;
    /// The payload is raw-deflated and prefixed with its original size.
    pub const IS_COMPRESSED: u8 = 0x04;
    /// A source route follows the recipient. **v2 only** — ignored on v1.
    pub const HAS_ROUTE: u8 = 0x08;
    /// Request-Sync Response: exempt from the freshness window.
    pub const IS_RSR: u8 = 0x10;
}

// --- FileTransferLimits.maxFramedFileBytes, recomputed rather than guessed. ---
// Ported from `BitFoundation/FileTransferLimits.swift`. This is the decoder's
// pre-allocation ceiling, so it has to match theirs exactly or we reject frames
// they consider legal.
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// `fileName` + `mimeType` TLVs, each with a 16-bit length.
const MAX_METADATA_BYTES: usize = (u16::MAX as usize) * 2;
/// TLV tags and lengths, plus the metadata itself.
const TLV_ENVELOPE_OVERHEAD: usize = 18 + MAX_METADATA_BYTES;
/// Worst-case binary framing: v2 header + sender + recipient + signature.
const BINARY_ENVELOPE_OVERHEAD: usize =
    V2_HEADER_SIZE + SENDER_ID_LEN + RECIPIENT_ID_LEN + SIGNATURE_LEN;

/// Largest `payloadLength` the decoder will honour: 1_179_760 bytes.
///
/// Checked *before* any allocation sized from the frame — this is an anti-DoS
/// bound, not tidiness. A 4-byte v2 length field would otherwise let a 16-byte
/// frame ask us to reserve 4 GiB.
pub const MAX_FRAMED_FILE_BYTES: usize =
    MAX_PAYLOAD_BYTES + TLV_ENVELOPE_OVERHEAD + BINARY_ENVELOPE_OVERHEAD;

const fn header_size(version: u8) -> Option<usize> {
    match version {
        1 => Some(V1_HEADER_SIZE),
        2 => Some(V2_HEADER_SIZE),
        _ => None,
    }
}

/// v2 widened the length fields — both the header's `payloadLength` and the
/// compressed payload's original-size prefix — from 2 bytes to 4.
const fn length_field_size(version: u8) -> usize {
    if version == 2 { 4 } else { 2 }
}

mod decode;
mod encode;

pub use decode::decode;
pub use encode::encode;

#[cfg(test)]
mod tests;
