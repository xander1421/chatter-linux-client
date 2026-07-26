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

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::file_stream::{FileManifest, TRANSFER_ID_LEN};
use chatter_mesh::tlv::hex_encode;
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

/// Read size for hashing and copying. Matches `encrypt_file_to_file`'s window, so
/// the two passes over a completed transfer cost the same.
const IO_CHUNK_BYTES: usize = 256 * 1024;

const STATE_MAGIC: &[u8; 6] = b"CMFT01";

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

/// Which chunks of a transfer have landed.
///
/// Sans-IO and separately testable: the resume logic is bit arithmetic, and the
/// consequence of getting it wrong is a transfer that either never completes or
/// completes with a hole in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkBitmap {
    bits: Vec<u8>,
    count: u32,
    present: u32,
}

impl ChunkBitmap {
    #[must_use]
    pub fn new(count: u32) -> Self {
        Self {
            bits: vec![0u8; (count as usize).div_ceil(8)],
            count,
            present: 0,
        }
    }

    /// Restores a bitmap from its persisted bytes.
    ///
    /// Rejects a byte length that does not match `count` rather than padding or
    /// truncating: a bitmap of the wrong width for its manifest describes a
    /// different transfer, and guessing which one would resume into a hole.
    pub fn from_bytes(bytes: &[u8], count: u32) -> Option<Self> {
        if bytes.len() != (count as usize).div_ceil(8) {
            return None;
        }
        // Bits past `count` in the final byte must be clear, or `present` and
        // `is_complete` disagree with `missing`.
        let mut map = Self {
            bits: bytes.to_vec(),
            count,
            present: 0,
        };
        map.present = (0..count).filter(|index| map.contains(*index)).count() as u32;
        if map.bits.iter().map(|b| b.count_ones()).sum::<u32>() != map.present {
            return None;
        }
        Some(map)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        if index >= self.count {
            return false;
        }
        let byte = (index / 8) as usize;
        self.bits
            .get(byte)
            .is_some_and(|bits| bits & (1 << (index % 8)) != 0)
    }

    /// Marks a chunk received. `true` when this was news.
    pub fn insert(&mut self, index: u32) -> bool {
        if index >= self.count || self.contains(index) {
            return false;
        }
        let byte = (index / 8) as usize;
        if let Some(bits) = self.bits.get_mut(byte) {
            *bits |= 1 << (index % 8);
            self.present += 1;
            return true;
        }
        false
    }

    #[must_use]
    pub fn present(&self) -> u32 {
        self.present
    }

    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.present == self.count
    }

    /// The first `limit` chunks still missing, ascending.
    ///
    /// Ascending and bounded because that is what a retransmit request can carry:
    /// asking for the earliest gaps first means repeated rounds walk forward
    /// through the file instead of re-asking for an arbitrary subset.
    #[must_use]
    pub fn missing(&self, limit: usize) -> Vec<u32> {
        (0..self.count)
            .filter(|index| !self.contains(*index))
            .take(limit)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Receive
// ---------------------------------------------------------------------------

/// One transfer being assembled.
#[derive(Clone, Debug)]
pub struct InboundTransfer {
    pub manifest: FileManifest,
    /// The peer whose announce-registered signing key authenticated the
    /// manifest. Every chunk must come from the same peer: without that check a
    /// transfer is a write channel into a file anyone in range can contribute to.
    pub sender: PeerId,
    /// The row identity, fixed when the manifest landed.
    ///
    /// Derived from the manifest packet, not from each chunk, and persisted with
    /// the bitmap — so a transfer resumed after a restart still lands on the row
    /// `already_stored` would recognise, rather than a second one.
    pub wire_id: String,
    /// The manifest packet's timestamp, so the finished row is dated when the
    /// sender sent it rather than when the last chunk happened to arrive.
    pub timestamp_ms: u64,
    pub received: ChunkBitmap,
    pub last_progress_secs: u64,
    /// When chunks were last asked for, so a re-request waits out the quiet
    /// period like the first one did.
    ///
    /// Without this, `stalled` was true forever once a transfer went quiet —
    /// its only clock was the last chunk to *arrive*, and a transfer receiving
    /// nothing never advances it. The caller runs every tick, so a stalled
    /// transfer emitted a signed 64-index request twice a second indefinitely:
    /// measured at 2/s for minutes against a link already too busy to deliver
    /// the chunks being asked for.
    pub last_request_secs: u64,
}

/// What a manifest did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestOutcome {
    /// A new transfer; nothing has arrived for it yet.
    Started,
    /// A transfer already on disk, picked up where it stopped.
    Resumed { present: u32, total: u32 },
    /// A repeat of a manifest we are already working on. Manifests are
    /// re-broadcast on the announce cadence, so this is the common case.
    Unchanged { present: u32, total: u32 },
}

/// What a chunk did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// Written; the transfer is still incomplete.
    Stored { present: u32, total: u32 },
    /// Already had it. Normal: a re-request races the original arriving.
    Duplicate,
    /// Every chunk is present. The caller must verify the digest next.
    Complete,
    /// No manifest for this transfer id. Not an error — a peer can hear chunks
    /// before the manifest that explains them, and the manifest repeats.
    Unknown,
    /// A chunk for a live transfer from someone other than its manifest's
    /// sender, or an index the manifest does not have.
    Refused(&'static str),
}

/// The receive spool: partial transfers on disk, keyed by transfer id.
pub struct FileStreamSpool {
    dir: PathBuf,
    transfers: BTreeMap<TransferId, InboundTransfer>,
}

impl FileStreamSpool {
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            transfers: BTreeMap::new(),
        }
    }

    /// A spool over the real directory, with whatever survived a restart already
    /// loaded.
    #[must_use]
    pub fn restored() -> Self {
        let mut spool = Self::new(transfers_dir());
        let resumed = spool.restore(super::state::now_secs());
        if resumed > 0 {
            tracing::info!(
                target: "mesh::file",
                transfers = resumed,
                "resumed partial mesh file transfers from disk"
            );
        }
        spool
    }

    /// Load every intact partial transfer in the directory.
    ///
    /// Anything unparseable is *removed*, not skipped. A state file we cannot
    /// read describes a partial file we cannot finish, and leaving the pair
    /// behind would accumulate them silently.
    /// `now_secs` stamps what is loaded, so a resumed transfer looks like one
    /// that has just made progress rather than one that never has.
    ///
    /// Without it the resumed transfer carried `last_progress_secs: 0`, which
    /// `abandon_stale` reads as "no progress since the epoch" — so every resumed
    /// transfer was discarded on the first tick after startup, and P9's
    /// resumability worked everywhere except across the restart it exists for.
    pub fn restore(&mut self, now_secs: u64) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut loaded = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("state") {
                continue;
            }
            match self.load_state(&path, now_secs) {
                Some(transfer) => {
                    loaded += 1;
                    self.transfers.insert(transfer.manifest.transfer_id, transfer);
                }
                None => {
                    tracing::warn!(
                        target: "mesh::file",
                        path = %path.display(),
                        "a partial mesh transfer could not be resumed; discarding it"
                    );
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_file(path.with_extension("part"));
                }
            }
        }
        loaded
    }

    /// Register a manifest, resuming from disk when there is something to resume.
    pub fn begin(
        &mut self,
        manifest: FileManifest,
        sender: PeerId,
        wire_id: String,
        timestamp_ms: u64,
        now_secs: u64,
    ) -> Result<ManifestOutcome, MeshError> {
        let id = manifest.transfer_id;
        if let Some(existing) = self.transfers.get_mut(&id) {
            // A repeat of a manifest already in progress. The manifest is
            // re-broadcast on the announce cadence so a peer that joined
            // mid-transfer can pick it up, which makes this the *usual* path
            // rather than a duplicate to be logged about.
            //
            // A manifest with the same id but different chunking is refused
            // rather than adopted: the bitmap already on disk means something
            // under the old chunking, and reinterpreting it would resume into a
            // hole.
            if existing.manifest != manifest {
                return Err(MeshError::TransferManifestChanged {
                    transfer: hex_encode(&id),
                });
            }
            existing.last_progress_secs = now_secs;
            return Ok(ManifestOutcome::Unchanged {
                present: existing.received.present(),
                total: existing.received.count(),
            });
        }

        ensure_transfers_dir().map_err(|source| MeshError::FileIo {
            path: self.dir.display().to_string(),
            source,
        })?;

        // Something on disk for this id from a previous run that `restore` could
        // not load, or a transfer whose state file survived a wipe of the map.
        // Either way the safe move is a clean start.
        let part = self.part_path(&id);
        let state = self.state_path(&id);
        if state.exists() {
            let _ = std::fs::remove_file(&state);
        }

        let count = manifest.chunk_count;
        let total_size = manifest.total_size;
        let transfer = InboundTransfer {
            manifest,
            sender,
            wire_id,
            timestamp_ms,
            received: ChunkBitmap::new(count),
            last_progress_secs: now_secs,
            // A new transfer has never been asked for, so the first request is
            // gated only by the progress clock.
            last_request_secs: 0,
        };

        // Sized up front, so the file's length is the file's length from the
        // start and does not depend on which chunk happens to arrive last. The
        // interior is a hole until written; nothing reads it before the digest
        // check.
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&part)
            .map_err(|source| MeshError::FileIo {
                path: part.display().to_string(),
                source,
            })?;
        file.set_len(total_size).map_err(|source| MeshError::FileIo {
            path: part.display().to_string(),
            source,
        })?;

        self.write_state(&transfer)?;
        self.transfers.insert(id, transfer);
        Ok(ManifestOutcome::Started)
    }

    /// Write one chunk straight into the partial file.
    ///
    /// Never accumulates: the bytes go from the caller's slice to the file at
    /// `index * chunk_size` and are dropped.
    pub fn accept_chunk(
        &mut self,
        transfer_id: &TransferId,
        index: u32,
        bytes: &[u8],
        sender: &PeerId,
        now_secs: u64,
    ) -> Result<ChunkOutcome, MeshError> {
        let part = self.part_path(transfer_id);
        let Some(transfer) = self.transfers.get_mut(transfer_id) else {
            return Ok(ChunkOutcome::Unknown);
        };

        // The manifest names the sender; a chunk from anyone else is an attempt
        // to write into someone else's file. Per-chunk signature verification
        // happens a layer up, in the runtime, against the announce registry —
        // this is the check that the verified sender is the *right* verified
        // sender.
        if &transfer.sender != sender {
            return Ok(ChunkOutcome::Refused(
                "a chunk from a peer other than the manifest's sender",
            ));
        }

        let Some((offset, expected_len)) = transfer.manifest.chunk_span(index) else {
            return Ok(ChunkOutcome::Refused("a chunk index the manifest does not have"));
        };
        // The last chunk is short and every other one is exactly `chunk_size`. A
        // chunk of any other length would either leave a gap inside the span or
        // overwrite the next one.
        if bytes.len() != expected_len {
            return Ok(ChunkOutcome::Refused(
                "a chunk whose length disagrees with the manifest's chunking",
            ));
        }
        if transfer.received.contains(index) {
            return Ok(ChunkOutcome::Duplicate);
        }

        let io = |source: std::io::Error| MeshError::FileIo {
            path: part.display().to_string(),
            source,
        };
        let mut file = OpenOptions::new().write(true).open(&part).map_err(io)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.flush().map_err(io)?;

        transfer.received.insert(index);
        transfer.last_progress_secs = now_secs;
        let (present, total) = (transfer.received.present(), transfer.received.count());
        let complete = transfer.received.is_complete();

        // The bitmap is persisted *after* the bytes, never before. A crash
        // between the two loses the record of a chunk that did land, which costs
        // one re-request; the other order would record a chunk that did not, and
        // the transfer would "complete" with a hole and fail its digest.
        let transfer = transfer.clone();
        self.write_state(&transfer)?;

        if complete {
            Ok(ChunkOutcome::Complete)
        } else {
            Ok(ChunkOutcome::Stored { present, total })
        }
    }

    /// Verify the assembled file against the manifest's digest.
    ///
    /// The gate between "every chunk arrived" and "this is the file that was
    /// sent". On mismatch the transfer is discarded here, inside the call, so
    /// there is no window in which a caller could decide to retry it.
    pub fn verify(&mut self, transfer_id: &TransferId) -> Result<(), MeshError> {
        let Some(transfer) = self.transfers.get(transfer_id) else {
            return Err(MeshError::UnknownTransfer {
                transfer: hex_encode(transfer_id),
            });
        };
        let expected = transfer.manifest.sha256;
        let actual = digest_file(&self.part_path(transfer_id))?;
        if actual == expected {
            return Ok(());
        }
        // No retry, ever. Every chunk verified against its sender's signing key,
        // so there is no chunk to blame — re-requesting would fetch the same
        // bytes and fail identically, forever, and the loop would be
        // indistinguishable from a working transfer that never finishes.
        self.discard(transfer_id);
        Err(MeshError::TransferDigestMismatch {
            transfer: hex_encode(transfer_id),
        })
    }

    /// Take a completed transfer's record, leaving its files in place.
    ///
    /// Split from [`discard`](Self::discard) because the caller still has to read
    /// the partial file — that is what the streaming encryptor consumes — and
    /// deleting it here would leave the caller holding a path to nothing.
    pub fn take(&mut self, transfer_id: &TransferId) -> Option<InboundTransfer> {
        self.transfers.remove(transfer_id)
    }

    /// Forget a transfer and remove both of its files.
    pub fn discard(&mut self, transfer_id: &TransferId) {
        self.transfers.remove(transfer_id);
        for path in [self.part_path(transfer_id), self.state_path(transfer_id)] {
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    target: "mesh::file",
                    %error,
                    path = %path.display(),
                    "a partial mesh transfer could not be removed"
                );
            }
        }
    }

    /// Transfers that have gone quiet with chunks still missing, and the chunks
    /// each one wants.
    ///
    /// This is the resume trigger. A broadcast has no acknowledgement, so the
    /// sender cannot know what landed; the receiver notices the silence and asks.
    #[must_use]
    /// Takes `&mut self` so it can stamp what it hands out.
    ///
    /// The stamp is the whole point and is deliberately not left to the caller:
    /// a quiet period enforced by remembering to record the request is a quiet
    /// period that stops working the first time someone forgets. Returning a
    /// transfer *is* asking for it, so the two happen together.
    pub fn stalled(
        &mut self,
        now_secs: u64,
        quiet_secs: u64,
        limit: usize,
    ) -> Vec<(TransferId, Vec<u32>)> {
        let mut asking = Vec::new();
        for transfer in self.transfers.values_mut() {
            if transfer.received.is_complete() {
                continue;
            }
            // Quiet on both clocks: nothing has arrived *and* nothing has been
            // asked for recently. The second is what bounds the request rate.
            if now_secs.saturating_sub(transfer.last_progress_secs) < quiet_secs
                || now_secs.saturating_sub(transfer.last_request_secs) < quiet_secs
            {
                continue;
            }
            let missing = transfer.received.missing(limit);
            if missing.is_empty() {
                continue;
            }
            transfer.last_request_secs = now_secs;
            asking.push((transfer.manifest.transfer_id, missing));
        }
        asking
    }

    /// Drop transfers that have made no progress for `max_age_secs`.
    pub fn abandon_stale(&mut self, now_secs: u64, max_age_secs: u64) {
        let stale: Vec<TransferId> = self
            .transfers
            .values()
            .filter(|transfer| now_secs.saturating_sub(transfer.last_progress_secs) > max_age_secs)
            .map(|transfer| transfer.manifest.transfer_id)
            .collect();
        for id in stale {
            tracing::info!(
                target: "mesh::file",
                transfer = %hex_encode(&id),
                "abandoning a mesh file transfer that stopped making progress"
            );
            self.discard(&id);
        }
    }

    #[must_use]
    pub fn get(&self, transfer_id: &TransferId) -> Option<&InboundTransfer> {
        self.transfers.get(transfer_id)
    }

    /// Transfers whose every chunk is present but which have not been stored.
    ///
    /// Two ways one gets here, and neither is the normal path — the normal path
    /// stores the file the moment the last chunk lands:
    ///
    /// - The process ended between the last chunk and the store.
    /// - The store itself failed, out of disk or on the row write. Every byte is
    ///   already here and nothing will retransmit them, so a periodic retry is the
    ///   only recovery left.
    #[must_use]
    pub fn completed(&self) -> Vec<TransferId> {
        self.transfers
            .values()
            .filter(|transfer| transfer.received.is_complete())
            .map(|transfer| transfer.manifest.transfer_id)
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.transfers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
    }

    #[must_use]
    pub fn part_path(&self, transfer_id: &TransferId) -> PathBuf {
        self.dir.join(format!("{}.part", hex_encode(transfer_id)))
    }

    fn state_path(&self, transfer_id: &TransferId) -> PathBuf {
        self.dir.join(format!("{}.state", hex_encode(transfer_id)))
    }

    /// The manifest, the sender, the row identity, and the bitmap — one file,
    /// rewritten whole on every chunk.
    ///
    /// The manifest is stored as its own TLV encoding rather than a second
    /// serialisation of the same fields: one format on the wire and on disk means
    /// the two cannot drift, and the decoder already validates every invariant a
    /// restored transfer needs.
    ///
    /// Rewritten whole, and not atomically. A torn write leaves a state file that
    /// does not parse, which `restore` treats as an unresumable transfer and
    /// removes — costing the transfer, which is the right way for this to fail.
    /// The alternative, a partially-updated bitmap, would claim a chunk that
    /// never landed.
    fn write_state(&self, transfer: &InboundTransfer) -> Result<(), MeshError> {
        let path = self.state_path(&transfer.manifest.transfer_id);
        let io = |source: std::io::Error| MeshError::FileIo {
            path: path.display().to_string(),
            source,
        };

        let manifest = transfer.manifest.encode()?;
        let sender = transfer.sender.as_str().as_bytes();
        let wire = transfer.wire_id.as_bytes();
        if sender.len() > u16::MAX as usize || wire.len() > u16::MAX as usize {
            return Err(MeshError::OutOfRange(
                "a transfer's sender or wire id is too long to persist",
            ));
        }

        let mut out = Vec::with_capacity(64 + manifest.len() + transfer.received.as_bytes().len());
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&(manifest.len() as u16).to_be_bytes());
        out.extend_from_slice(&manifest);
        out.extend_from_slice(&(sender.len() as u16).to_be_bytes());
        out.extend_from_slice(sender);
        out.extend_from_slice(&(wire.len() as u16).to_be_bytes());
        out.extend_from_slice(wire);
        out.extend_from_slice(&transfer.timestamp_ms.to_be_bytes());
        out.extend_from_slice(transfer.received.as_bytes());

        let mut file = File::create(&path).map_err(io)?;
        file.write_all(&out).map_err(io)?;
        file.flush().map_err(io)?;
        Ok(())
    }

    /// The inverse of [`write_state`](Self::write_state), returning `None` for
    /// anything it cannot fully account for.
    fn load_state(&self, path: &Path, now_secs: u64) -> Option<InboundTransfer> {
        let bytes = std::fs::read(path).ok()?;
        // The same bounds-checked cursor the wire decoders use, for the same
        // reason: this file is attacker-influenced too — its contents came off
        // the radio — and a hand-rolled offset walk over it would be the one
        // place in the transfer path that could index out of range.
        let mut reader = chatter_mesh::tlv::Reader::new(&bytes);

        if reader.take(STATE_MAGIC.len()).ok()? != STATE_MAGIC {
            return None;
        }
        let manifest_len = reader.u16_be().ok()? as usize;
        let manifest = FileManifest::decode(reader.take(manifest_len).ok()?).ok()?;
        let sender_len = reader.u16_be().ok()? as usize;
        let sender = PeerId::from_str(core::str::from_utf8(reader.take(sender_len).ok()?).ok()?);
        let wire_len = reader.u16_be().ok()? as usize;
        let wire_id = core::str::from_utf8(reader.take(wire_len).ok()?)
            .ok()?
            .to_string();
        let timestamp_ms = reader.u64_be().ok()?;
        let received = ChunkBitmap::from_bytes(reader.take_rest(), manifest.chunk_count)?;

        // The partial file has to be there, at the length the manifest declares.
        // A bitmap describing a file that is gone, or a differently sized one,
        // cannot be resumed — it can only be restarted.
        let part = self.part_path(&manifest.transfer_id);
        if std::fs::metadata(&part).ok()?.len() != manifest.total_size {
            return None;
        }

        Some(InboundTransfer {
            manifest,
            sender,
            wire_id,
            timestamp_ms,
            received,
            // Deliberately not the persisted time. A transfer resumed at startup
            // gets a fresh quiet window, so the first thing a restart does is not
            // to fire a retransmit request at a peer it has not even discovered
            // Stamped now, not zero. A resumed transfer gets a full quiet
            // window before it asks for anything — so a restart does not open
            // with a retransmit request at a peer it has not discovered yet —
            // and, just as importantly, is not immediately stale to
            // `abandon_stale`, which is what used to delete it outright.
            last_progress_secs: now_secs,
            last_request_secs: now_secs,
        })
    }
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// One transfer this device is sending.
///
/// Holds a *path*, never the bytes. `next_chunk` walks the file in chunk-sized
/// windows and `read_chunk` reads exactly one window, so a 16 MiB send costs 32
/// KiB of memory at a time — the same shape the server upload path has.
#[derive(Clone, Debug)]
pub struct OutboundTransfer {
    pub manifest: FileManifest,
    pub path: PathBuf,
    /// The row this transfer belongs to, so the sender's own copy and the
    /// delivery state stay attached to it.
    pub message_id: String,
    /// Next chunk of the first pass. Equals `chunk_count` once the pass is done.
    cursor: u32,
    /// Chunks a receiver asked for again, served before the first pass continues.
    requested: std::collections::BTreeSet<u32>,
    /// When the manifest was last put on the air.
    pub last_manifest_secs: u64,
    pub started_secs: u64,
}

impl OutboundTransfer {
    #[must_use]
    pub fn new(manifest: FileManifest, path: PathBuf, message_id: String, now_secs: u64) -> Self {
        Self {
            manifest,
            path,
            message_id,
            cursor: 0,
            requested: std::collections::BTreeSet::new(),
            last_manifest_secs: now_secs,
            started_secs: now_secs,
            // Zero, so the first chunk goes out on the next tick rather than
            // waiting out a pacing interval nothing has used yet.
        }
    }

    /// Record chunks a receiver asked for.
    ///
    /// Indices the manifest does not have are dropped rather than queued: they
    /// cannot be read, and keeping them would make `is_finished` never true.
    /// Record a receiver's request, and learn from it.
    ///
    /// An index below `cursor` was already put on the air once, so asking for it
    /// again says the link did not carry it. That is the only congestion signal
    /// this medium offers — broadcast is unacknowledged, so nothing else reports
    /// whether a chunk arrived — and it is enough to widen the pacing interval.
    ///
    /// An index at or above `cursor` is not evidence of anything: the receiver is
    /// asking for a chunk the first pass has not reached yet, which is ordinary
    /// impatience rather than loss.
    pub fn note_requested(&mut self, indices: &[u32]) {
        for index in indices {
            if self.manifest.chunk_span(*index).is_some() {
                self.requested.insert(*index);
            }
        }
    }


    /// The next chunk to put on the air, retransmissions first.
    ///
    /// Requests come first because a receiver that asked is *blocked* on that
    /// chunk — its transfer cannot complete without it — while the first pass is
    /// merely unfinished.
    pub fn next_chunk(&mut self) -> Option<u32> {
        if let Some(index) = self.requested.iter().next().copied() {
            self.requested.remove(&index);
            return Some(index);
        }
        if self.cursor < self.manifest.chunk_count {
            let index = self.cursor;
            self.cursor += 1;
            return Some(index);
        }
        None
    }

    /// Whether the first pass is done and nothing has been re-requested.
    ///
    /// Not "the receiver has it" — nothing on a broadcast can tell us that. It
    /// means there is nothing left to send unless somebody asks.
    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.cursor >= self.manifest.chunk_count && self.requested.is_empty()
    }

    #[must_use]
    pub fn sent_count(&self) -> u32 {
        self.cursor
    }

    /// Read exactly one chunk out of the source file.
    ///
    /// Opens the file per chunk rather than holding a descriptor for the life of
    /// the transfer. A transfer lasts minutes and is paced across ticks, and a
    /// long-lived handle to a file the user picked is a handle to a file the user
    /// may move or replace — the failure then is a read error on one chunk rather
    /// than a stale descriptor serving stale bytes.
    pub fn read_chunk(&self, index: u32) -> Result<Vec<u8>, MeshError> {
        let (offset, len) = self.manifest.chunk_span(index).ok_or(MeshError::OutOfRange(
            "a chunk index outside the manifest",
        ))?;
        let io = |source: std::io::Error| MeshError::FileIo {
            path: self.path.display().to_string(),
            source,
        };
        let mut file = File::open(&self.path).map_err(io)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).map_err(io)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh::tlv::file_stream::MAX_REQUESTED_CHUNKS;

    const ID: TransferId = [1, 2, 3, 4, 5, 6, 7, 8];

    fn spool() -> (FileStreamSpool, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();
        Box::leak(Box::new(dir));
        (FileStreamSpool::new(path.clone()), path)
    }

    fn manifest_for(bytes: &[u8], chunk_size: u32) -> FileManifest {
        FileManifest::new(
            ID,
            Some("payload.bin".into()),
            Some("application/octet-stream".into()),
            bytes.len() as u64,
            chunk_size,
            Sha256::digest(bytes).into(),
        )
        .expect("valid manifest")
    }

    fn peer(id: &str) -> PeerId {
        PeerId::from_str(id)
    }

    fn body(len: usize) -> Vec<u8> {
        // Position-dependent, so a chunk written at the wrong offset changes the
        // digest. A run of one byte would not.
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn feed(
        spool: &mut FileStreamSpool,
        manifest: &FileManifest,
        bytes: &[u8],
        indices: impl IntoIterator<Item = u32>,
        sender: &PeerId,
    ) -> Vec<ChunkOutcome> {
        indices
            .into_iter()
            .map(|index| {
                let (offset, len) = manifest.chunk_span(index).expect("in range");
                let slice = &bytes[offset as usize..offset as usize + len];
                spool
                    .accept_chunk(&ID, index, slice, sender, 100)
                    .expect("accepted")
            })
            .collect()
    }

    // -- bitmap ------------------------------------------------------------

    #[test]
    fn a_resumed_transfer_survives_the_first_tick_after_a_restart() {
        // Resume used to stamp `last_progress_secs: 0`, which `abandon_stale`
        // reads as "no progress since the epoch". The transfer was therefore
        // discarded — file, bitmap and all — on the first tick after startup, so
        // resumability worked in every test that never restarted and failed at
        // the one thing it exists for.
        let (mut spool, dir) = spool();
        let bytes = body(9_000);
        let manifest = manifest_for(&bytes, 1_024);
        let sender = peer("cccccccccccccccc");
        spool
            .begin(manifest.clone(), sender.clone(), "WIRE-R".into(), 5, 10)
            .unwrap();
        feed(&mut spool, &manifest, &bytes, [0, 1, 2], &sender);
        drop(spool);

        let now = 1_000_000u64;
        let mut restarted = FileStreamSpool::new(dir);
        assert_eq!(restarted.restore(now), 1, "it comes back off disk");

        // The tick the app runs immediately after startup.
        restarted.abandon_stale(now, TRANSFER_ABANDON_SECS);
        let transfer = restarted
            .get(&ID)
            .expect("a transfer resumed a moment ago has not gone an hour without progress");
        assert_eq!(transfer.received.present(), 3, "and its chunks are still there");

        // It is still abandoned once it really has been quiet for an hour.
        restarted.abandon_stale(now + TRANSFER_ABANDON_SECS + 1, TRANSFER_ABANDON_SECS);
        assert!(restarted.get(&ID).is_none(), "stale still means stale");
    }

    #[test]
    fn a_stalled_transfer_is_not_re_requested_within_the_quiet_period() {
        // `stalled` used to consult one clock: the last chunk to *arrive*. A
        // transfer receiving nothing never advances that, so it was stalled
        // forever and the caller — which runs every tick — emitted a signed
        // 64-index request twice a second for as long as the transfer existed.
        // Measured on hardware at 2/s for minutes, against a link already too
        // busy to deliver the chunks being asked for.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut spool = FileStreamSpool::new(dir.path().to_path_buf());
        let bytes = vec![3u8; 4_096];
        let manifest = manifest_for(&bytes, 1_024);
        let sender = PeerId::from_str("aaaaaaaaaaaaaaaa");

        spool.begin(manifest.clone(), sender, "WIRE".into(), 0, 100);

        // Nothing has arrived and nothing has been asked for: the first request
        // goes out.
        let first = spool.stalled(100 + CHUNK_REQUEST_QUIET_SECS, CHUNK_REQUEST_QUIET_SECS, 8);
        assert_eq!(first.len(), 1, "a quiet transfer is asked for once");

        // Every moment inside the quiet period after that asks for nothing,
        // however often the caller runs.
        for offset in [0, 1, 2, CHUNK_REQUEST_QUIET_SECS - 1] {
            let again = spool.stalled(
                100 + CHUNK_REQUEST_QUIET_SECS + offset,
                CHUNK_REQUEST_QUIET_SECS,
                8,
            );
            assert!(
                again.is_empty(),
                "a request at +{offset}s repeats one already in flight"
            );
        }

        // And once the period is out, it asks again — the transfer is still
        // incomplete, so giving up would be worse than asking twice.
        let later = spool.stalled(
            100 + CHUNK_REQUEST_QUIET_SECS * 2 + 1,
            CHUNK_REQUEST_QUIET_SECS,
            8,
        );
        assert_eq!(later.len(), 1, "the quiet period bounds the rate, not the retries");
    }

    #[test]
    fn a_bitmap_tracks_presence_and_reports_the_earliest_gaps() {
        let mut map = ChunkBitmap::new(20);
        assert_eq!(map.missing(3), vec![0, 1, 2]);
        assert!(map.insert(0));
        assert!(!map.insert(0), "the second set is not news");
        assert!(map.insert(19));
        assert_eq!(map.present(), 2);
        assert!(map.contains(19));
        assert!(!map.contains(20), "an index past the end is never present");
        assert!(!map.insert(20), "and cannot be set");
        assert_eq!(map.missing(2), vec![1, 2]);
        assert!(!map.is_complete());
        for index in 1..19 {
            map.insert(index);
        }
        assert!(map.is_complete());
        assert!(map.missing(10).is_empty());
    }

    #[test]
    fn a_bitmap_of_the_wrong_width_for_its_manifest_is_refused() {
        // Resuming with a mismatched bitmap would resume into a hole: bits would
        // name chunks of a differently chunked file.
        let mut map = ChunkBitmap::new(9);
        map.insert(3);
        let bytes = map.as_bytes().to_vec();
        assert_eq!(bytes.len(), 2);
        assert_eq!(ChunkBitmap::from_bytes(&bytes, 9), Some(map));
        assert_eq!(ChunkBitmap::from_bytes(&bytes, 8), None);
        assert_eq!(ChunkBitmap::from_bytes(&bytes, 17), None);
    }

    #[test]
    fn a_bitmap_with_bits_set_past_its_count_is_refused() {
        // Otherwise `present` counts a chunk that cannot exist and the transfer
        // reads as complete with a hole in it.
        assert_eq!(ChunkBitmap::from_bytes(&[0xFF], 4), None);
        assert_eq!(ChunkBitmap::from_bytes(&[0x0F], 4).map(|m| m.present()), Some(4));
    }

    // -- receive -----------------------------------------------------------

    #[test]
    fn a_chunked_transfer_assembles_the_original_bytes() {
        let (mut spool, _dir) = spool();
        let bytes = body(70_000);
        let manifest = manifest_for(&bytes, 32 * 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        assert_eq!(manifest.chunk_count, 3);

        assert_eq!(
            spool
                .begin(manifest.clone(), sender.clone(), "WIRE-1".into(), 42, 10)
                .unwrap(),
            ManifestOutcome::Started
        );

        let outcomes = feed(&mut spool, &manifest, &bytes, 0..3, &sender);
        assert_eq!(
            outcomes,
            vec![
                ChunkOutcome::Stored { present: 1, total: 3 },
                ChunkOutcome::Stored { present: 2, total: 3 },
                ChunkOutcome::Complete,
            ]
        );

        spool.verify(&ID).expect("the digest has to match");
        assert_eq!(
            std::fs::read(spool.part_path(&ID)).unwrap(),
            bytes,
            "the assembled file is the file that was sent"
        );
    }

    #[test]
    fn chunks_arriving_out_of_order_still_assemble() {
        // Every chunk is written at its own offset, so order cannot matter — and
        // it must not, because a re-request delivers the gaps last.
        let (mut spool, _dir) = spool();
        let bytes = body(5_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();

        feed(&mut spool, &manifest, &bytes, [4, 0, 3, 1], &sender);
        assert_eq!(spool.get(&ID).unwrap().received.present(), 4);
        assert_eq!(
            feed(&mut spool, &manifest, &bytes, [2], &sender),
            vec![ChunkOutcome::Complete]
        );
        spool.verify(&ID).expect("digest");
        assert_eq!(std::fs::read(spool.part_path(&ID)).unwrap(), bytes);
    }

    #[test]
    fn a_missing_chunk_is_named_by_the_spool_once_the_transfer_goes_quiet() {
        // The resume mechanism's input. Without this the sender would never learn
        // what to retransmit: a broadcast is not acknowledged.
        let (mut spool, _dir) = spool();
        let bytes = body(5_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();
        feed(&mut spool, &manifest, &bytes, [0, 1, 3, 4], &sender);

        assert!(
            spool.stalled(100, CHUNK_REQUEST_QUIET_SECS, 8).is_empty(),
            "a transfer that just made progress is not stalled"
        );
        let stalled = spool.stalled(100 + CHUNK_REQUEST_QUIET_SECS, CHUNK_REQUEST_QUIET_SECS, 8);
        assert_eq!(stalled, vec![(ID, vec![2])]);

        feed(&mut spool, &manifest, &bytes, [2], &sender);
        assert!(
            spool
                .stalled(1_000, CHUNK_REQUEST_QUIET_SECS, 8)
                .is_empty(),
            "a complete transfer has nothing to ask for"
        );
    }

    #[test]
    fn a_digest_mismatch_discards_and_leaves_nothing_to_retry() {
        // The rule the design is explicit about: a mismatch means some chunk
        // that passed its packet signature still produced the wrong file, so
        // there is nothing to re-request and asking again would loop.
        let (mut spool, _dir) = spool();
        let bytes = body(3_000);
        let mut manifest = manifest_for(&bytes, 1024);
        manifest.sha256 = [0xAB; 32];
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();
        assert_eq!(
            feed(&mut spool, &manifest, &bytes, 0..3, &sender).last(),
            Some(&ChunkOutcome::Complete)
        );

        let part = spool.part_path(&ID);
        assert!(matches!(
            spool.verify(&ID),
            Err(MeshError::TransferDigestMismatch { .. })
        ));
        assert!(spool.get(&ID).is_none(), "the transfer is gone");
        assert!(!part.exists(), "and so are its bytes");
        assert!(
            spool.stalled(10_000, 0, 8).is_empty(),
            "nothing is left that could be re-requested"
        );
        // A chunk arriving afterwards finds no transfer rather than restarting one.
        assert_eq!(
            spool
                .accept_chunk(&ID, 0, &bytes[..1024], &sender, 200)
                .unwrap(),
            ChunkOutcome::Unknown
        );
    }

    #[test]
    fn a_restart_resumes_from_the_persisted_bitmap_instead_of_starting_over() {
        // The reason the bitmap is on disk at all. In memory, resume survives a
        // link drop and dies with the process — which is exactly the case it is
        // wanted for.
        let (mut spool, dir) = spool();
        let bytes = body(9_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "WIRE-9".into(), 77, 10)
            .unwrap();
        feed(&mut spool, &manifest, &bytes, [0, 1, 2, 3, 4, 5, 6], &sender);
        drop(spool);

        // A fresh process over the same directory.
        let mut restarted = FileStreamSpool::new(dir);
        assert_eq!(restarted.restore(200), 1);
        let transfer = restarted.get(&ID).expect("resumed");
        assert_eq!(transfer.received.present(), 7);
        assert_eq!(transfer.wire_id, "WIRE-9", "the row identity survives too");
        assert_eq!(transfer.timestamp_ms, 77);
        assert_eq!(transfer.sender, sender);
        assert_eq!(restarted.stalled(10_000, 60, 8), vec![(ID, vec![7, 8])]);

        // And the tail completes rather than the whole file being resent.
        assert_eq!(
            feed(&mut restarted, &manifest, &bytes, [7, 8], &sender).last(),
            Some(&ChunkOutcome::Complete)
        );
        restarted.verify(&ID).expect("digest");
        assert_eq!(std::fs::read(restarted.part_path(&ID)).unwrap(), bytes);
    }

    #[test]
    fn a_repeated_manifest_resumes_rather_than_truncating_what_arrived() {
        // Manifests repeat on the announce cadence so a peer joining mid-transfer
        // can pick one up. If a repeat restarted the transfer, that cadence would
        // guarantee no transfer ever finished.
        let (mut spool, _dir) = spool();
        let bytes = body(4_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();
        feed(&mut spool, &manifest, &bytes, [0, 1], &sender);

        assert_eq!(
            spool
                .begin(manifest.clone(), sender.clone(), "W2".into(), 2, 20)
                .unwrap(),
            ManifestOutcome::Unchanged { present: 2, total: 4 }
        );
        assert_eq!(spool.get(&ID).unwrap().received.present(), 2);
        assert_eq!(
            spool.get(&ID).unwrap().wire_id,
            "W",
            "the row identity is the first manifest's, so a repeat cannot fork the row"
        );
    }

    #[test]
    fn a_manifest_that_changes_its_chunking_mid_transfer_is_refused() {
        let (mut spool, _dir) = spool();
        let bytes = body(4_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();
        let mut reshaped = manifest_for(&bytes, 2048);
        reshaped.sha256 = manifest.sha256;
        assert!(matches!(
            spool.begin(reshaped, sender, "W".into(), 1, 11),
            Err(MeshError::TransferManifestChanged { .. })
        ));
    }

    #[test]
    fn a_chunk_from_a_different_peer_is_refused() {
        // Otherwise a transfer is an unauthenticated write channel into a file:
        // the packet signature proves who sent *that packet*, not that they are
        // the peer whose manifest opened the file.
        let (mut spool, _dir) = spool();
        let bytes = body(3_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender, "W".into(), 1, 10)
            .unwrap();
        assert!(matches!(
            spool
                .accept_chunk(&ID, 0, &bytes[..1024], &peer("cccccccccccccccc"), 20)
                .unwrap(),
            ChunkOutcome::Refused(_)
        ));
        assert_eq!(spool.get(&ID).unwrap().received.present(), 0);
    }

    #[test]
    fn a_chunk_of_the_wrong_length_or_index_is_refused() {
        let (mut spool, _dir) = spool();
        let bytes = body(3_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();

        // Short: would leave a gap inside its own span, which no later chunk
        // fills and which the digest would then catch far too late.
        assert!(matches!(
            spool.accept_chunk(&ID, 0, &bytes[..100], &sender, 20).unwrap(),
            ChunkOutcome::Refused(_)
        ));
        // Long: would overwrite the head of the next chunk.
        assert!(matches!(
            spool.accept_chunk(&ID, 0, &bytes[..2000], &sender, 20).unwrap(),
            ChunkOutcome::Refused(_)
        ));
        // Past the end: nothing to write at.
        assert!(matches!(
            spool.accept_chunk(&ID, 99, &bytes[..1024], &sender, 20).unwrap(),
            ChunkOutcome::Refused(_)
        ));
        // The last chunk *is* short, and that one is fine.
        assert_eq!(
            spool
                .accept_chunk(&ID, 2, &bytes[2048..3000], &sender, 20)
                .unwrap(),
            ChunkOutcome::Stored { present: 1, total: 3 }
        );
    }

    #[test]
    fn a_duplicate_chunk_is_recognised_rather_than_rewritten() {
        // A re-request racing the original arrival is normal, and rewriting would
        // double the disk traffic of every recovered transfer.
        let (mut spool, _dir) = spool();
        let bytes = body(3_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();
        feed(&mut spool, &manifest, &bytes, [0], &sender);
        assert_eq!(
            feed(&mut spool, &manifest, &bytes, [0], &sender),
            vec![ChunkOutcome::Duplicate]
        );
    }

    #[test]
    fn a_chunk_for_an_unknown_transfer_is_ignored_not_started() {
        // A peer can hear chunks before the manifest that explains them. Starting
        // a transfer from a chunk would mean assembling a file with no declared
        // size and no digest to check it against.
        let (mut spool, _dir) = spool();
        assert_eq!(
            spool
                .accept_chunk(&ID, 0, b"orphan", &peer("bbbbbbbbbbbbbbbb"), 10)
                .unwrap(),
            ChunkOutcome::Unknown
        );
        assert!(!spool.part_path(&ID).exists());
    }

    #[test]
    fn a_transfer_that_stops_making_progress_is_abandoned_with_its_bytes() {
        let (mut spool, _dir) = spool();
        let bytes = body(3_000);
        let manifest = manifest_for(&bytes, 1024);
        let sender = peer("bbbbbbbbbbbbbbbb");
        spool
            .begin(manifest.clone(), sender.clone(), "W".into(), 1, 10)
            .unwrap();
        feed(&mut spool, &manifest, &bytes, [0], &sender);
        let part = spool.part_path(&ID);

        spool.abandon_stale(100, TRANSFER_ABANDON_SECS);
        assert!(spool.get(&ID).is_some(), "still fresh");
        spool.abandon_stale(100 + TRANSFER_ABANDON_SECS + 1, TRANSFER_ABANDON_SECS);
        assert!(spool.is_empty());
        assert!(!part.exists(), "an abandoned transfer leaves nothing behind");
    }

    #[test]
    fn a_state_file_with_no_partial_file_is_not_resumed() {
        let (mut spool, dir) = spool();
        let bytes = body(3_000);
        let manifest = manifest_for(&bytes, 1024);
        spool
            .begin(manifest, peer("bbbbbbbbbbbbbbbb"), "W".into(), 1, 10)
            .unwrap();
        std::fs::remove_file(spool.part_path(&ID)).unwrap();

        let mut restarted = FileStreamSpool::new(dir);
        assert_eq!(restarted.restore(200), 0);
        assert!(restarted.is_empty());
    }

    #[test]
    fn a_corrupt_state_file_is_removed_rather_than_accumulated() {
        let (mut spool, dir) = spool();
        let bytes = body(3_000);
        let manifest = manifest_for(&bytes, 1024);
        spool
            .begin(manifest, peer("bbbbbbbbbbbbbbbb"), "W".into(), 1, 10)
            .unwrap();
        let state = dir.join(format!("{}.state", hex_encode(&ID)));
        std::fs::write(&state, b"not a transfer").unwrap();

        let mut restarted = FileStreamSpool::new(dir);
        assert_eq!(restarted.restore(200), 0);
        assert!(!state.exists(), "and it does not linger to be retried forever");
    }

    // -- send --------------------------------------------------------------

    #[test]
    fn a_send_walks_the_file_in_windows_and_never_reads_it_whole() {
        let (_spool, dir) = spool();
        let bytes = body(5_000);
        let path = dir.join("source.bin");
        std::fs::write(&path, &bytes).unwrap();
        let manifest = manifest_for(&bytes, 1024);
        let mut send = OutboundTransfer::new(manifest.clone(), path, "MSG".into(), 0);

        let mut rebuilt = Vec::new();
        let mut emitted = Vec::new();
        while let Some(index) = send.next_chunk() {
            let chunk = send.read_chunk(index).expect("read");
            assert!(chunk.len() <= 1024, "one window at a time");
            emitted.push(index);
            rebuilt.extend_from_slice(&chunk);
        }
        assert_eq!(emitted, vec![0, 1, 2, 3, 4]);
        assert_eq!(rebuilt, bytes, "the windows tile the file exactly");
        assert!(send.is_drained());
        assert!(send.read_chunk(5).is_err(), "no chunk past the end");
    }

    #[test]
    fn a_requested_chunk_is_retransmitted_before_the_first_pass_continues() {
        // The receiver that asked is blocked on that chunk; the first pass is
        // merely unfinished.
        let (_spool, dir) = spool();
        let bytes = body(5_000);
        let path = dir.join("source2.bin");
        std::fs::write(&path, &bytes).unwrap();
        let mut send = OutboundTransfer::new(manifest_for(&bytes, 1024), path, "MSG".into(), 0);

        assert_eq!(send.next_chunk(), Some(0));
        assert_eq!(send.next_chunk(), Some(1));
        send.note_requested(&[0, 99]);
        assert_eq!(send.next_chunk(), Some(0), "the re-request jumps the queue");
        assert_eq!(send.next_chunk(), Some(2), "then the pass resumes");
        assert_eq!(send.next_chunk(), Some(3));
        assert_eq!(send.next_chunk(), Some(4));
        assert_eq!(
            send.next_chunk(),
            None,
            "index 99 is not in the manifest and must not keep the transfer alive"
        );
        assert!(send.is_drained());
    }

    #[test]
    fn a_requested_chunk_reopens_a_drained_transfer() {
        let (_spool, dir) = spool();
        let bytes = body(2_000);
        let path = dir.join("source3.bin");
        std::fs::write(&path, &bytes).unwrap();
        let mut send = OutboundTransfer::new(manifest_for(&bytes, 1024), path, "MSG".into(), 0);
        while send.next_chunk().is_some() {}
        assert!(send.is_drained());

        send.note_requested(&[1]);
        assert!(!send.is_drained(), "there is work again");
        assert_eq!(send.next_chunk(), Some(1));
        assert!(send.is_drained());
    }

    #[test]
    fn the_request_cap_bounds_what_one_round_asks_for() {
        let mut map = ChunkBitmap::new(500);
        assert_eq!(map.missing(MAX_REQUESTED_CHUNKS).len(), MAX_REQUESTED_CHUNKS);
        for index in 0..500 {
            map.insert(index);
        }
        assert!(map.missing(MAX_REQUESTED_CHUNKS).is_empty());
    }
}
