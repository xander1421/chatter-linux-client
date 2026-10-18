use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use chatter_mesh::tlv::file_stream::FileManifest;

use crate::services::mesh::error::MeshError;

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
    /// Returns how many of `indices` this transfer can actually serve.
    ///
    /// An index the manifest does not cover is dropped, and the count lets the
    /// caller say so: "asked for 8, queued 8" and "asked for 8, queued 0" are the
    /// difference between a slow link and a request nobody can answer, and they
    /// used to log identically.
    pub fn note_requested(&mut self, indices: &[u32]) -> usize {
        let mut queued = 0;
        for index in indices {
            if self.manifest.chunk_span(*index).is_some() {
                self.requested.insert(*index);
                queued += 1;
            }
        }
        queued
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
        let (offset, len) = self
            .manifest
            .chunk_span(index)
            .ok_or(MeshError::OutOfRange("a chunk index outside the manifest"))?;
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
