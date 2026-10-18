//! Chunked file transfer: the receive spool and the send window.
//!
//! The wire types live in [`chatter_mesh::tlv::file_stream`] and are sans-IO.
//! This is the half that touches disk, and the whole point of it is what it
//! *never* does: hold an assembled payload in memory. A chunk goes straight into
//! a temp file at `index * chunk_size` and is forgotten; the assembled file is
//! read back exactly once, by the streaming encryptor, on its way into the
//! attachments directory.
//!
//! # Why a persisted bitmap
//!
//! Resume is the feature. On this radio a transfer that drops at 90% is the
//! normal case — rotating RPAs, `le-connection-abort-by-local`, a duty-cycled
//! scan — and starting again from zero turns a slow transfer into an impossible
//! one. Tracking received chunks in memory would give resume across a *link*
//! drop and lose it across a process restart, which is precisely the moment it
//! is most wanted: the app was killed, the file is still half there, and the only
//! record of which half was in a `HashMap`.
//!
//! So the bitmap goes on disk next to the partial file, and the manifest with it
//! — a bitmap with no manifest cannot say what the bits mean.
//!
//! # Why the digest is checked before anything reads the file
//!
//! Each chunk is signature-verified in its own right (see
//! `BleRuntime::on_file_chunk`), which authenticates *who* sent each chunk. It
//! does not establish that the concatenation is the file the manifest described:
//! a chunk written at the wrong offset, a manifest and a body from two different
//! versions of a file, a torn write. The whole-file SHA-256 is what closes that,
//! and it has to be checked before the streaming encryptor reads a byte, because
//! after that the bytes are inside the attachments directory and something will
//! offer them to the user.
//!
//! A mismatch **discards and does not retry**. Every chunk passed its packet
//! signature, so there is no chunk to blame and no chunk to re-request;
//! re-requesting would ask for the same bytes and fail the same way, forever.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use chatter_mesh::tlv::file_stream::TRANSFER_ID_LEN;
use sha2::{Digest, Sha256};

use super::error::MeshError;
use crate::services::attachment::attachments_dir;

/// A transfer identifier, 8 bytes.
pub type TransferId = [u8; TRANSFER_ID_LEN];

/// How long a partial transfer is kept without progress before it is abandoned.
///
/// An hour, which is deliberately far longer than any link outage: the whole
/// point of persisting the bitmap is that the transfer survives things that last
/// minutes. What it bounds is disk — a sender that vanishes mid-transfer would
/// otherwise leave its partial file in the spool forever.
pub const TRANSFER_ABANDON_SECS: u64 = 3600;

/// How long a transfer may go without a new chunk before the receiver asks for
/// what is missing.
///
/// Long enough that it is not competing with chunks still in flight — a 32 KiB
/// chunk is ~12 s of wire at the pessimistic goodput — and short enough that a
/// stalled transfer is noticed while the sender is probably still in range.
pub const CHUNK_REQUEST_QUIET_SECS: u64 = 45;

/// Shortest gap between asking a peer for chunks, and where a transfer starts.
///
/// [`CHUNK_REQUEST_QUIET_SECS`] is the *ceiling*, not the cadence. Starting there
/// made the tail of a transfer cost far more than the air did: measured on a real
/// 60-chunk file, one round recovered 49 chunks in about 45 seconds, and the last
/// handful then took several minutes — one 45-second wait each, almost all of it
/// silence. The link was never the limit; the wait was.
///
/// Five seconds is comfortably longer than the gap between chunks of a stream that
/// is actually flowing, so a transfer in progress still never interrupts itself to
/// ask for what is already on its way.
pub const CHUNK_REQUEST_MIN_QUIET_SECS: u64 = 5;

/// Read size for hashing and copying. Matches `encrypt_file_to_file`'s window, so
/// the two passes over a completed transfer cost the same.
const IO_CHUNK_BYTES: usize = 256 * 1024;

/// Bumped when the state layout changes. `restore` removes anything it cannot
/// parse, so an older file is discarded rather than misread — which is the whole
/// reason the magic carries a version.
const STATE_MAGIC: &[u8; 6] = b"CMFT02";

/// Where partial transfers live: *beside* the attachments directory, never
/// inside it.
///
/// Everything in `attachments/` is named by an `AttachmentId` and read by the
/// attachment layer, which has no concept of "incomplete". A half-written
/// plaintext in there is a file the app would happily hand to a decoder, and a
/// plaintext at that — the assembled file is not encrypted until it leaves here.
#[must_use]
pub fn transfers_dir() -> PathBuf {
    attachments_dir().with_file_name("mesh-transfers")
}

/// Create the spool directory, owner-only, like the attachments directory.
pub fn ensure_transfers_dir() -> std::io::Result<()> {
    let dir = transfers_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Destroy every partial transfer.
///
/// Called by the panic wipe. A partial file is still proof that something was
/// received, in the clear, which is exactly what the wipe exists to destroy —
/// and unlike an attachment there is no row whose deletion would crypto-shred
/// it.
pub fn clear_transfers_dir() {
    let dir = transfers_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Err(error) = std::fs::remove_file(entry.path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                target: "mesh::file",
                %error,
                path = %entry.path().display(),
                "a partial mesh transfer could not be removed by the wipe"
            );
        }
    }
}

/// SHA-256 of a file, read in bounded windows.
///
/// The one operation that has to touch every byte of a completed transfer twice
/// (once here, once in the encryptor) and still must not allocate proportionally
/// to its size.
pub fn digest_file(path: &Path) -> Result<[u8; 32], MeshError> {
    let io = |source: std::io::Error| MeshError::FileIo {
        path: path.display().to_string(),
        source,
    };
    let mut file = File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; IO_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buf).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().into())
}

// ---------------------------------------------------------------------------
// Chunk bitmap
// ---------------------------------------------------------------------------

mod bitmap;
mod inbound;
mod outbound;

#[cfg(test)]
mod tests;

pub use bitmap::ChunkBitmap;
pub use inbound::{ChunkOutcome, FileStreamSpool, InboundTransfer, ManifestOutcome};
pub use outbound::OutboundTransfer;
