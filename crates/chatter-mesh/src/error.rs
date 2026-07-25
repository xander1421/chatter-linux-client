//! Error type for the mesh wire layer.
//!
//! Every decode path in this crate is total: malformed input returns `Err`, never
//! panics and never allocates on an attacker-chosen length before validating it.
//! This crate parses unauthenticated radio input from strangers, so the parser is
//! the whole attack surface.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MeshError {
    /// Input ended before a required field was fully read.
    #[error("truncated input: needed {needed} more byte(s) at offset {offset}")]
    Truncated { offset: usize, needed: usize },

    /// Packet version byte was neither 1 nor 2.
    #[error("unsupported packet version {0}")]
    UnsupportedVersion(u8),

    /// A length field exceeded the protocol's hard cap.
    #[error("declared length {declared} exceeds maximum {max}")]
    LengthExceeded { declared: usize, max: usize },

    /// Compressed payload claimed a decompressed size that would make the ratio
    /// implausible — a decompression bomb. bitchat rejects above 50_000:1.
    #[error("suspicious compression ratio {ratio}:1")]
    CompressionRatio { ratio: u64 },

    /// zlib/deflate stream could not be decoded, or produced the wrong length.
    #[error("decompression failed")]
    Decompression,

    /// A TLV field carried a type byte the decoder does not know and could not skip.
    #[error("malformed TLV at offset {offset}")]
    MalformedTlv { offset: usize },

    /// A TLV value that must be UTF-8 was not.
    #[error("invalid UTF-8 in field {field}")]
    InvalidUtf8 { field: &'static str },

    /// A fixed-width field carried the wrong number of bytes.
    #[error("field {field} had length {actual}, expected {expected}")]
    BadFieldLength {
        field: &'static str,
        actual: usize,
        expected: usize,
    },

    /// A required TLV field was absent.
    #[error("missing required field {0}")]
    MissingField(&'static str),

    /// The value is structurally valid but outside what the protocol permits
    /// (route longer than 255 hops, empty route hop, oversized envelope, ...).
    #[error("value out of range: {0}")]
    OutOfRange(&'static str),
}
