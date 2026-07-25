//! Noise sessions, one-way seals, and Ed25519 signing for the bitchat mesh.
//!
//! Sans-IO, like [`chatter_mesh`]: a handshake is a state machine, not a socket.
//! Key custody is out of scope — every entry point takes key material as a
//! parameter, so this crate works with the fixed keys the specification vectors
//! require and leaves keychain binding to the integration layer.
//!
//! # Why `snow` and not a port
//!
//! bitchat implements Noise by hand, but validates that implementation against the
//! canonical specification vectors (`bitchatTests/Noise/NoiseTestVectors.json` is the
//! cacophony fixture set). A spec-conformant library therefore interoperates with it
//! by construction, and `tests/noise_vectors.rs` proves it against the same file.
//!
//! # What is genuinely bitchat-specific
//!
//! Everything in [`seal`] and [`sign`]: the two domain-separating prologues, the
//! one-shot `X` seal framing, and the canonical announce bytes. Those have no
//! upstream vectors and are where the care belongs.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod identity;
pub mod prekey;
pub mod seal;
pub mod session;
pub mod sign;

pub use error::NoiseError;
pub use identity::{MeshIdentity, fingerprint_of};

/// Noise protocol name for interactive sessions.
pub const NOISE_XX_PROTOCOL: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
/// Noise protocol name for one-shot seals.
pub const NOISE_X_PROTOCOL: &str = "Noise_X_25519_ChaChaPoly_SHA256";

/// Domain separation for courier envelopes sealed to a peer's static key.
pub const COURIER_PROLOGUE: &[u8] = b"bitchat-courier-v1";
/// Prefix for prekey-sealed envelopes; the full prologue appends the prekey ID
/// as a big-endian u32 so a ciphertext cannot be replayed against another prekey.
pub const PREKEY_PROLOGUE_PREFIX: &[u8] = b"bitchat-prekey-v1";
/// Context string mixed into the canonical announce signing bytes.
pub const ANNOUNCE_CONTEXT: &[u8] = b"bitchat-announce-v1";
/// Context string covering a signed prekey bundle.
///
/// Defined in [`prekey`] where it is used; re-exported here so all four domain
/// separators are discoverable in one place. Distinct from
/// [`PREKEY_PROLOGUE_PREFIX`] on purpose: that one separates seal transcripts,
/// this one separates bundle signatures. Collapsing them would let a signature
/// from one construction be replayed into the other.
pub use prekey::PREKEY_BUNDLE_CONTEXT;
