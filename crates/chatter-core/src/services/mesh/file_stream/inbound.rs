use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::file_stream::FileManifest;
use chatter_mesh::tlv::hex_encode;

use super::{
    CHUNK_REQUEST_MIN_QUIET_SECS, ChunkBitmap, STATE_MAGIC, TransferId, digest_file,
    ensure_transfers_dir, transfers_dir,
};
use crate::services::mesh::error::MeshError;

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
    /// How long this transfer currently waits between requests.
    ///
    /// Starts at [`CHUNK_REQUEST_MIN_QUIET_SECS`] and doubles, up to the ceiling,
    /// each time a request produces nothing — and drops straight back to the floor
    /// the moment one produces a chunk. A peer that is answering should be asked
    /// again promptly; a peer that is gone should be asked rarely, and the two are
    /// only distinguishable by whether the last ask worked.
    pub request_backoff_secs: u64,
    /// Chunks present when the last request went out, to judge whether it worked.
    pub present_at_last_request: u32,
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
    /// A manifest for a transfer that already finished. Ignored.
    AlreadyComplete,
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
    completed: std::collections::VecDeque<TransferId>,
}

/// How many finished transfer ids to remember. Enough to cover a sender's
/// manifest repeats for every file in a burst, small enough to be free.
const MAX_REMEMBERED_COMPLETIONS: usize = 64;

impl FileStreamSpool {
    /// Transfer ids finished recently, so a re-broadcast manifest is not
    /// mistaken for a new transfer.
    ///
    /// A sender repeats the manifest on the announce cadence for as long as it
    /// holds the transfer open, and those repeats keep arriving after the file is
    /// complete. By then the transfer is gone from `transfers` — finished and
    /// cleared — so `begin` saw a manifest it did not recognise and started the
    /// whole download again. The `already_stored` guard downstream cannot catch
    /// it either: that keys on the manifest *packet's* dedup id, which is
    /// different for every re-broadcast.
    ///
    /// Observed on hardware: the same 116 KB file received three times in four
    /// minutes — and stored twice, a second encrypted copy under a second row —
    /// while a 2.5 MB transfer waited for the air.
    ///
    /// The transfer id is the stable identity here, so that is what is remembered.
    /// Bounded and in memory only: after a restart a repeat costs one re-download,
    /// which is the old behaviour and not worth a file on disk to avoid.
    fn note_completed(&mut self, transfer_id: TransferId) {
        if self.completed.contains(&transfer_id) {
            return;
        }
        if self.completed.len() >= MAX_REMEMBERED_COMPLETIONS {
            self.completed.pop_front();
        }
        self.completed.push_back(transfer_id);
    }

    /// Whether this transfer finished recently.
    #[must_use]
    pub fn is_complete(&self, transfer_id: &TransferId) -> bool {
        self.completed.contains(transfer_id)
    }

    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            transfers: BTreeMap::new(),
            completed: std::collections::VecDeque::new(),
        }
    }

    /// A spool over the real directory, with whatever survived a restart already
    /// loaded.
    #[must_use]
    pub fn restored() -> Self {
        let mut spool = Self::new(transfers_dir());
        let resumed = spool.restore(super::super::state::now_secs());
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
                    self.transfers
                        .insert(transfer.manifest.transfer_id, transfer);
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
        if self.is_complete(&id) {
            return Ok(ManifestOutcome::AlreadyComplete);
        }
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
            request_backoff_secs: CHUNK_REQUEST_MIN_QUIET_SECS,
            present_at_last_request: 0,
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
        file.set_len(total_size)
            .map_err(|source| MeshError::FileIo {
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
            return Ok(ChunkOutcome::Refused(
                "a chunk index the manifest does not have",
            ));
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
    /// Finish a transfer: remember it, then clear it.
    ///
    /// Distinct from [`discard`](Self::discard), which is for a transfer that
    /// failed or was abandoned. Those must stay eligible to start again — a peer
    /// re-sending a file after a stall is exactly the recovery the mesh wants —
    /// whereas one that completed must not be downloaded a second time.
    pub fn finish(&mut self, transfer_id: &TransferId) {
        self.note_completed(*transfer_id);
        self.discard(transfer_id);
    }

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
    /// `quiet_secs` is the ceiling on the per-transfer backoff, not the interval.
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

            // Judge the previous request before deciding on the next. Anything
            // arriving since then means the peer is answering, so go back to
            // asking promptly; nothing means back off, so a peer that has left
            // is not shouted at every few seconds for an hour.
            if transfer.last_request_secs > 0 {
                if transfer.received.present() > transfer.present_at_last_request {
                    transfer.request_backoff_secs = CHUNK_REQUEST_MIN_QUIET_SECS;
                } else {
                    transfer.request_backoff_secs =
                        transfer.request_backoff_secs.saturating_mul(2).clamp(
                            CHUNK_REQUEST_MIN_QUIET_SECS,
                            quiet_secs.max(CHUNK_REQUEST_MIN_QUIET_SECS),
                        );
                }
            }
            let wait = transfer.request_backoff_secs.clamp(
                CHUNK_REQUEST_MIN_QUIET_SECS,
                quiet_secs.max(CHUNK_REQUEST_MIN_QUIET_SECS),
            );

            // Quiet on both clocks: nothing has arrived *and* nothing has been
            // asked for recently. The first stops a transfer interrupting a stream
            // that is already flowing; the second bounds the request rate.
            if now_secs.saturating_sub(transfer.last_progress_secs) < wait
                || now_secs.saturating_sub(transfer.last_request_secs) < wait
            {
                continue;
            }
            let missing = transfer.received.missing(limit);
            if missing.is_empty() {
                continue;
            }
            transfer.last_request_secs = now_secs;
            transfer.present_at_last_request = transfer.received.present();
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
        // Persisted, so the abandon clock measures real elapsed time rather than
        // time since the last restart. Stamping it fresh on resume let a transfer
        // whose sender is long gone survive indefinitely: every restart moved its
        // deadline an hour further out, and it went on asking for chunks nobody
        // holds, every 45 seconds, for as long as the app kept being reopened.
        out.extend_from_slice(&transfer.last_progress_secs.to_be_bytes());
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
        let last_progress_secs = reader.u64_be().ok()?;
        let received = ChunkBitmap::from_bytes(reader.take_rest(), manifest.chunk_count)?;
        let present_on_disk = received.present();

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
            // The persisted value, so `abandon_stale` sees how long this
            // transfer has *really* been without progress. Zero here used to
            // delete every resumed transfer on the first tick; `now_secs` then
            // went too far the other way and made a dead one immortal across
            // restarts. The recorded time is the only one that is true.
            last_progress_secs,
            // The request clock is still stamped fresh, and for its own reason: a
            // restart should not open by firing a retransmit request at a peer it
            // has not discovered yet.
            last_request_secs: now_secs,
            // A resumed transfer has no idea whether its peer is still there, so
            // it starts at the floor and finds out.
            request_backoff_secs: CHUNK_REQUEST_MIN_QUIET_SECS,
            present_at_last_request: present_on_disk,
        })
    }
}
