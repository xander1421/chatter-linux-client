//! Chunked, resumable file transfer: manifest, chunk, and chunk request.
//!
//! **Not a port.** Every other module in this directory reproduces a Swift or
//! Kotlin type byte for byte; there is no bitchat original for these three. They
//! exist because a large single-packet transfer has no way to request missing
//! fragments. A 1 MiB payload is ~2236 fragments, ~1.14 MiB on the wire at
//! 514 bytes per unrouted fragment, or ~117 s at the optimistic 10 kB/s measured
//! on this hardware. The assembler now expires a stream 30 s after its last
//! progress, so a continuous stream may finish; one dropped fragment still makes
//! the entire packet unusable. Chunking above the fragmenter bounds that loss
//! and lets the receiver ask for only the missing chunks.
//!
//! One module for three packet types, which is a deliberate exception to this
//! directory's one-type-per-module rule. The rule follows from provenance —
//! each module mirrors one upstream type, and the upstream types genuinely
//! disagree about TLV widths, so sharing between them would be a lie. These
//! three are *one protocol* with no upstream: they share the transfer id width,
//! the chunk size, and the caps, and splitting them would mean importing
//! constants across modules in a directory that has deliberately never done
//! that.
//!
//! # Message types
//!
//! [`MessageType::FileManifest`] = `0xE0`, [`MessageType::FileChunk`] = `0xE1`,
//! [`MessageType::FileChunkRequest`] = `0xE2`. These are Chatter extension tags.
//!
//! Upstream has reserved `0x2A`/`0x2B` and assigned `0x2C` to `announceV2`.
//! These extension tags remain provisional until allocated with upstream.
//!
//! # Why chunk TLVs are not added to `BitchatFilePacket`
//!
//! Because bitchat's decoder *skips* unknown TLV types. A chunked file dressed
//! as a `FileTransfer` would decode over there as a file with partial content —
//! a silently corrupt file, not a clean refusal. A new message type is the only
//! shape that fails safely against a peer that does not implement it.
//!
//! # Wire formats
//!
//! `FileManifest` is TLV, `tag ‖ len:u16be ‖ value`, because it is metadata with
//! optional fields:
//!
//! | Type | Field | Notes |
//! |---|---|---|
//! | 0x01 | transferId (8) | required |
//! | 0x02 | totalSize (u64 BE) | required, `1..=MAX_STREAMED_FILE_BYTES` |
//! | 0x03 | chunkSize (u32 BE) | required, `MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES` |
//! | 0x04 | chunkCount (u32 BE) | required, must equal `ceil(total / chunk)` |
//! | 0x05 | sha256 (32) | required, over the *plaintext* file |
//! | 0x06 | fileName (UTF-8) | optional |
//! | 0x07 | mimeType (UTF-8) | optional |
//!
//! `FileChunk` and `FileChunkRequest` are *not* TLV. A 32 KiB chunk has no
//! optional fields and no forward-compatible extension point worth five bytes
//! per packet on a link that moves single-digit kB/s, so both are fixed headers:
//!
//! ```text
//! FileChunk:        [transferId: 8][index: u32 BE][bytes…]
//! FileChunkRequest: [transferId: 8][count: u16 BE][index: u32 BE] * count
//! ```

use crate::error::MeshError;
use crate::fragment::{DEFAULT_FRAGMENT_SIZE, FRAGMENT_LIFETIME_SECS};
use crate::tlv::{Reader, fixed, put_tlv_u16_len, utf8};

const TLV_TRANSFER_ID: u8 = 0x01;
const TLV_TOTAL_SIZE: u8 = 0x02;
const TLV_CHUNK_SIZE: u8 = 0x03;
const TLV_CHUNK_COUNT: u8 = 0x04;
const TLV_SHA256: u8 = 0x05;
const TLV_FILE_NAME: u8 = 0x06;
const TLV_MIME_TYPE: u8 = 0x07;

/// Width of a transfer identifier.
///
/// Eight bytes, matching the fragmenter's stream id, so every "which stream is
/// this" field in the crate is the same width and the same hex length in a log
/// line.
pub const TRANSFER_ID_LEN: usize = 8;

/// Fixed header of a [`FileChunk`]: transfer id and chunk index.
pub const CHUNK_HEADER_LEN: usize = TRANSFER_ID_LEN + 4;

/// Encoded unrouted fragment at the default chunk size: v2 header, sender,
/// recipient, fragment header, and chunk. Broadcasts omit the recipient, but
/// using the directed size keeps the ordinary lifetime estimate conservative.
/// A source route adds bytes per hop; the send planner budgets those per packet.
const WIRE_BLOCK_BYTES: usize = DEFAULT_FRAGMENT_SIZE + 16 + 8 + 8 + 13;

/// The pessimistic end of the goodput measured on this hardware (Galaxy S25
/// Ultra, Galaxy S9+, Linux/BlueZ), in bytes per second.
///
/// The same figure `fragment.rs` calls generous. Every size decision below is
/// derived from it rather than from an MTU, because the MTU is not what limits
/// this — the reassembly clock is.
pub const PESSIMISTIC_GOODPUT_BYTES_PER_SEC: usize = 3_000;

/// The optimistic end of the same measurement. Used for honest estimates, never
/// for sizing.
pub const OPTIMISTIC_GOODPUT_BYTES_PER_SEC: usize = 10_000;

/// Chunk size: 32 KiB.
///
/// Not derived from the MTU — the fragmenter already owns that layer, and
/// deriving from it would couple two decisions that change for different
/// reasons. Derived instead from three things:
///
/// - **It should reassemble within one fragment lifetime.** 32 KiB is ~70
///   fragments, ~36 kB of wire once padded, which is ~12 s at
///   [`PESSIMISTIC_GOODPUT_BYTES_PER_SEC`] against a 30 s window. See
///   [`fits_fragment_lifetime`], and the test that asserts the margin.
/// - **Per-packet overhead has to amortise.** ~70 fragments per chunk means the
///   88-byte signed envelope is noise.
/// - **A stalled transfer should hold almost nothing.** A 1 MiB file is 32
///   chunks, which is fine enough granularity that a re-request costs 32 KiB,
///   not a megabyte.
pub const CHUNK_BYTES: usize = 32 * 1024;

/// Smallest chunk size a manifest may declare.
///
/// A peer with a tiny MTU may legitimately want smaller chunks, but not
/// arbitrarily small: `chunk_count` is `ceil(total / chunk_size)`, so a
/// one-byte chunk size on a large file names sixteen million chunks and the
/// receiver allocates a bitmap for all of them. The floor is what bounds that.
pub const MIN_CHUNK_BYTES: usize = 1024;

/// Largest chunk size a manifest may declare — the same as what we send.
///
/// A chunk larger than this cannot be reassembled inside
/// [`FRAGMENT_LIFETIME_SECS`] with margin, so accepting one would mean
/// accepting a transfer that can never complete. Refusing the manifest is the
/// honest answer; there is no third-party implementation to stay compatible
/// with.
pub const MAX_CHUNK_BYTES: usize = CHUNK_BYTES;

/// Hard ceiling on a streamed transfer: 16 MiB.
///
/// Once the bytes are streamed, memory no longer sets this — *time* does, and
/// the server's 2 GiB attachment cap is meaningless here. At
/// [`OPTIMISTIC_GOODPUT_BYTES_PER_SEC`] 16 MiB is ~28 minutes of continuous
/// radio; at [`PESSIMISTIC_GOODPUT_BYTES_PER_SEC`] it is over 90. That is
/// already at the edge of what anyone would wait for, and a 100 MiB file would
/// be a three-hour commitment rather than a transfer. Sized by patience, as the
/// design says, and [`estimated_seconds`] exists so the number can be shown
/// rather than discovered.
pub const MAX_STREAMED_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Largest payload still worth sending as a single `MessageType::FileTransfer`.
///
/// Deliberately equal to [`CHUNK_BYTES`]: both answer the same question — how
/// much can one fragment stream carry before the reassembly clock beats it — so
/// two different numbers would mean one of them was wrong. Anything above this
/// is chunked, which buys resume; anything at or below it stays on the
/// single-packet path that real bitchat speaks.
pub const MAX_SINGLE_PACKET_FILE_BYTES: usize = CHUNK_BYTES;

/// Chunk indices one [`FileChunkRequest`] may name.
///
/// Bounded so the encoded request never itself needs fragmenting: `8 + 2 + 4*64
/// = 266` payload bytes, plus a 16-byte header, 8-byte sender and 64-byte
/// signature, is 354 — inside one GATT write on any MTU this stack negotiates.
/// A fragmented retransmit request would be subject to the very reassembly
/// window it exists to work around.
///
/// The encoder truncates past this rather than failing, exactly as
/// `encode_fragment_id_filter` does for the fragment id filter: a receiver
/// missing 300 chunks asks for 64 now and the rest next round, which is slower
/// than one big ask and infinitely better than a request no peer accepts.
pub const MAX_REQUESTED_CHUNKS: usize = 64;

/// Bytes actually put on the air to carry `payload_len` bytes of packet payload.
///
/// Counts the signed envelope and charges each fragment its maximum encoded
/// frame size. Used to size everything above against the reassembly clock.
#[must_use]
pub fn wire_bytes_for_payload(payload_len: usize) -> usize {
    // 16-byte v2 header + 8 sender + 64 signature. Broadcast omits the
    // recipient, which is the shape all three of these types are sent in.
    const ENVELOPE_BYTES: usize = 16 + 8 + 64;
    let encoded = payload_len + ENVELOPE_BYTES;
    if encoded <= WIRE_BLOCK_BYTES {
        return WIRE_BLOCK_BYTES;
    }
    encoded.div_ceil(DEFAULT_FRAGMENT_SIZE) * WIRE_BLOCK_BYTES
}

/// Whether a packet payload of this size can be reassembled before the
/// assembler evicts it, at the pessimistic goodput.
///
/// This is the predicate the whole design turns on. It is exposed rather than
/// inlined so the constants above can be *tested* against it instead of
/// asserted by comment.
#[must_use]
pub fn fits_fragment_lifetime(payload_len: usize) -> bool {
    let seconds = wire_bytes_for_payload(payload_len) / PESSIMISTIC_GOODPUT_BYTES_PER_SEC;
    (seconds as u64) < FRAGMENT_LIFETIME_SECS
}

/// How long a whole transfer of `total_size` will take at `goodput`, in seconds.
///
/// For telling the user before they commit, not for sizing anything. Counts the
/// per-chunk wire cost, so it includes the padding and envelope overhead the raw
/// file size hides.
#[must_use]
pub fn estimated_seconds(total_size: u64, chunk_size: u32, goodput_bytes_per_sec: usize) -> u64 {
    if goodput_bytes_per_sec == 0 || chunk_size == 0 {
        return u64::MAX;
    }
    let chunks = total_size.div_ceil(chunk_size as u64);
    let per_chunk = wire_bytes_for_payload(CHUNK_HEADER_LEN + chunk_size as usize) as u64;
    chunks.saturating_mul(per_chunk) / goodput_bytes_per_sec as u64
}

/// How many chunks a file of `total_size` splits into at `chunk_size`.
///
/// `None` when the arithmetic could not be represented — a zero chunk size, or a
/// count past `u32`. Both are refusals rather than clamps: a clamped count
/// describes a different file than the one being sent.
#[must_use]
pub fn chunk_count_for(total_size: u64, chunk_size: u32) -> Option<u32> {
    if chunk_size == 0 {
        return None;
    }
    u32::try_from(total_size.div_ceil(chunk_size as u64)).ok()
}

/// Metadata for one chunked transfer, sent before the chunks and repeated while
/// the transfer is live so a peer that joins mid-transfer can pick it up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileManifest {
    pub transfer_id: [u8; TRANSFER_ID_LEN],
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub total_size: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
    /// SHA-256 of the plaintext file.
    ///
    /// Verified on completion, **before** anything reads the assembled file. A
    /// manifest signature authenticates the manifest; it says nothing about the
    /// bytes that arrive afterwards, and this digest is what closes that gap
    /// over the file as a whole.
    pub sha256: [u8; 32],
}

impl FileManifest {
    /// Builds a manifest, deriving and validating the chunk count.
    ///
    /// Fallible on purpose: an invalid manifest is not representable, so no
    /// receiver has to defend against one that came from our own encoder.
    pub fn new(
        transfer_id: [u8; TRANSFER_ID_LEN],
        file_name: Option<String>,
        mime_type: Option<String>,
        total_size: u64,
        chunk_size: u32,
        sha256: [u8; 32],
    ) -> Result<Self, MeshError> {
        let chunk_count = chunk_count_for(total_size, chunk_size).ok_or(MeshError::OutOfRange(
            "file manifest chunk count out of range",
        ))?;
        let manifest = Self {
            transfer_id,
            file_name,
            mime_type,
            total_size,
            chunk_size,
            chunk_count,
            sha256,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Every structural invariant a receiver relies on, checked in one place.
    ///
    /// Called by both `encode` and `decode`, because the fields are public and a
    /// caller can build a manifest without `new`. The `chunk_count` check is the
    /// load-bearing one: a manifest whose count disagrees with
    /// `ceil(total / chunk)` either names chunks that can never arrive (so the
    /// transfer never completes and the spool entry lives until it expires) or
    /// fewer than the file needs (so a "complete" transfer is missing its tail).
    pub fn validate(&self) -> Result<(), MeshError> {
        if self.total_size == 0 {
            return Err(MeshError::OutOfRange(
                "file manifest declares an empty file",
            ));
        }
        if self.total_size > MAX_STREAMED_FILE_BYTES as u64 {
            return Err(MeshError::LengthExceeded {
                declared: self.total_size.min(usize::MAX as u64) as usize,
                max: MAX_STREAMED_FILE_BYTES,
            });
        }
        let chunk_size = self.chunk_size as usize;
        if !(MIN_CHUNK_BYTES..=MAX_CHUNK_BYTES).contains(&chunk_size) {
            return Err(MeshError::OutOfRange(
                "file manifest chunk size outside the accepted range",
            ));
        }
        match chunk_count_for(self.total_size, self.chunk_size) {
            Some(expected) if expected == self.chunk_count => Ok(()),
            _ => Err(MeshError::OutOfRange(
                "file manifest chunk count disagrees with its size and chunk size",
            )),
        }
    }

    /// Byte offset and length of one chunk within the finished file.
    ///
    /// The last chunk is short. `None` for an index past the end, which is what
    /// makes a hostile index a refusal rather than a write past the file.
    #[must_use]
    pub fn chunk_span(&self, index: u32) -> Option<(u64, usize)> {
        if index >= self.chunk_count {
            return None;
        }
        let offset = index as u64 * self.chunk_size as u64;
        let remaining = self.total_size.saturating_sub(offset);
        let len = remaining.min(self.chunk_size as u64) as usize;
        if len == 0 { None } else { Some((offset, len)) }
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        self.validate()?;
        let mut out = Vec::with_capacity(96);

        put_tlv_u16_len(&mut out, TLV_TRANSFER_ID, &self.transfer_id)?;
        put_tlv_u16_len(&mut out, TLV_TOTAL_SIZE, &self.total_size.to_be_bytes())?;
        put_tlv_u16_len(&mut out, TLV_CHUNK_SIZE, &self.chunk_size.to_be_bytes())?;
        put_tlv_u16_len(&mut out, TLV_CHUNK_COUNT, &self.chunk_count.to_be_bytes())?;
        put_tlv_u16_len(&mut out, TLV_SHA256, &self.sha256)?;

        // A name or MIME type too long for its length field is dropped rather
        // than failing the encode, as `BitchatFilePacket` does: the transfer is
        // still worth making.
        if let Some(name) = &self.file_name
            && name.len() <= u16::MAX as usize
        {
            put_tlv_u16_len(&mut out, TLV_FILE_NAME, name.as_bytes())?;
        }
        if let Some(mime) = &self.mime_type
            && mime.len() <= u16::MAX as usize
        {
            put_tlv_u16_len(&mut out, TLV_MIME_TYPE, mime.as_bytes())?;
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);

        let mut transfer_id: Option<[u8; TRANSFER_ID_LEN]> = None;
        let mut total_size: Option<u64> = None;
        let mut chunk_size: Option<u32> = None;
        let mut chunk_count: Option<u32> = None;
        let mut sha256: Option<[u8; 32]> = None;
        let mut file_name: Option<String> = None;
        let mut mime_type: Option<String> = None;

        while !reader.is_empty() {
            let tlv_type = reader.u8()?;
            let length = reader.u16_be()? as usize;
            let value = reader.take(length)?;

            match tlv_type {
                TLV_TRANSFER_ID => {
                    transfer_id = Some(fixed::<TRANSFER_ID_LEN>(
                        value,
                        "file_manifest.transfer_id",
                    )?);
                }
                TLV_TOTAL_SIZE => {
                    total_size = Some(u64::from_be_bytes(fixed::<8>(
                        value,
                        "file_manifest.total_size",
                    )?));
                }
                TLV_CHUNK_SIZE => {
                    chunk_size = Some(u32::from_be_bytes(fixed::<4>(
                        value,
                        "file_manifest.chunk_size",
                    )?));
                }
                TLV_CHUNK_COUNT => {
                    chunk_count = Some(u32::from_be_bytes(fixed::<4>(
                        value,
                        "file_manifest.chunk_count",
                    )?));
                }
                TLV_SHA256 => {
                    sha256 = Some(fixed::<32>(value, "file_manifest.sha256")?);
                }
                TLV_FILE_NAME => file_name = Some(utf8(value, "file_manifest.file_name")?),
                TLV_MIME_TYPE => mime_type = Some(utf8(value, "file_manifest.mime_type")?),
                // Forward compatibility: an unknown TLV is skipped, its length
                // read as the two-byte form every field here uses.
                _ => {}
            }
        }

        // Every required field is required, unlike the file packet's optional
        // metadata. A manifest missing its digest or its chunking is not a
        // degraded manifest, it is a transfer that cannot be verified or
        // assembled — and a wrong-width field is a missing field, not a
        // reinterpretable one.
        let manifest = Self {
            transfer_id: transfer_id.ok_or(MeshError::MissingField("file_manifest.transfer_id"))?,
            file_name,
            mime_type,
            total_size: total_size.ok_or(MeshError::MissingField("file_manifest.total_size"))?,
            chunk_size: chunk_size.ok_or(MeshError::MissingField("file_manifest.chunk_size"))?,
            chunk_count: chunk_count.ok_or(MeshError::MissingField("file_manifest.chunk_count"))?,
            sha256: sha256.ok_or(MeshError::MissingField("file_manifest.sha256"))?,
        };
        manifest.validate()?;
        Ok(manifest)
    }
}

/// One chunk of a transfer.
///
/// Carries no digest of its own. That is a decision with a cost: a corrupt
/// chunk is only detected by the whole-file digest, so the whole file is
/// discarded rather than the one chunk re-requested. The alternative costs 32
/// bytes on *every* chunk to identify a chunk that, having passed a packet
/// signature, was almost certainly not corrupted in transit. Measure the real
/// corruption rate before paying that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChunk {
    pub transfer_id: [u8; TRANSFER_ID_LEN],
    pub index: u32,
    pub bytes: Vec<u8>,
}

impl FileChunk {
    pub fn new(
        transfer_id: [u8; TRANSFER_ID_LEN],
        index: u32,
        bytes: Vec<u8>,
    ) -> Result<Self, MeshError> {
        let chunk = Self {
            transfer_id,
            index,
            bytes,
        };
        chunk.validate()?;
        Ok(chunk)
    }

    fn validate(&self) -> Result<(), MeshError> {
        if self.bytes.is_empty() {
            return Err(MeshError::OutOfRange("file chunk carries no bytes"));
        }
        if self.bytes.len() > MAX_CHUNK_BYTES {
            return Err(MeshError::LengthExceeded {
                declared: self.bytes.len(),
                max: MAX_CHUNK_BYTES,
            });
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        self.validate()?;
        let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + self.bytes.len());
        out.extend_from_slice(&self.transfer_id);
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.bytes);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);
        let transfer_id = reader.array::<TRANSFER_ID_LEN>()?;
        let index = reader.u32_be()?;
        let chunk = Self {
            transfer_id,
            index,
            bytes: reader.take_rest().to_vec(),
        };
        chunk.validate()?;
        Ok(chunk)
    }
}

/// The chunks a receiver is still missing.
///
/// **Retransmission is receiver-requested, and that inversion is the point.** A
/// broadcast has no acknowledgement, so a sender cannot know what landed — any
/// sender-driven retransmit would be guessing. The receiver is the only party
/// that knows, so the receiver asks.
///
/// Broadcast rather than directed to the manifest's sender: it costs a few dozen
/// bytes of flood and keeps the broadcast-only gate identical across all three
/// of these types, which is the gate that stops a cleartext file appearing in
/// what the UI calls an encrypted DM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChunkRequest {
    pub transfer_id: [u8; TRANSFER_ID_LEN],
    /// Ascending, deduplicated, and at most [`MAX_REQUESTED_CHUNKS`] long.
    pub indices: Vec<u32>,
}

impl FileChunkRequest {
    /// Sorts, deduplicates and truncates `indices` to what one request can
    /// carry.
    ///
    /// Truncation rather than refusal, for the reason
    /// [`MAX_REQUESTED_CHUNKS`] gives. Sorting is not cosmetic: it makes the
    /// truncation take the *earliest* missing chunks, so a receiver asking
    /// repeatedly makes forward progress from the front of the file instead of
    /// re-asking for an arbitrary subset.
    pub fn new(
        transfer_id: [u8; TRANSFER_ID_LEN],
        mut indices: Vec<u32>,
    ) -> Result<Self, MeshError> {
        indices.sort_unstable();
        indices.dedup();
        indices.truncate(MAX_REQUESTED_CHUNKS);
        if indices.is_empty() {
            return Err(MeshError::OutOfRange("file chunk request names no chunks"));
        }
        Ok(Self {
            transfer_id,
            indices,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        if self.indices.is_empty() {
            return Err(MeshError::OutOfRange("file chunk request names no chunks"));
        }
        let named = self.indices.len().min(MAX_REQUESTED_CHUNKS);
        let mut out = Vec::with_capacity(TRANSFER_ID_LEN + 2 + named * 4);
        out.extend_from_slice(&self.transfer_id);
        out.extend_from_slice(&(named as u16).to_be_bytes());
        for index in self.indices.iter().take(named) {
            out.extend_from_slice(&index.to_be_bytes());
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);
        let transfer_id = reader.array::<TRANSFER_ID_LEN>()?;
        let count = reader.u16_be()? as usize;
        if count == 0 {
            return Err(MeshError::OutOfRange("file chunk request names no chunks"));
        }
        // The declared count must be *present*: a request claiming 60 chunks in
        // a buffer holding two is truncation, and reading what happens to be
        // there would serve chunks nobody asked for.
        let named = reader.take(count.saturating_mul(4))?;
        let indices: Vec<u32> = named
            .chunks_exact(4)
            .take(MAX_REQUESTED_CHUNKS)
            .map(|raw| u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
            .collect();
        Ok(Self {
            transfer_id,
            indices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: [u8; TRANSFER_ID_LEN] = [0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8];

    fn manifest() -> FileManifest {
        FileManifest::new(
            ID,
            Some("clip.mp4".to_string()),
            Some("video/mp4".to_string()),
            100_000,
            CHUNK_BYTES as u32,
            [0x7Fu8; 32],
        )
        .expect("valid")
    }

    // -- the arithmetic the whole design rests on --------------------------

    #[test]
    fn a_whole_chunk_reassembles_well_inside_the_fragment_lifetime() {
        // The reason chunking works where a single packet cannot. If this fails,
        // CHUNK_BYTES has been raised past what one reassembly window can carry
        // and every large transfer silently stops completing.
        let payload = CHUNK_HEADER_LEN + CHUNK_BYTES;
        assert!(
            fits_fragment_lifetime(payload),
            "a chunk must land inside the {FRAGMENT_LIFETIME_SECS}s window"
        );

        // And with room to spare, because the pessimistic goodput figure is
        // itself optimistic next to a duty-cycled radio sharing an antenna.
        let seconds = wire_bytes_for_payload(payload) / PESSIMISTIC_GOODPUT_BYTES_PER_SEC;
        assert!(
            (seconds as u64) * 2 < FRAGMENT_LIFETIME_SECS,
            "a chunk takes {seconds}s of a {FRAGMENT_LIFETIME_SECS}s window; \
             less than 2x margin is not enough on a radio that stalls"
        );
    }

    #[test]
    fn a_one_mebibyte_single_packet_still_cannot_land_but_its_chunks_can() {
        // Both halves of the P9 argument in one assertion pair.
        assert!(
            !fits_fragment_lifetime(1024 * 1024),
            "if a 1 MiB single packet ever fits, P8's path was enough and this \
             protocol should not exist"
        );
        let chunks = (1024 * 1024usize).div_ceil(CHUNK_BYTES);
        assert_eq!(chunks, 32, "1 MiB is 32 chunks at 32 KiB");
        for _ in 0..chunks {
            assert!(fits_fragment_lifetime(CHUNK_HEADER_LEN + CHUNK_BYTES));
        }
    }

    #[test]
    fn the_single_packet_threshold_is_one_the_fragmenter_can_actually_carry() {
        // The send-time size fork. A threshold above what one reassembly window
        // carries would route files onto the path that cannot deliver them.
        assert!(fits_fragment_lifetime(MAX_SINGLE_PACKET_FILE_BYTES));
        assert_eq!(MAX_SINGLE_PACKET_FILE_BYTES, CHUNK_BYTES);
    }

    #[test]
    fn a_chunk_request_never_needs_fragmenting() {
        // A retransmit request subject to the reassembly window it exists to
        // work around would be self-defeating.
        let full = FileChunkRequest::new(ID, (0..MAX_REQUESTED_CHUNKS as u32 * 4).collect())
            .expect("valid");
        let encoded = full.encode().expect("encodes");
        assert_eq!(
            encoded.len(),
            TRANSFER_ID_LEN + 2 + MAX_REQUESTED_CHUNKS * 4
        );
        assert_eq!(
            wire_bytes_for_payload(encoded.len()),
            WIRE_BLOCK_BYTES,
            "the whole request has to fit one BLE frame"
        );
    }

    #[test]
    fn the_cap_is_sized_by_patience_and_the_estimate_says_so() {
        let optimistic = estimated_seconds(
            MAX_STREAMED_FILE_BYTES as u64,
            CHUNK_BYTES as u32,
            OPTIMISTIC_GOODPUT_BYTES_PER_SEC,
        );
        let pessimistic = estimated_seconds(
            MAX_STREAMED_FILE_BYTES as u64,
            CHUNK_BYTES as u32,
            PESSIMISTIC_GOODPUT_BYTES_PER_SEC,
        );
        assert!(
            optimistic > 20 * 60 && optimistic < 45 * 60,
            "the ceiling is ~half an hour at the optimistic rate, got {optimistic}s"
        );
        assert!(
            pessimistic < 3 * 60 * 60,
            "and never a multi-hour commitment, got {pessimistic}s"
        );
        assert_eq!(estimated_seconds(1, 0, 1), u64::MAX, "no divide by zero");
        assert_eq!(estimated_seconds(1, 1, 0), u64::MAX);
    }

    // -- manifest ----------------------------------------------------------

    #[test]
    fn a_manifest_round_trips_every_field() {
        let decoded = FileManifest::decode(&manifest().encode().unwrap()).unwrap();
        assert_eq!(decoded, manifest());
        assert_eq!(decoded.chunk_count, 4, "100000 / 32768 rounds up to 4");
    }

    #[test]
    fn manifest_metadata_is_optional_and_the_rest_is_not() {
        let bare = FileManifest::new(ID, None, None, 2048, 1024, [0u8; 32]).unwrap();
        let decoded = FileManifest::decode(&bare.encode().unwrap()).unwrap();
        assert_eq!(decoded.file_name, None);
        assert_eq!(decoded.mime_type, None);

        // Drop the digest TLV and the manifest is no longer verifiable, which is
        // not a degraded manifest — it is a transfer with no way to tell a
        // finished file from a wrong one.
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_TRANSFER_ID, &ID).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_TOTAL_SIZE, &2048u64.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CHUNK_SIZE, &1024u32.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CHUNK_COUNT, &2u32.to_be_bytes()).unwrap();
        assert!(matches!(
            FileManifest::decode(&bytes),
            Err(MeshError::MissingField("file_manifest.sha256"))
        ));
    }

    #[test]
    fn a_manifest_whose_chunk_count_lies_is_refused() {
        // The interesting attack on a manifest. A count larger than the file
        // needs means the transfer can never complete and the spool entry sits
        // until it expires; a count smaller means a "complete" transfer is
        // missing its tail and the digest check is the only thing left between
        // that and a truncated file on disk.
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_TRANSFER_ID, &ID).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_TOTAL_SIZE, &4096u64.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CHUNK_SIZE, &1024u32.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CHUNK_COUNT, &9u32.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_SHA256, &[0u8; 32]).unwrap();
        assert!(matches!(
            FileManifest::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn a_manifest_chunk_size_outside_the_accepted_range_is_refused() {
        // Too small: `chunk_count` would name millions of chunks and the
        // receiver would size a bitmap for all of them.
        let tiny = FileManifest::new(ID, None, None, 1_000_000, 1, [0u8; 32]);
        assert!(matches!(tiny, Err(MeshError::OutOfRange(_))));

        // Too large: a chunk that cannot reassemble inside the lifetime window
        // is a transfer that can never finish, so the manifest is refused
        // rather than accepted and stalled.
        let huge = FileManifest::new(
            ID,
            None,
            None,
            1_000_000,
            (MAX_CHUNK_BYTES + 1) as u32,
            [0u8; 32],
        );
        assert!(matches!(huge, Err(MeshError::OutOfRange(_))));
        assert!(!fits_fragment_lifetime(
            CHUNK_HEADER_LEN + MAX_CHUNK_BYTES * 4
        ));
    }

    #[test]
    fn a_manifest_above_the_time_cap_is_refused() {
        let over = FileManifest::new(
            ID,
            None,
            None,
            MAX_STREAMED_FILE_BYTES as u64 + 1,
            CHUNK_BYTES as u32,
            [0u8; 32],
        );
        assert!(matches!(
            over,
            Err(MeshError::LengthExceeded {
                max: MAX_STREAMED_FILE_BYTES,
                ..
            })
        ));
        // And an empty file, which has no chunks and therefore no transfer.
        assert!(matches!(
            FileManifest::new(ID, None, None, 0, CHUNK_BYTES as u32, [0u8; 32]),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn chunk_spans_tile_the_file_exactly_and_stop_at_the_end() {
        let manifest = FileManifest::new(ID, None, None, 2500, 1024, [0u8; 32]).unwrap();
        assert_eq!(manifest.chunk_count, 3);
        assert_eq!(manifest.chunk_span(0), Some((0, 1024)));
        assert_eq!(manifest.chunk_span(1), Some((1024, 1024)));
        assert_eq!(
            manifest.chunk_span(2),
            Some((2048, 452)),
            "the last chunk is short, and writing a full one would extend the file"
        );
        assert_eq!(
            manifest.chunk_span(3),
            None,
            "an index past the end must not resolve to an offset to write at"
        );
        assert_eq!(manifest.chunk_span(u32::MAX), None);

        let total: usize = (0..manifest.chunk_count)
            .map(|i| manifest.chunk_span(i).unwrap().1)
            .sum();
        assert_eq!(total as u64, manifest.total_size);
    }

    #[test]
    fn manifest_truncation_never_yields_a_manifest_that_lies() {
        // The required TLVs are written first, so a prefix that happens to end
        // after the digest *does* decode — as a manifest with no name and no MIME
        // type, which is a legitimate manifest. What must never happen is a
        // decode whose chunking or digest differs from what was encoded, because
        // that is a receiver assembling a different file than the sender sent.
        let full = manifest();
        let encoded = full.encode().unwrap();
        for cut in 0..encoded.len() {
            if let Ok(partial) = FileManifest::decode(&encoded[..cut]) {
                assert_eq!(partial.transfer_id, full.transfer_id, "cut {cut}");
                assert_eq!(partial.total_size, full.total_size, "cut {cut}");
                assert_eq!(partial.chunk_size, full.chunk_size, "cut {cut}");
                assert_eq!(partial.chunk_count, full.chunk_count, "cut {cut}");
                assert_eq!(partial.sha256, full.sha256, "cut {cut}");
            }
        }
        // And nothing shorter than all five required TLVs decodes at all.
        let required_len = 3 * 5 + TRANSFER_ID_LEN + 8 + 4 + 4 + 32;
        for cut in 0..required_len {
            assert!(
                FileManifest::decode(&encoded[..cut]).is_err(),
                "a manifest missing a required field at {cut} must not decode"
            );
        }
        assert_eq!(FileManifest::decode(&encoded).unwrap(), full);
    }

    #[test]
    fn unknown_manifest_tlvs_are_skipped() {
        let mut bytes = manifest().encode().unwrap();
        put_tlv_u16_len(&mut bytes, 0x7F, &[0xDE, 0xAD]).unwrap();
        assert_eq!(FileManifest::decode(&bytes).unwrap(), manifest());
    }

    #[test]
    fn a_wrong_width_manifest_field_is_a_failure_not_a_reinterpretation() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_TRANSFER_ID, &[1, 2, 3]).unwrap();
        assert!(matches!(
            FileManifest::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "file_manifest.transfer_id",
                ..
            })
        ));
    }

    #[test]
    fn invalid_utf8_manifest_metadata_is_rejected() {
        let mut bytes = manifest().encode().unwrap();
        put_tlv_u16_len(&mut bytes, TLV_FILE_NAME, &[0xFF, 0xFE]).unwrap();
        assert!(matches!(
            FileManifest::decode(&bytes),
            Err(MeshError::InvalidUtf8 {
                field: "file_manifest.file_name"
            })
        ));
    }

    // -- chunk -------------------------------------------------------------

    #[test]
    fn a_chunk_round_trips_and_its_header_is_where_it_says() {
        let chunk = FileChunk::new(ID, 0x0102_0304, vec![9, 8, 7]).unwrap();
        let encoded = chunk.encode().unwrap();
        assert_eq!(
            encoded,
            vec![
                0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 1, 2, 3, 4, 9, 8, 7
            ]
        );
        assert_eq!(FileChunk::decode(&encoded).unwrap(), chunk);
    }

    #[test]
    fn an_empty_or_oversized_chunk_is_refused() {
        assert!(matches!(
            FileChunk::new(ID, 0, Vec::new()),
            Err(MeshError::OutOfRange(_))
        ));
        // An empty chunk on the wire too: a header with nothing after it would
        // otherwise mark an index received without contributing a byte.
        let mut header = ID.to_vec();
        header.extend_from_slice(&0u32.to_be_bytes());
        assert!(matches!(
            FileChunk::decode(&header),
            Err(MeshError::OutOfRange(_))
        ));
        assert!(matches!(
            FileChunk::new(ID, 0, vec![0u8; MAX_CHUNK_BYTES + 1]),
            Err(MeshError::LengthExceeded { .. })
        ));
    }

    #[test]
    fn a_chunk_shorter_than_its_header_is_truncation() {
        for cut in 0..CHUNK_HEADER_LEN {
            assert!(matches!(
                FileChunk::decode(&vec![0u8; cut]),
                Err(MeshError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn a_full_size_chunk_round_trips() {
        let chunk = FileChunk::new(ID, 41, vec![0x5A; MAX_CHUNK_BYTES]).unwrap();
        let decoded = FileChunk::decode(&chunk.encode().unwrap()).unwrap();
        assert_eq!(decoded.bytes.len(), MAX_CHUNK_BYTES);
        assert_eq!(decoded.index, 41);
    }

    // -- chunk request -----------------------------------------------------

    #[test]
    fn a_chunk_request_round_trips_sorted_and_deduplicated() {
        let request = FileChunkRequest::new(ID, vec![7, 1, 7, 3]).unwrap();
        assert_eq!(request.indices, vec![1, 3, 7]);
        assert_eq!(
            FileChunkRequest::decode(&request.encode().unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn an_overlong_chunk_request_is_truncated_to_the_earliest_chunks() {
        // The `encode_fragment_id_filter` precedent: truncate rather than refuse,
        // and truncate from the *front* so repeated asking makes progress.
        let wanted: Vec<u32> = (0..500).rev().collect();
        let request = FileChunkRequest::new(ID, wanted).unwrap();
        assert_eq!(request.indices.len(), MAX_REQUESTED_CHUNKS);
        assert_eq!(request.indices.first(), Some(&0));
        assert_eq!(
            request.indices.last(),
            Some(&(MAX_REQUESTED_CHUNKS as u32 - 1))
        );
    }

    #[test]
    fn a_chunk_request_naming_nothing_is_refused() {
        assert!(matches!(
            FileChunkRequest::new(ID, Vec::new()),
            Err(MeshError::OutOfRange(_))
        ));
        let mut bytes = ID.to_vec();
        bytes.extend_from_slice(&0u16.to_be_bytes());
        assert!(matches!(
            FileChunkRequest::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn a_chunk_request_claiming_more_than_it_carries_is_truncation() {
        // Reading whatever follows would make us serve chunks nobody asked for,
        // which on this link is airtime the mesh cannot spare.
        let mut bytes = ID.to_vec();
        bytes.extend_from_slice(&60u16.to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes());
        assert!(matches!(
            FileChunkRequest::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn a_chunk_request_decode_stops_at_the_cap() {
        // A peer that ignores the encoder's cap does not get to make us allocate
        // past it.
        let over = MAX_REQUESTED_CHUNKS + 10;
        let mut bytes = ID.to_vec();
        bytes.extend_from_slice(&(over as u16).to_be_bytes());
        for index in 0..over as u32 {
            bytes.extend_from_slice(&index.to_be_bytes());
        }
        let decoded = FileChunkRequest::decode(&bytes).unwrap();
        assert_eq!(decoded.indices.len(), MAX_REQUESTED_CHUNKS);
    }
}
