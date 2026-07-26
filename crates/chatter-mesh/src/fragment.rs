//! Outbound fragment planning and bounded inbound reassembly.
//!
//! Ported from `Services/BLE/BLEOutboundFragmentPlanner.swift`,
//! `BLEFragmentAssemblyBuffer.swift`, and `BLEFragmentHandler.swift`.
//!
//! What gets fragmented is the *whole encoded packet*, not its payload: the
//! planner calls `packet.toBinaryData(padding:)` and slices those bytes, so a
//! completed reassembly hands the caller a buffer to feed straight back into
//! [`crate::codec::decode`], flags and compression intact.
//!
//! The assembler is deliberately bounded in three directions — concurrent
//! streams, bytes per stream, and stream lifetime — because every one of those
//! is attacker-controlled: fragments bypass the packet deduplicator, and a peer
//! can open a stream by sending one piece of a message it never finishes.

use std::collections::{BTreeMap, HashMap};

use crate::error::MeshError;
use crate::packet::{BROADCAST_RECIPIENT, BitchatPacket, MessageType, SENDER_ID_LEN};

/// `TransportConfig.bleDefaultFragmentSize` — ~512 byte MTU minus protocol
/// overhead.
pub const DEFAULT_FRAGMENT_SIZE: usize = 469;

/// `TransportConfig.bleMaxInFlightAssemblies`.
pub const MAX_IN_FLIGHT_ASSEMBLIES: usize = 128;

/// `TransportConfig.bleFragmentLifetimeSeconds`, measured from the first
/// fragment of a stream — not from the most recent one.
pub const FRAGMENT_LIFETIME_SECS: u64 = 30;

/// `FileTransferLimits.maxPayloadBytes` — the reassembly ceiling for everything
/// except a file transfer.
pub const MAX_REASSEMBLED_BYTES: usize = 1024 * 1024;

/// `FileTransferLimits.maxFramedFileBytes` — the ceiling for a reassembled
/// `FileTransfer`, which carries `maxPayloadBytes` of content plus TLV metadata
/// (two `u16`-length fields at their maximum) and the binary envelope
/// (16-byte v2 header + 8 sender + 8 recipient + 64 signature).
pub const MAX_FRAMED_FILE_BYTES: usize =
    MAX_REASSEMBLED_BYTES + (18 + 2 * u16::MAX as usize) + (16 + 8 + 8 + 64);

// A framed file must always leave room for its own envelope on top of the raw
// payload ceiling. Checked at compile time rather than in a test, since both
// sides are constants and a runtime assert would never observe a change.
const _: () = assert!(MAX_FRAMED_FILE_BYTES > MAX_REASSEMBLED_BYTES);

/// `BLEOutboundFragmentPlanner.minimumChunkSize`. A smaller request is raised to
/// this, which also makes a zero `max_chunk` harmless.
pub const MIN_FRAGMENT_CHUNK: usize = 64;

/// Ceiling the assembler enforces on a stream's `total`; the planner refuses to
/// emit more, since no bitchat peer would reassemble them.
pub const MAX_FRAGMENT_COUNT: usize = 10_000;

/// 8-byte stream ID + `u16` index + `u16` total + 1 original-type byte.
pub const FRAGMENT_HEADER_LEN: usize = 13;

/// `RequestSyncPacket.maxFragmentIdFilterCount` — the wire filter cannot name
/// more streams than this in one request.
pub const MAX_STALLED_STREAM_IDS: usize = 60;

/// The fixed prefix every fragment payload carries.
///
/// Layout, big-endian throughout:
/// `[0..8] stream id | [8..10] index | [10..12] total | [12] original type`,
/// with the chunk from byte 13. `original_type` is not in the design's sketch of
/// this struct but is on the wire and is load-bearing: it selects the reassembly
/// size limit, exactly as `BLEFragmentHeader.originalType` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentHeader {
    pub fragment_id: [u8; 8],
    pub index: u16,
    pub total: u16,
    pub original_type: u8,
}

impl FragmentHeader {
    /// Reads the header from a fragment packet, returning it with the chunk that
    /// follows.
    ///
    /// Mirrors `BLEFragmentHeader.init?(packet:)`: a payload shorter than the
    /// header, a zero or implausible `total`, or an `index` outside `0..total`
    /// is rejected rather than assembled.
    pub fn parse(packet: &BitchatPacket) -> Result<(Self, &[u8]), MeshError> {
        let payload = packet.payload.as_slice();
        let Some(head) = payload.get(..FRAGMENT_HEADER_LEN) else {
            return Err(MeshError::Truncated {
                offset: payload.len(),
                needed: FRAGMENT_HEADER_LEN - payload.len(),
            });
        };

        let mut fragment_id = [0u8; 8];
        fragment_id.copy_from_slice(&head[..8]);
        let index = u16::from_be_bytes([head[8], head[9]]);
        let total = u16::from_be_bytes([head[10], head[11]]);
        let original_type = head[12];

        if total == 0 || total as usize > MAX_FRAGMENT_COUNT {
            return Err(MeshError::OutOfRange("fragment total out of range"));
        }
        if index >= total {
            return Err(MeshError::OutOfRange("fragment index beyond total"));
        }

        let chunk = payload.get(FRAGMENT_HEADER_LEN..).unwrap_or(&[]);
        Ok((
            Self {
                fragment_id,
                index,
                total,
                original_type,
            },
            chunk,
        ))
    }
}

/// Splits a packet into fragment packets.
///
/// The packet is encoded once (padded per `BLEOutboundPacketPolicy.padsBLEFrame`,
/// which pads only Noise traffic) and the bytes are sliced into `max_chunk`
/// pieces, raised to [`MIN_FRAGMENT_CHUNK`] if smaller. Per `SOURCE_ROUTING.md`
/// §5 the fragments inherit the parent's route and are marked v2 when it has
/// one — a fragment that fell back to v1 or dropped the route would flood,
/// negating the route.
pub fn plan_fragments(
    packet: &BitchatPacket,
    max_chunk: usize,
    fragment_id: [u8; 8],
) -> Result<Vec<BitchatPacket>, MeshError> {
    let full = crate::codec::encode(packet, pads_ble_frame(packet.msg_type))?;
    if full.is_empty() {
        return Err(MeshError::OutOfRange("nothing to fragment"));
    }

    let chunk_size = max_chunk.max(MIN_FRAGMENT_CHUNK);
    let total = full.len().div_ceil(chunk_size);
    if total > MAX_FRAGMENT_COUNT {
        return Err(MeshError::LengthExceeded {
            declared: total,
            max: MAX_FRAGMENT_COUNT,
        });
    }
    // Guarded by MAX_FRAGMENT_COUNT above, so the cast cannot wrap.
    let total_u16 = total as u16;

    let routed = packet.route.as_ref().is_some_and(|route| !route.is_empty());
    let version = if routed { 2 } else { 1 };

    let mut out = Vec::with_capacity(total);
    for (index, chunk) in full.chunks(chunk_size).enumerate() {
        let mut payload = Vec::with_capacity(FRAGMENT_HEADER_LEN + chunk.len());
        payload.extend_from_slice(&fragment_id);
        payload.extend_from_slice(&(index as u16).to_be_bytes());
        payload.extend_from_slice(&total_u16.to_be_bytes());
        payload.push(packet.msg_type);
        payload.extend_from_slice(chunk);

        out.push(BitchatPacket {
            version,
            msg_type: MessageType::Fragment.as_u8(),
            ttl: packet.ttl,
            timestamp: packet.timestamp,
            sender_id: packet.sender_id,
            recipient_id: packet.recipient_id,
            payload,
            // Fragments are never signed: the signature rides inside the
            // reassembled bytes, covering the original packet.
            signature: None,
            route: packet.route.clone(),
            is_rsr: packet.is_rsr,
        });
    }

    Ok(out)
}

/// Which packet types are padded before going on the wire.
///
/// Ported from `BLEOutboundPacketPolicy.padsBLEFrame(for:)`. Voice frames are
/// deliberately unpadded: padding a ~490-byte signed voice packet to the 512
/// block would push it over the MTU and into this very fragment path.
pub fn pads_ble_frame(msg_type: u8) -> bool {
    matches!(
        MessageType::from_u8(msg_type),
        Some(MessageType::NoiseEncrypted | MessageType::NoiseHandshake)
    )
}

/// A reassembly is identified by sender *and* stream ID: two peers picking the
/// same random 8-byte ID must not collide into one buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct StreamKey {
    sender: [u8; SENDER_ID_LEN],
    id: [u8; 8],
}

#[derive(Clone, Debug)]
struct Assembly {
    /// `total` from the fragment that opened the stream.
    total: u16,
    /// When the stream opened. Drives expiry.
    started_at: u64,
    /// Whether this stream reassembles a broadcast packet. Only broadcast
    /// streams are recoverable by a targeted REQUEST_SYNC — peers archive
    /// nothing else for gossip sync.
    is_broadcast: bool,
    /// When a *new* index last landed. Drives stall detection.
    last_fragment_at: u64,
    chunks: BTreeMap<u16, Vec<u8>>,
}

/// Bounded inbound fragment reassembly.
#[derive(Clone, Debug, Default)]
pub struct FragmentAssembler {
    streams: HashMap<StreamKey, Assembly>,
}

impl FragmentAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of reassemblies currently held.
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// Drops every in-flight reassembly.
    pub fn clear(&mut self) {
        self.streams.clear();
    }

    /// Takes one fragment, returning the reassembled original bytes once the
    /// final piece lands.
    ///
    /// Errors: a malformed header, or a stream whose projected size exceeds its
    /// limit — in which case the whole stream is dropped, matching
    /// `AppendResult.oversized`.
    pub fn accept(
        &mut self,
        packet: &BitchatPacket,
        now_unix_secs: u64,
    ) -> Result<Option<Vec<u8>>, MeshError> {
        let (header, chunk) = FragmentHeader::parse(packet)?;
        let key = StreamKey {
            sender: packet.sender_id,
            id: header.fragment_id,
        };
        // No recipient field at all also means broadcast, per
        // `BLEFragmentHeader.isBroadcastFragment`.
        let is_broadcast = packet
            .recipient_id
            .is_none_or(|recipient| recipient == BROADCAST_RECIPIENT);

        if !self.streams.contains_key(&key) {
            self.make_room();
            self.streams.insert(
                key,
                Assembly {
                    total: header.total,
                    started_at: now_unix_secs,
                    is_broadcast,
                    last_fragment_at: now_unix_secs,
                    chunks: BTreeMap::new(),
                },
            );
        }

        let limit = assembly_limit(header.original_type);
        let held: usize = self
            .streams
            .get(&key)
            .map(|assembly| assembly.chunks.values().map(Vec::len).sum())
            .unwrap_or(0);
        let projected = held.saturating_add(chunk.len());
        if projected > limit {
            self.streams.remove(&key);
            return Err(MeshError::LengthExceeded {
                declared: projected,
                max: limit,
            });
        }

        let Some(assembly) = self.streams.get_mut(&key) else {
            // Unreachable: inserted above when absent. Handled as a drop rather
            // than a panic, because this crate never panics on radio input.
            return Ok(None);
        };

        // Only real progress resets the stall clock: fragments bypass the packet
        // deduplicator, so a relayed duplicate of an index we already hold must
        // not keep suppressing the targeted REQUEST_SYNC for a stalled stream.
        let is_new_index = assembly
            .chunks
            .insert(header.index, chunk.to_vec())
            .is_none();
        if is_new_index {
            assembly.last_fragment_at = now_unix_secs;
        }

        // Completion is judged against *this* fragment's `total`, as in the
        // Swift, not against the total recorded when the stream opened.
        if assembly.chunks.len() != header.total as usize {
            return Ok(None);
        }

        let mut reassembled = Vec::with_capacity(held + chunk.len());
        for index in 0..header.total {
            if let Some(piece) = assembly.chunks.get(&index) {
                reassembled.extend_from_slice(piece);
            }
        }
        self.streams.remove(&key);
        Ok(Some(reassembled))
    }

    /// Drops reassemblies older than [`FRAGMENT_LIFETIME_SECS`], measured from
    /// the stream's first fragment.
    ///
    /// Returns how many were dropped, so the caller can say so. This layer is
    /// sans-IO and does not log, but a silent eviction is the difference between
    /// "the sender is too slow" and "the file vanished": a transfer that misses
    /// the window leaves no other trace anywhere, on either device. That cost a
    /// long debugging session once.
    pub fn evict_expired(&mut self, now_unix_secs: u64) -> usize {
        let before = self.streams.len();
        self.streams
            .retain(|_, assembly| now_unix_secs.saturating_sub(assembly.started_at) <= FRAGMENT_LIFETIME_SECS);
        before - self.streams.len()
    }

    /// Stream IDs of incomplete *broadcast* reassemblies that have seen no new
    /// fragment for `stall_secs` — candidates for a targeted REQUEST_SYNC.
    ///
    /// Oldest stall first, capped at [`MAX_STALLED_STREAM_IDS`] because the wire
    /// filter cannot carry more. Directed reassemblies are excluded: peers
    /// archive only broadcast fragments for gossip sync, so a targeted request
    /// could not recover them.
    ///
    /// The Swift also stamps each returned stream with a `lastResyncRequestAt`
    /// so it is not re-requested within a retry window. That bookkeeping cannot
    /// live here — the frozen signature takes `&self` — so spacing repeat
    /// requests (`TransportConfig.bleFragmentResyncRetrySeconds`, 10 s) is the
    /// caller's job.
    pub fn stalled_stream_ids(&self, now_unix_secs: u64, stall_secs: u64) -> Vec<[u8; 8]> {
        let mut candidates: Vec<(u64, StreamKey)> = self
            .streams
            .iter()
            .filter(|(_, assembly)| {
                assembly.is_broadcast
                    && assembly.chunks.len() < assembly.total as usize
                    && now_unix_secs.saturating_sub(assembly.last_fragment_at) >= stall_secs
            })
            .map(|(key, assembly)| (assembly.last_fragment_at, *key))
            .collect();

        // Oldest stall first; the key breaks ties so the result is deterministic
        // (the Swift's dictionary order is not).
        candidates.sort_unstable();
        candidates
            .into_iter()
            .take(MAX_STALLED_STREAM_IDS)
            .map(|(_, key)| key.id)
            .collect()
    }

    /// Evicts the oldest reassembly when the in-flight cap is already reached.
    fn make_room(&mut self) {
        if self.streams.len() < MAX_IN_FLIGHT_ASSEMBLIES {
            return;
        }
        let oldest = self
            .streams
            .iter()
            .map(|(key, assembly)| (assembly.started_at, *key))
            .min();
        if let Some((_, key)) = oldest {
            self.streams.remove(&key);
        }
    }
}

/// Reassembly ceiling by original packet type, per
/// `BLEFragmentAssemblyBuffer.assemblyLimit(for:)`. File transfers get headroom
/// for TLV metadata and binary framing on top of the payload cap.
fn assembly_limit(original_type: u8) -> usize {
    if original_type == MessageType::FileTransfer.as_u8() {
        MAX_FRAMED_FILE_BYTES
    } else {
        MAX_REASSEMBLED_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::SIGNATURE_LEN;

    /// Incompressible filler. A run of one byte would trip the codec's
    /// compression heuristic and make fragment counts depend on the deflate
    /// ratio rather than on the planner.
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect()
    }

    fn original(payload_len: usize) -> BitchatPacket {
        BitchatPacket::new(
            MessageType::Message,
            [0xAA; SENDER_ID_LEN],
            None,
            1_700_000_000_000,
            noise(payload_len),
            7,
        )
    }

    fn fragment_of(header: &FragmentHeader, chunk: &[u8], sender: [u8; SENDER_ID_LEN]) -> BitchatPacket {
        let mut payload = Vec::new();
        payload.extend_from_slice(&header.fragment_id);
        payload.extend_from_slice(&header.index.to_be_bytes());
        payload.extend_from_slice(&header.total.to_be_bytes());
        payload.push(header.original_type);
        payload.extend_from_slice(chunk);
        BitchatPacket::new(MessageType::Fragment, sender, None, 1, payload, 7)
    }

    // --- planning ------------------------------------------------------------

    #[test]
    fn plan_slices_the_encoded_packet_and_round_trips_through_the_assembler() {
        let packet = original(2000);
        let encoded = crate::codec::encode(&packet, pads_ble_frame(packet.msg_type))
            .expect("well-formed packet encodes");

        let fragments = plan_fragments(&packet, DEFAULT_FRAGMENT_SIZE, [0x11; 8])
            .expect("a 2 KB packet fragments");
        assert_eq!(
            fragments.len(),
            encoded.len().div_ceil(DEFAULT_FRAGMENT_SIZE)
        );

        let mut assembler = FragmentAssembler::new();
        let mut reassembled = None;
        for fragment in &fragments {
            assert_eq!(fragment.msg_type, MessageType::Fragment.as_u8());
            assert_eq!(fragment.sender_id, packet.sender_id);
            assert_eq!(fragment.ttl, packet.ttl);
            assert_eq!(fragment.timestamp, packet.timestamp);
            assert_eq!(fragment.version, 1);
            assert!(fragment.signature.is_none());
            if let Some(done) = assembler.accept(fragment, 100).expect("valid fragment") {
                reassembled = Some(done);
            }
        }
        assert_eq!(reassembled.as_deref(), Some(encoded.as_slice()));
        assert!(assembler.is_empty(), "a completed stream is dropped");
    }

    #[test]
    fn every_fragment_carries_the_stream_id_index_total_and_original_type() {
        let packet = original(1200);
        let fragments =
            plan_fragments(&packet, 300, [0xAB; 8]).expect("packet fragments");
        let total = fragments.len() as u16;
        assert!(total > 1);

        for (i, fragment) in fragments.iter().enumerate() {
            let (header, chunk) = FragmentHeader::parse(fragment).expect("planner emits valid headers");
            assert_eq!(header.fragment_id, [0xAB; 8]);
            assert_eq!(header.index, i as u16);
            assert_eq!(header.total, total);
            assert_eq!(header.original_type, MessageType::Message.as_u8());
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= 300);
        }
    }

    #[test]
    fn chunk_size_is_raised_to_the_minimum() {
        let packet = original(600);
        let tiny = plan_fragments(&packet, 1, [0x01; 8]).expect("packet fragments");
        let floored = plan_fragments(&packet, MIN_FRAGMENT_CHUNK, [0x01; 8]).expect("packet fragments");
        assert_eq!(tiny.len(), floored.len());

        let zero = plan_fragments(&packet, 0, [0x01; 8]).expect("a zero request is floored, not fatal");
        assert_eq!(zero.len(), floored.len());
    }

    #[test]
    fn routed_packets_produce_v2_fragments_that_inherit_the_route() {
        let mut packet = original(1000);
        packet.version = 2;
        packet.recipient_id = Some([0x33; SENDER_ID_LEN]);
        packet.route = Some(vec![[0x44; SENDER_ID_LEN], [0x55; SENDER_ID_LEN]]);

        let fragments = plan_fragments(&packet, 200, [0x02; 8]).expect("packet fragments");
        for fragment in &fragments {
            assert_eq!(fragment.version, 2, "SOURCE_ROUTING.md §5");
            assert_eq!(fragment.route, packet.route);
            assert_eq!(fragment.recipient_id, packet.recipient_id);
        }

        // An empty route is not a route.
        packet.route = Some(Vec::new());
        let fragments = plan_fragments(&packet, 200, [0x02; 8]).expect("packet fragments");
        assert!(fragments.iter().all(|f| f.version == 1));
    }

    #[test]
    fn a_single_chunk_packet_still_produces_one_fragment() {
        let packet = original(4);
        let fragments = plan_fragments(&packet, DEFAULT_FRAGMENT_SIZE, [0x09; 8])
            .expect("small packets still fragment");
        assert_eq!(fragments.len(), 1);
        let (header, _) = FragmentHeader::parse(&fragments[0]).expect("valid header");
        assert_eq!((header.index, header.total), (0, 1));
    }

    #[test]
    fn refusing_to_emit_more_fragments_than_a_peer_would_assemble() {
        // MIN_FRAGMENT_CHUNK * MAX_FRAGMENT_COUNT = 640 000 bytes. A payload
        // that large needs v2: v1's payload length field is only 2 bytes.
        let mut packet = original(700_000);
        packet.version = 2;
        let err = plan_fragments(&packet, 1, [0x00; 8]).expect_err("too many fragments");
        assert!(matches!(
            err,
            MeshError::LengthExceeded {
                max: MAX_FRAGMENT_COUNT,
                ..
            }
        ));
    }

    #[test]
    fn noise_traffic_is_padded_and_nothing_else_is() {
        assert!(pads_ble_frame(MessageType::NoiseEncrypted.as_u8()));
        assert!(pads_ble_frame(MessageType::NoiseHandshake.as_u8()));
        for msg_type in [
            MessageType::Announce,
            MessageType::Message,
            MessageType::Leave,
            MessageType::RequestSync,
            MessageType::Fragment,
            MessageType::FileTransfer,
            MessageType::CourierEnvelope,
            MessageType::BoardPost,
            MessageType::Ping,
            MessageType::Pong,
            MessageType::NostrCarrier,
            MessageType::PrekeyBundle,
            MessageType::GroupMessage,
            MessageType::VoiceFrame,
            // The chunked-file types are not padded either, and for the same
            // reason voice frames are not: padding a 32 KiB chunk to the block
            // size buys nothing (its length is already a manifest-declared,
            // public quantity) and costs another fragment.
            MessageType::FileManifest,
            MessageType::FileChunk,
            MessageType::FileChunkRequest,
        ] {
            assert!(!pads_ble_frame(msg_type.as_u8()), "{msg_type:?}");
        }
        assert!(!pads_ble_frame(0xEE), "unknown types are not padded");
    }

    // --- header parsing ------------------------------------------------------

    #[test]
    fn header_rejects_short_payloads() {
        let mut packet = BitchatPacket::new(
            MessageType::Fragment,
            [0x01; SENDER_ID_LEN],
            None,
            1,
            vec![0u8; FRAGMENT_HEADER_LEN - 1],
            7,
        );
        assert!(matches!(
            FragmentHeader::parse(&packet),
            Err(MeshError::Truncated { needed: 1, .. })
        ));

        packet.payload = Vec::new();
        assert!(matches!(
            FragmentHeader::parse(&packet),
            Err(MeshError::Truncated {
                offset: 0,
                needed: FRAGMENT_HEADER_LEN
            })
        ));
    }

    #[test]
    fn header_accepts_an_empty_final_chunk() {
        let header = FragmentHeader {
            fragment_id: [0x07; 8],
            index: 1,
            total: 2,
            original_type: MessageType::Message.as_u8(),
        };
        let packet = fragment_of(&header, &[], [0x01; SENDER_ID_LEN]);
        let (parsed, chunk) = FragmentHeader::parse(&packet).expect("13 bytes is a valid header");
        assert_eq!(parsed, header);
        assert!(chunk.is_empty());
    }

    #[test]
    fn header_rejects_impossible_index_and_total() {
        let sender = [0x01; SENDER_ID_LEN];
        let zero_total = FragmentHeader {
            fragment_id: [0; 8],
            index: 0,
            total: 0,
            original_type: 0x02,
        };
        assert!(FragmentHeader::parse(&fragment_of(&zero_total, b"x", sender)).is_err());

        let index_past_end = FragmentHeader {
            total: 3,
            index: 3,
            ..zero_total
        };
        assert!(FragmentHeader::parse(&fragment_of(&index_past_end, b"x", sender)).is_err());

        let absurd_total = FragmentHeader {
            total: u16::MAX,
            index: 0,
            ..zero_total
        };
        assert!(FragmentHeader::parse(&fragment_of(&absurd_total, b"x", sender)).is_err());

        let at_the_ceiling = FragmentHeader {
            total: MAX_FRAGMENT_COUNT as u16,
            index: 0,
            ..zero_total
        };
        assert!(FragmentHeader::parse(&fragment_of(&at_the_ceiling, b"x", sender)).is_ok());
    }

    // --- assembly ------------------------------------------------------------

    fn stream(id: u8, sender: u8, total: u16) -> impl Fn(u16, &[u8]) -> BitchatPacket {
        move |index, chunk| {
            let header = FragmentHeader {
                fragment_id: [id; 8],
                index,
                total,
                original_type: MessageType::Message.as_u8(),
            };
            fragment_of(&header, chunk, [sender; SENDER_ID_LEN])
        }
    }

    #[test]
    fn out_of_order_fragments_reassemble_in_index_order() {
        let make = stream(0x01, 0xAA, 3);
        let mut assembler = FragmentAssembler::new();
        assert_eq!(assembler.accept(&make(2, b"ccc"), 10).expect("ok"), None);
        assert_eq!(assembler.accept(&make(0, b"aaa"), 11).expect("ok"), None);
        assert_eq!(
            assembler.accept(&make(1, b"bbb"), 12).expect("ok"),
            Some(b"aaabbbccc".to_vec())
        );
    }

    #[test]
    fn duplicate_fragments_neither_complete_nor_reset_the_stall_clock() {
        let make = stream(0x02, 0xAA, 2);
        let mut assembler = FragmentAssembler::new();
        assert_eq!(assembler.accept(&make(0, b"aa"), 100).expect("ok"), None);
        // A relayed duplicate at t=200 must not look like progress.
        assert_eq!(assembler.accept(&make(0, b"aa"), 200).expect("ok"), None);
        assert_eq!(assembler.stalled_stream_ids(205, 5), vec![[0x02; 8]]);
        assert_eq!(
            assembler.accept(&make(1, b"bb"), 205).expect("ok"),
            Some(b"aabb".to_vec())
        );
    }

    #[test]
    fn streams_from_different_senders_do_not_collide() {
        let from_a = stream(0x03, 0xAA, 2);
        let from_b = stream(0x03, 0xBB, 2);
        let mut assembler = FragmentAssembler::new();
        assert_eq!(assembler.accept(&from_a(0, b"a0"), 1).expect("ok"), None);
        assert_eq!(assembler.accept(&from_b(0, b"b0"), 1).expect("ok"), None);
        assert_eq!(assembler.len(), 2);
        assert_eq!(
            assembler.accept(&from_b(1, b"b1"), 1).expect("ok"),
            Some(b"b0b1".to_vec())
        );
        assert_eq!(
            assembler.accept(&from_a(1, b"a1"), 1).expect("ok"),
            Some(b"a0a1".to_vec())
        );
    }

    #[test]
    fn an_oversized_stream_is_dropped_whole() {
        let header = FragmentHeader {
            fragment_id: [0x04; 8],
            index: 0,
            total: 4,
            original_type: MessageType::Message.as_u8(),
        };
        let huge = vec![0u8; MAX_REASSEMBLED_BYTES / 2 + 1];
        let packet = fragment_of(&header, &huge, [0xAA; SENDER_ID_LEN]);

        let mut assembler = FragmentAssembler::new();
        assert_eq!(assembler.accept(&packet, 1).expect("first piece fits"), None);

        let second = fragment_of(
            &FragmentHeader { index: 1, ..header },
            &huge,
            [0xAA; SENDER_ID_LEN],
        );
        let err = assembler.accept(&second, 1).expect_err("second piece overflows");
        assert!(matches!(
            err,
            MeshError::LengthExceeded {
                max: MAX_REASSEMBLED_BYTES,
                ..
            }
        ));
        assert!(assembler.is_empty(), "the whole stream is evicted");
    }

    #[test]
    fn file_transfers_get_the_framed_headroom() {
        assert_eq!(
            assembly_limit(MessageType::FileTransfer.as_u8()),
            MAX_FRAMED_FILE_BYTES
        );
        assert_eq!(assembly_limit(MessageType::Message.as_u8()), MAX_REASSEMBLED_BYTES);
        // The signature width is part of that headroom; keep the two in step.
        assert_eq!(
            MAX_FRAMED_FILE_BYTES,
            MAX_REASSEMBLED_BYTES + 131_088 + 32 + SIGNATURE_LEN
        );
    }

    #[test]
    fn the_in_flight_cap_evicts_the_oldest_stream() {
        let mut assembler = FragmentAssembler::new();
        for i in 0..MAX_IN_FLIGHT_ASSEMBLIES {
            let header = FragmentHeader {
                fragment_id: (i as u64).to_be_bytes(),
                index: 0,
                total: 2,
                original_type: MessageType::Message.as_u8(),
            };
            assembler
                .accept(&fragment_of(&header, b"x", [0xAA; SENDER_ID_LEN]), i as u64)
                .expect("ok");
        }
        assert_eq!(assembler.len(), MAX_IN_FLIGHT_ASSEMBLIES);

        let newcomer = FragmentHeader {
            fragment_id: [0xFF; 8],
            index: 0,
            total: 2,
            original_type: MessageType::Message.as_u8(),
        };
        assembler
            .accept(
                &fragment_of(&newcomer, b"x", [0xAA; SENDER_ID_LEN]),
                MAX_IN_FLIGHT_ASSEMBLIES as u64,
            )
            .expect("ok");
        assert_eq!(assembler.len(), MAX_IN_FLIGHT_ASSEMBLIES);

        // Stream 0 was the oldest, so its second fragment opens a fresh stream
        // rather than completing the old one.
        let header = FragmentHeader {
            fragment_id: 0u64.to_be_bytes(),
            index: 1,
            total: 2,
            original_type: MessageType::Message.as_u8(),
        };
        assert_eq!(
            assembler
                .accept(&fragment_of(&header, b"y", [0xAA; SENDER_ID_LEN]), 999)
                .expect("ok"),
            None
        );
    }

    #[test]
    fn expiry_is_measured_from_the_first_fragment() {
        let make = stream(0x05, 0xAA, 3);
        let mut assembler = FragmentAssembler::new();
        assembler.accept(&make(0, b"a"), 1_000).expect("ok");
        // Progress at t=1020 does not extend the lifetime.
        assembler.accept(&make(1, b"b"), 1_020).expect("ok");

        assembler.evict_expired(1_030);
        assert_eq!(assembler.len(), 1, "exactly at the lifetime, still held");

        assembler.evict_expired(1_031);
        assert!(assembler.is_empty());
    }

    #[test]
    fn stall_detection_covers_only_incomplete_broadcast_streams() {
        let mut assembler = FragmentAssembler::new();

        let broadcast = stream(0x06, 0xAA, 2);
        assembler.accept(&broadcast(0, b"a"), 100).expect("ok");

        let directed_header = FragmentHeader {
            fragment_id: [0x07; 8],
            index: 0,
            total: 2,
            original_type: MessageType::Message.as_u8(),
        };
        let mut directed = fragment_of(&directed_header, b"a", [0xAA; SENDER_ID_LEN]);
        directed.recipient_id = Some([0x33; SENDER_ID_LEN]);
        assembler.accept(&directed, 100).expect("ok");

        // Not stalled yet.
        assert!(assembler.stalled_stream_ids(104, 5).is_empty());
        // Stalled: only the broadcast stream is recoverable by a resync.
        assert_eq!(assembler.stalled_stream_ids(105, 5), vec![[0x06; 8]]);

        // An explicit all-0xFF recipient counts as broadcast too.
        let explicit_header = FragmentHeader {
            fragment_id: [0x08; 8],
            ..directed_header
        };
        let mut explicit = fragment_of(&explicit_header, b"a", [0xAA; SENDER_ID_LEN]);
        explicit.recipient_id = Some(BROADCAST_RECIPIENT);
        assembler.accept(&explicit, 100).expect("ok");
        assert_eq!(
            assembler.stalled_stream_ids(105, 5),
            vec![[0x06; 8], [0x08; 8]]
        );
    }

    #[test]
    fn stalled_streams_are_ordered_oldest_first_and_capped() {
        let mut assembler = FragmentAssembler::new();
        for i in 0..(MAX_STALLED_STREAM_IDS as u64 + 5) {
            let header = FragmentHeader {
                fragment_id: i.to_be_bytes(),
                index: 0,
                total: 2,
                original_type: MessageType::Message.as_u8(),
            };
            // Later i = later last_fragment_at = less stalled.
            assembler
                .accept(&fragment_of(&header, b"x", [0xAA; SENDER_ID_LEN]), 1_000 + i)
                .expect("ok");
        }

        let stalled = assembler.stalled_stream_ids(2_000, 5);
        assert_eq!(stalled.len(), MAX_STALLED_STREAM_IDS);
        assert_eq!(stalled[0], 0u64.to_be_bytes(), "oldest stall first");
        assert_eq!(stalled[MAX_STALLED_STREAM_IDS - 1], 59u64.to_be_bytes());
    }

    #[test]
    fn completed_streams_never_appear_as_stalled() {
        let make = stream(0x0A, 0xAA, 1);
        let mut assembler = FragmentAssembler::new();
        assert_eq!(
            assembler.accept(&make(0, b"whole"), 10).expect("ok"),
            Some(b"whole".to_vec())
        );
        assert!(assembler.stalled_stream_ids(1_000, 5).is_empty());
    }

    #[test]
    fn clear_drops_everything() {
        let make = stream(0x0B, 0xAA, 2);
        let mut assembler = FragmentAssembler::new();
        assembler.accept(&make(0, b"a"), 1).expect("ok");
        assert!(!assembler.is_empty());
        assembler.clear();
        assert!(assembler.is_empty());
    }

    // MARK: what the lifetime window can actually carry

    #[test]
    fn a_one_mebibyte_payload_cannot_reassemble_within_the_lifetime_window() {
        // Not a style complaint — an arithmetic wall, and the reason P9 chunking
        // is mandatory rather than an optimisation.
        //
        // FRAGMENT_LIFETIME_SECS is 30, measured from the *first* fragment, so a
        // transfer that is 99% complete at t=31s is discarded whatever its
        // progress. Meanwhile MAX_REASSEMBLED_BYTES is 1 MiB, which at
        // DEFAULT_FRAGMENT_SIZE is well over two thousand fragments, each padded
        // out to a 512-byte block on the wire.
        //
        // Measured BLE goodput on this stack is single-digit to low-tens of kB/s.
        //
        // Note what the duty cycle does *not* do: `ScanCommand` is only ever mapped
        // to `MeshLink::set_scanning`, and on BlueZ that starts and stops
        // *discovery*. It never touches an established link, so 5s-on/10s-off gates
        // how fast a peer is found, not how fast bytes reach one already connected.
        // An earlier version of this comment claimed otherwise and inflated the
        // pessimistic figure threefold; the conclusion is unchanged because even the
        // optimistic case is past the wall.
        let fragments = MAX_REASSEMBLED_BYTES.div_ceil(DEFAULT_FRAGMENT_SIZE);
        assert!(
            fragments > 2_000,
            "1 MiB is {fragments} fragments at {DEFAULT_FRAGMENT_SIZE} bytes"
        );
        assert!(
            fragments < MAX_FRAGMENT_COUNT,
            "the count cap is not what stops this — the clock is"
        );

        // Each 469-byte fragment pads to the 512-byte block, so the wire cost is
        // larger than the payload.
        let wire_bytes = fragments * 512;

        // Both figures are plain goodput, not duty-cycled goodput.
        for goodput_bytes_per_sec in [10_000usize, 3_000] {
            let seconds = wire_bytes / goodput_bytes_per_sec;
            assert!(
                seconds as u64 > FRAGMENT_LIFETIME_SECS,
                "1 MiB needs {seconds}s at {goodput_bytes_per_sec} B/s but the \
                 assembly is evicted after {FRAGMENT_LIFETIME_SECS}s"
            );
        }

        // What the window *can* carry, at the optimistic figure. Anything above
        // this has to be chunked at a layer above the fragmenter, with resume,
        // because no single reassembly will ever finish.
        let carryable = 10_000 * FRAGMENT_LIFETIME_SECS as usize;
        assert!(
            carryable < MAX_REASSEMBLED_BYTES / 3,
            "the reachable ceiling is a fraction of the advertised one"
        );
    }
}
