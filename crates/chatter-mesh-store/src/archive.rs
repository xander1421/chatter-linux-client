//! The public-message cache that survives a restart.
//!
//! Ported from `Sync/GossipMessageArchive.swift`.
//!
//! This is what lets a device act as a town crier. The gossip-sync store keeps the
//! recent public history it has heard; persisting that store means the history
//! survives a relaunch, so a phone can be killed, restarted hours later, walk into a
//! second mesh partition and still hand over the room's backlog.
//!
//! # Why this one blob is not sealed
//!
//! [`crate::storage::Storage`] takes sealed bytes and never sees plaintext. The
//! archive is the case the doc comment there calls out explicitly: its contents are
//! *signed public broadcasts* — every byte was already transmitted in the clear to
//! anyone in radio range, and every byte will be re-broadcast to any peer that asks
//! for it. Encrypting it would protect nothing that is not already public while
//! adding a key the sync layer would have to hold. At-rest protection is the storage
//! implementation's file protection, and the whole blob is dropped on a panic wipe
//! along with everything else.
//!
//! # Format
//!
//! Swift writes a JSON array of base64 packet blobs. We are not wire-compatible with
//! that file and do not need to be — it never leaves the device — so this is a plain
//! length-prefixed framing instead:
//!
//! ```text
//! version:u8 = 1 ‖ count:u32be ‖ (len:u32be ‖ packet_bytes) * count
//! ```
//!
//! Records are raw `BitchatPacket` encodings with padding off, matching Swift's
//! `toBinaryData(padding: false)`: padding is a link-layer concern and re-padding on
//! the way out is free, while storing it inflates the file by up to a bucket per
//! message.
//!
//! Decoding is strict about its own framing — a length that runs past the end, a
//! trailing tail, an unknown version — because a blob that does not decode cleanly
//! is not a blob we should half-restore. Callers treat a decode failure as "start
//! empty" rather than as fatal: losing carried history is bad, refusing to start is
//! worse.

use std::sync::Arc;

use chatter_mesh::codec;
use chatter_mesh::packet::BitchatPacket;

use crate::error::StoreError;
use crate::storage::{Storage, StorageKey};

/// Framing version. Bumping it invalidates old blobs, which is the intended
/// migration: the archive is a cache of already-public traffic that ages out in
/// hours anyway.
pub const ARCHIVE_FORMAT_VERSION: u8 = 1;

/// Largest single record accepted, matching the mesh's reassembly ceiling — no
/// single packet larger than this can exist on this transport, so anything bigger
/// in the blob means the blob is corrupt.
pub const MAX_ARCHIVED_PACKET_BYTES: usize = chatter_mesh::fragment::MAX_REASSEMBLED_BYTES;

/// Largest record count accepted. Comfortably above the sync store's own capacity
/// (1000 by default); it exists so a corrupt length field cannot make us reserve an
/// enormous vector before we notice.
pub const MAX_ARCHIVED_PACKETS: usize = 10_000;

/// Name used in [`StoreError::Malformed`].
const ARCHIVE_LABEL: &str = "public archive";

fn malformed() -> StoreError {
    StoreError::Malformed(ARCHIVE_LABEL)
}

/// Durable cache of the public broadcast packets this device carries.
///
/// Holds the storage seam rather than a path: the real implementation lands over
/// chatter-core's database, and tests use [`crate::storage::MemoryStorage`].
#[derive(Clone)]
pub struct GossipMessageArchive {
    storage: Arc<dyn Storage>,
}

impl core::fmt::Debug for GossipMessageArchive {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `Storage` is a trait object with no Debug bound, and it holds the user's
        // durable state — printing it would be neither possible nor wise.
        f.debug_struct("GossipMessageArchive")
            .finish_non_exhaustive()
    }
}

impl GossipMessageArchive {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    /// Raw packet blobs as stored, or an error if the blob will not decode.
    pub fn try_load(&self) -> Result<Vec<Vec<u8>>, StoreError> {
        match self.storage.load(StorageKey::PublicArchive)? {
            Some(blob) => decode_archive(&blob),
            None => Ok(Vec::new()),
        }
    }

    /// Raw packet blobs, with any failure reported as "nothing archived".
    ///
    /// Mirrors Swift's `load()`, which swallows a missing file, an unreadable file
    /// and a malformed file alike. Use [`try_load`](Self::try_load) when the caller
    /// wants to tell those apart.
    pub fn load(&self) -> Vec<Vec<u8>> {
        self.try_load().unwrap_or_default()
    }

    /// Decoded packets, skipping any record that no longer parses.
    ///
    /// A record that fails to decode is dropped rather than failing the load: it is
    /// one lost public message, and the rest of the room's history is still good.
    pub fn load_packets(&self) -> Vec<BitchatPacket> {
        self.load()
            .iter()
            .filter_map(|bytes| codec::decode(bytes).ok())
            .collect()
    }

    /// Replaces the archive with `packets`.
    ///
    /// An empty list wipes the blob instead of writing an empty one, matching
    /// Swift's `removeItem` — there is no reason to leave a file behind saying we
    /// carry nothing.
    pub fn save(&self, packets: &[Vec<u8>]) -> Result<(), StoreError> {
        if packets.is_empty() {
            return self.storage.wipe(StorageKey::PublicArchive);
        }
        self.storage
            .save(StorageKey::PublicArchive, &encode_archive(packets))
    }

    /// Encodes and stores `packets`, skipping any that will not encode.
    ///
    /// Swift's `compactMap` over `toBinaryData(padding:)` does the same. A packet
    /// that cannot be re-encoded cannot be re-sent either, so keeping it would only
    /// mean carrying bytes we can never serve.
    pub fn save_packets(&self, packets: &[BitchatPacket]) -> Result<(), StoreError> {
        let blobs: Vec<Vec<u8>> = packets
            .iter()
            .filter_map(|packet| codec::encode(packet, false).ok())
            .collect();
        self.save(&blobs)
    }

    /// Forgets everything. Called on panic wipe.
    pub fn wipe(&self) -> Result<(), StoreError> {
        self.storage.wipe(StorageKey::PublicArchive)
    }
}

/// Frames packet blobs into the archive format.
///
/// A record over [`MAX_ARCHIVED_PACKET_BYTES`], or one past
/// [`MAX_ARCHIVED_PACKETS`], is dropped here rather than written and then rejected
/// on the way back in — the encoder and the decoder must agree on what is
/// representable. Callers building the list from live packets cannot produce
/// either: nothing that large exists on this transport, and the sync store's own
/// capacity is an order of magnitude below the count cap.
pub fn encode_archive(packets: &[Vec<u8>]) -> Vec<u8> {
    let kept: Vec<&Vec<u8>> = packets
        .iter()
        .filter(|packet| packet.len() <= MAX_ARCHIVED_PACKET_BYTES)
        .take(MAX_ARCHIVED_PACKETS)
        .collect();

    let body: usize = kept.iter().map(|packet| 4 + packet.len()).sum();
    let mut out = Vec::with_capacity(5 + body);
    out.push(ARCHIVE_FORMAT_VERSION);
    // Both casts below are exact: the count is capped at 10_000 and each record at
    // 1 MiB, so neither can reach `u32::MAX`.
    out.extend_from_slice(&(kept.len() as u32).to_be_bytes());
    for packet in kept {
        out.extend_from_slice(&(packet.len() as u32).to_be_bytes());
        out.extend_from_slice(packet);
    }
    out
}

/// Parses the archive format.
///
/// An empty blob is an empty archive, not an error: that is what a wiped key looks
/// like if an implementation writes zero bytes rather than deleting the row.
pub fn decode_archive(blob: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
    if blob.is_empty() {
        return Ok(Vec::new());
    }

    let (version, rest) = blob.split_first().ok_or_else(malformed)?;
    if *version != ARCHIVE_FORMAT_VERSION {
        return Err(malformed());
    }

    let count_bytes = rest.get(..4).ok_or_else(malformed)?;
    let count =
        u32::from_be_bytes(<[u8; 4]>::try_from(count_bytes).map_err(|_| malformed())?) as usize;
    if count > MAX_ARCHIVED_PACKETS {
        return Err(malformed());
    }

    let mut packets = Vec::with_capacity(count);
    let mut offset = 4usize;
    for _ in 0..count {
        let end = offset.checked_add(4).ok_or_else(malformed)?;
        let len_bytes = rest.get(offset..end).ok_or_else(malformed)?;
        let len =
            u32::from_be_bytes(<[u8; 4]>::try_from(len_bytes).map_err(|_| malformed())?) as usize;
        if len > MAX_ARCHIVED_PACKET_BYTES {
            return Err(malformed());
        }
        let record_end = end.checked_add(len).ok_or_else(malformed)?;
        let record = rest.get(end..record_end).ok_or_else(malformed)?;
        packets.push(record.to_vec());
        offset = record_end;
    }

    // Trailing bytes mean the count and the body disagree. Half a file is not a
    // file: refuse it rather than restore a prefix of unknown provenance.
    if offset != rest.len() {
        return Err(malformed());
    }

    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use chatter_mesh::packet::{MessageType, SENDER_ID_LEN};

    fn archive() -> (GossipMessageArchive, Arc<MemoryStorage>) {
        let storage = Arc::new(MemoryStorage::new());
        (GossipMessageArchive::new(storage.clone()), storage)
    }

    fn message(payload: &[u8], timestamp_ms: u64) -> BitchatPacket {
        BitchatPacket::new(
            MessageType::Message,
            [0x11; SENDER_ID_LEN],
            None,
            timestamp_ms,
            payload.to_vec(),
            7,
        )
    }

    #[test]
    fn round_trips_raw_blobs() {
        let packets = vec![vec![1, 2, 3], vec![], vec![0xFF; 500]];
        let encoded = encode_archive(&packets);
        assert_eq!(decode_archive(&encoded).expect("decodes"), packets);
    }

    #[test]
    fn header_layout_is_exact() {
        let encoded = encode_archive(&[vec![0xAA, 0xBB]]);
        assert_eq!(
            encoded,
            vec![
                ARCHIVE_FORMAT_VERSION,
                0,
                0,
                0,
                1, // count
                0,
                0,
                0,
                2, // record length
                0xAA,
                0xBB,
            ]
        );
    }

    #[test]
    fn an_empty_blob_is_an_empty_archive() {
        assert!(decode_archive(&[]).expect("empty decodes").is_empty());
    }

    #[test]
    fn rejects_a_foreign_version() {
        let mut encoded = encode_archive(&[vec![1]]);
        encoded[0] = 0x02;
        assert_eq!(decode_archive(&encoded), Err(malformed()));
    }

    #[test]
    fn rejects_truncation_at_every_boundary() {
        let encoded = encode_archive(&[vec![1, 2, 3], vec![4, 5]]);
        for cut in 1..encoded.len() {
            assert!(
                decode_archive(&encoded[..cut]).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
        assert!(decode_archive(&encoded).is_ok());
    }

    #[test]
    fn rejects_a_trailing_tail() {
        let mut encoded = encode_archive(&[vec![1]]);
        encoded.push(0x00);
        assert_eq!(decode_archive(&encoded), Err(malformed()));
    }

    #[test]
    fn rejects_an_absurd_count_without_allocating_for_it() {
        let mut blob = vec![ARCHIVE_FORMAT_VERSION];
        blob.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_archive(&blob), Err(malformed()));
    }

    #[test]
    fn rejects_an_absurd_record_length() {
        let mut blob = vec![ARCHIVE_FORMAT_VERSION];
        blob.extend_from_slice(&1u32.to_be_bytes());
        blob.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_archive(&blob), Err(malformed()));
    }

    #[test]
    fn saving_nothing_wipes_the_blob() {
        let (archive, storage) = archive();
        archive.save(&[vec![1, 2, 3]]).expect("save");
        assert_eq!(storage.len(), 1);

        archive.save(&[]).expect("save empty");
        assert!(storage.is_empty(), "an empty archive leaves no blob behind");
        assert!(archive.load().is_empty());
    }

    #[test]
    fn packets_survive_a_save_load_cycle() {
        let (archive, _storage) = archive();
        let packets = vec![
            message(b"first", 1_700_000_000_000),
            message(b"second", 1_700_000_000_001),
        ];
        archive.save_packets(&packets).expect("save");

        let restored = archive.load_packets();
        assert_eq!(restored, packets);
    }

    #[test]
    fn a_corrupt_blob_reads_as_empty_but_reports_through_try_load() {
        let (archive, storage) = archive();
        storage
            .save(StorageKey::PublicArchive, b"not an archive")
            .expect("save");

        assert!(
            archive.load().is_empty(),
            "a corrupt archive must not fail startup"
        );
        assert_eq!(archive.try_load(), Err(malformed()));
    }

    #[test]
    fn an_undecodable_record_is_skipped_not_fatal() {
        let (archive, _storage) = archive();
        let good = codec::encode(&message(b"good", 1_700_000_000_000), false).expect("encode");
        archive
            .save(&[vec![0xFF, 0xFF, 0xFF], good.clone()])
            .expect("save");

        let restored = archive.load_packets();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].payload, b"good".to_vec());
        // The raw blob still holds both records: only the decode drops one.
        assert_eq!(archive.load().len(), 2);
    }

    #[test]
    fn wipe_leaves_nothing() {
        let (archive, storage) = archive();
        archive.save(&[vec![7]]).expect("save");
        archive.wipe().expect("wipe");
        assert!(storage.is_empty());
    }

    #[test]
    fn stored_packets_carry_no_padding() {
        // Padding is a link-layer concern; storing it would inflate the file by a
        // bucket per message and change nothing about what we can serve.
        let (archive, _storage) = archive();
        let packet = message(b"hi", 1_700_000_000_000);
        archive
            .save_packets(core::slice::from_ref(&packet))
            .expect("save");

        let raw = archive.load();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0], codec::encode(&packet, false).expect("encode"));
        assert!(raw[0].len() < codec::encode(&packet, true).expect("encode").len());
    }
}
