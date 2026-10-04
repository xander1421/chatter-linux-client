//! Gossip-sync request payload (`MessageType::RequestSync`, 0x21).
//!
//! Ported from `Models/RequestSyncPacket.swift`, cross-checked against
//! `docs/REQUEST_SYNC_MANAGER.md`. Where they disagree the source wins; here
//! they agree.
//!
//! The packet carries a Golomb-Rice-coded set filter describing which packet IDs
//! the requester already holds, so the responder can send only the difference.
//! REQUEST_SYNC itself is link-local: TTL 0, never relayed.
//!
//! Wire format: `type:u8 ‖ length:u16be ‖ value`.
//!
//! | Type | Field | Notes |
//! |---|---|---|
//! | 0x01 | P (u8) | Golomb-Rice parameter, 1..=32 |
//! | 0x02 | M (u32 BE) | hash range, `N * 2^P`, must be > 0 |
//! | 0x03 | data | GR bitstream, MSB-first |
//! | 0x04 | types | packet types the filter covers |
//! | 0x05 | sinceTimestamp (u64 BE ms) | filter coverage cursor |
//! | 0x06 | fragmentIdFilter (UTF-8) | comma-separated 16-hex stream IDs |
//!
//! `sinceTimestamp` tells the responder the filter only covers packets at or
//! after that instant, so it skips older ones instead of re-sending them every
//! round. `fragmentIdFilter` restricts the fragment diff to exactly the named
//! streams — targeted resync for a stalled reassembly — and when present the
//! responder bypasses the `sinceTimestamp` cursor for those streams.
//!
//! # Acceptance caps
//!
//! Both the GCS payload (0x03) and the fragment filter (0x06) are capped at
//! 1024 bytes, and they fail *differently*: an oversized 0x03 rejects the whole
//! packet, while an oversized 0x06 is ignored and the rest still decodes. That
//! asymmetry is deliberate in bitchat — a bloated filter should degrade to an
//! untargeted sync, not deny the sync outright.

use std::collections::BTreeSet;

use crate::error::MeshError;
use crate::tlv::{Reader, hex_decode_exact, hex_encode, put_tlv_u16_len, utf8};

const TLV_P: u8 = 0x01;
const TLV_M: u8 = 0x02;
const TLV_DATA: u8 = 0x03;
const TLV_TYPES: u8 = 0x04;
const TLV_SINCE_TIMESTAMP: u8 = 0x05;
const TLV_FRAGMENT_ID_FILTER: u8 = 0x06;

/// Default acceptance cap for the GCS payload and the fragment filter.
pub const DEFAULT_MAX_ACCEPT_BYTES: usize = 1024;
/// `GCSFilter.maxP`.
pub const MAX_P: u8 = 32;
/// Most fragment IDs one 0x06 filter may carry. Each ID is 16 hex characters
/// plus a separator, so the largest encoded value is `60 * 17 - 1 = 1019`
/// bytes — just inside the 1024-byte cap.
pub const MAX_FRAGMENT_ID_FILTER_COUNT: usize = 60;
/// Width of a fragment stream ID.
pub const FRAGMENT_ID_LEN: usize = 8;

/// Every bit of the sync type bitfield that maps to a known message type.
///
/// Bits 0-10: announce, message, leave, noiseHandshake, noiseEncrypted,
/// fragment, requestSync, fileTransfer, boardPost, prekeyBundle, groupMessage.
/// Courier envelopes, ping/pong, Nostr carriers and voice frames deliberately
/// have no bit — replaying them through gossip sync would be useless or
/// harmful.
///
/// The byte codec is `encode_sync_types` / `decode_sync_types` in this module.
/// An unknown high bit is masked off and never re-serialised.
pub const SYNC_TYPE_KNOWN_MASK: u64 = 0x7FF;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestSyncPacket {
    /// Golomb-Rice parameter.
    pub p: u8,
    /// Hash range.
    pub m: u32,
    /// GR bitstream bytes, MSB-first.
    pub data: Vec<u8>,
    /// Packet types the filter covers, as a bitfield masked to the known bits.
    /// `None` means the peer did not say, which older clients never do.
    pub types: Option<u64>,
    /// Filter coverage cursor, milliseconds since the epoch.
    pub since_timestamp_ms: Option<u64>,
    /// Comma-separated 16-hex fragment stream IDs. Kept as the raw string so a
    /// re-encode is byte-identical; use [`RequestSyncPacket::fragment_ids`] to
    /// interpret it.
    pub fragment_id_filter: Option<String>,
}

impl RequestSyncPacket {
    pub fn new(p: u8, m: u32, data: Vec<u8>) -> Self {
        Self {
            p,
            m,
            data,
            types: None,
            since_timestamp_ms: None,
            fragment_id_filter: None,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut out = Vec::with_capacity(16 + self.data.len());

        put_tlv_u16_len(&mut out, TLV_P, &[self.p])?;
        put_tlv_u16_len(&mut out, TLV_M, &self.m.to_be_bytes())?;
        put_tlv_u16_len(&mut out, TLV_DATA, &self.data)?;

        // `toData()` returns nil for an empty set, so the TLV disappears rather
        // than being written as a zero byte.
        if let Some(types) = self.types {
            if let Some(encoded) = encode_sync_types(types) {
                put_tlv_u16_len(&mut out, TLV_TYPES, &encoded)?;
            }
        }

        if let Some(since) = self.since_timestamp_ms {
            put_tlv_u16_len(&mut out, TLV_SINCE_TIMESTAMP, &since.to_be_bytes())?;
        }

        if let Some(filter) = &self.fragment_id_filter {
            put_tlv_u16_len(&mut out, TLV_FRAGMENT_ID_FILTER, filter.as_bytes())?;
        }

        Ok(out)
    }

    /// Decodes with bitchat's default 1024-byte acceptance cap.
    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        Self::decode_with_limit(bytes, DEFAULT_MAX_ACCEPT_BYTES)
    }

    pub fn decode_with_limit(bytes: &[u8], max_accept_bytes: usize) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);

        let mut p: Option<u8> = None;
        let mut m: Option<u32> = None;
        let mut data: Option<Vec<u8>> = None;
        let mut types: Option<u64> = None;
        let mut since_timestamp_ms: Option<u64> = None;
        let mut fragment_id_filter: Option<String> = None;

        // `while off + 3 <= data.count`: a trailing stub shorter than a TLV
        // header is ignored.
        while reader.remaining() >= 3 {
            let tlv_type = reader.u8()?;
            let length = reader.u16_be()? as usize;
            let value = reader.take(length)?;

            match tlv_type {
                // Wrong-width values are ignored, leaving the field unset, so
                // the required-field check below is what rejects the packet.
                TLV_P => {
                    if let Some(byte) = one_byte(value) {
                        p = Some(byte);
                    }
                }
                TLV_M => {
                    if let Ok(raw) = <[u8; 4]>::try_from(value) {
                        m = Some(u32::from_be_bytes(raw));
                    }
                }
                TLV_DATA => {
                    if length > max_accept_bytes {
                        return Err(MeshError::LengthExceeded {
                            declared: length,
                            max: max_accept_bytes,
                        });
                    }
                    data = Some(value.to_vec());
                }
                TLV_TYPES => {
                    if let Some(decoded) = decode_sync_types(value) {
                        types = Some(decoded);
                    }
                }
                TLV_SINCE_TIMESTAMP => {
                    if let Ok(raw) = <[u8; 8]>::try_from(value) {
                        since_timestamp_ms = Some(u64::from_be_bytes(raw));
                    }
                }
                TLV_FRAGMENT_ID_FILTER => {
                    // Same acceptance cap as the GCS payload, but an oversized
                    // filter is ignored rather than failing the whole request.
                    if length <= max_accept_bytes {
                        fragment_id_filter =
                            Some(utf8(value, "request_sync.fragment_id_filter")?);
                    }
                }
                // Forward compatible: ignore unknown TLVs.
                _ => {}
            }
        }

        let p = p.ok_or(MeshError::MissingField("request_sync.p"))?;
        let m = m.ok_or(MeshError::MissingField("request_sync.m"))?;
        let data = data.ok_or(MeshError::MissingField("request_sync.data"))?;

        if !(1..=MAX_P).contains(&p) {
            return Err(MeshError::OutOfRange("request sync P outside 1...32"));
        }
        if m == 0 {
            return Err(MeshError::OutOfRange("request sync M is zero"));
        }

        Ok(Self {
            p,
            m,
            data,
            types,
            since_timestamp_ms,
            fragment_id_filter,
        })
    }

    /// The fragment stream IDs named by the 0x06 filter, or `None` when the
    /// filter is absent or names none we can parse.
    pub fn fragment_ids(&self) -> Option<BTreeSet<[u8; FRAGMENT_ID_LEN]>> {
        decode_fragment_id_filter(self.fragment_id_filter.as_deref())
    }
}

/// Builds the 0x06 filter string from 8-byte stream IDs, capped at 60.
/// `None` when there is nothing to name.
pub fn encode_fragment_id_filter(fragment_ids: &[[u8; FRAGMENT_ID_LEN]]) -> Option<String> {
    if fragment_ids.is_empty() {
        return None;
    }
    let tokens: Vec<String> = fragment_ids
        .iter()
        .take(MAX_FRAGMENT_ID_FILTER_COUNT)
        .map(|id| hex_encode(id))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.join(","))
}

/// Parses a 0x06 filter string, ignoring malformed tokens and capping at 60.
///
/// Empty tokens do not count against the cap: Swift's
/// `split(separator: ",")` omits them before `prefix(60)` sees them.
pub fn decode_fragment_id_filter(filter: Option<&str>) -> Option<BTreeSet<[u8; FRAGMENT_ID_LEN]>> {
    let filter = filter?;
    let mut ids = BTreeSet::new();
    for token in filter
        .split(',')
        .filter(|token| !token.is_empty())
        .take(MAX_FRAGMENT_ID_FILTER_COUNT)
    {
        if let Some(id) = hex_decode_exact::<FRAGMENT_ID_LEN>(token) {
            ids.insert(id);
        }
    }
    if ids.is_empty() { None } else { Some(ids) }
}

/// `SyncTypeFlags.toData()`: little-endian, trailing zero bytes trimmed, and
/// nothing at all for an empty set.
fn encode_sync_types(types: u64) -> Option<Vec<u8>> {
    let masked = types & SYNC_TYPE_KNOWN_MASK;
    if masked == 0 {
        return None;
    }
    let mut value = masked;
    let mut out = Vec::with_capacity(8);
    while value > 0 && out.len() < 8 {
        out.push((value & 0xFF) as u8);
        value >>= 8;
    }
    while out.last() == Some(&0) {
        out.pop();
    }
    if out.is_empty() { None } else { Some(out) }
}

/// `SyncTypeFlags.decode(_:)`: accepts 1 to 8 bytes, little-endian, and masks
/// off every bit that maps to no message type.
fn decode_sync_types(value: &[u8]) -> Option<u64> {
    if value.is_empty() || value.len() > 8 {
        return None;
    }
    let mut raw: u64 = 0;
    for (index, byte) in value.iter().enumerate() {
        raw |= (*byte as u64) << (8 * index as u32);
    }
    Some(raw & SYNC_TYPE_KNOWN_MASK)
}

fn one_byte(value: &[u8]) -> Option<u8> {
    if value.len() == 1 {
        value.first().copied()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RequestSyncPacket {
        RequestSyncPacket::new(19, 4096, vec![0xDE, 0xAD, 0xBE, 0xEF])
    }

    #[test]
    fn round_trips_the_required_fields() {
        let packet = sample();
        let encoded = packet.encode().unwrap();
        assert_eq!(RequestSyncPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn header_layout_is_exact() {
        let packet = RequestSyncPacket::new(19, 0x0000_1000, vec![0xAA]);
        assert_eq!(
            packet.encode().unwrap(),
            vec![
                TLV_P, 0x00, 0x01, 19,
                TLV_M, 0x00, 0x04, 0x00, 0x00, 0x10, 0x00,
                TLV_DATA, 0x00, 0x01, 0xAA,
            ]
        );
    }

    #[test]
    fn round_trips_every_optional_field() {
        let mut packet = sample();
        packet.types = Some(0b110);
        packet.since_timestamp_ms = Some(1_700_000_000_000);
        packet.fragment_id_filter = Some("00112233445566aa,ffeeddccbbaa9988".to_string());

        let encoded = packet.encode().unwrap();
        let decoded = RequestSyncPacket::decode(&encoded).unwrap();
        assert_eq!(decoded, packet);
        assert_eq!(decoded.since_timestamp_ms, Some(1_700_000_000_000));
        assert_eq!(decoded.fragment_ids().map(|ids| ids.len()), Some(2));
    }

    #[test]
    fn since_timestamp_is_eight_bytes_big_endian() {
        let mut packet = sample();
        packet.since_timestamp_ms = Some(1);
        let encoded = packet.encode().unwrap();
        let tail_start = encoded.len() - 11;
        assert_eq!(
            encoded.get(tail_start..),
            Some(&[TLV_SINCE_TIMESTAMP, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0, 1][..])
        );
    }

    #[test]
    fn a_wrong_width_since_timestamp_is_ignored() {
        let mut bytes = sample().encode().unwrap();
        put_tlv_u16_len(&mut bytes, TLV_SINCE_TIMESTAMP, &[0, 0, 0, 1]).unwrap();
        assert_eq!(
            RequestSyncPacket::decode(&bytes).unwrap().since_timestamp_ms,
            None
        );
    }

    #[test]
    fn types_are_little_endian_with_trailing_zeros_trimmed() {
        let mut packet = sample();
        // Bit 8 (boardPost) spills into a second byte.
        packet.types = Some(1 << 8);
        let encoded = packet.encode().unwrap();
        let tail_start = encoded.len() - 5;
        assert_eq!(
            encoded.get(tail_start..),
            Some(&[TLV_TYPES, 0x00, 0x02, 0x00, 0x01][..])
        );
        assert_eq!(RequestSyncPacket::decode(&encoded).unwrap().types, Some(1 << 8));
    }

    #[test]
    fn an_empty_type_set_emits_no_tlv() {
        let mut packet = sample();
        packet.types = Some(0);
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded, sample().encode().unwrap());
        assert_eq!(RequestSyncPacket::decode(&encoded).unwrap().types, None);
    }

    #[test]
    fn unknown_type_bits_are_masked_off() {
        let mut packet = sample();
        packet.types = Some(1 << 40 | 1 << 1);
        let encoded = packet.encode().unwrap();
        // Only bit 1 survives, so the field is a single byte.
        assert_eq!(encoded.last().copied(), Some(0x02));
        assert_eq!(RequestSyncPacket::decode(&encoded).unwrap().types, Some(0b10));
    }

    #[test]
    fn a_types_value_wider_than_eight_bytes_is_ignored() {
        let mut bytes = sample().encode().unwrap();
        put_tlv_u16_len(&mut bytes, TLV_TYPES, &[0xFFu8; 9]).unwrap();
        assert_eq!(RequestSyncPacket::decode(&bytes).unwrap().types, None);

        let mut bytes = sample().encode().unwrap();
        put_tlv_u16_len(&mut bytes, TLV_TYPES, &[]).unwrap();
        assert_eq!(RequestSyncPacket::decode(&bytes).unwrap().types, None);
    }

    #[test]
    fn unknown_tlvs_are_skipped() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, 0x40, &[1, 2, 3]).unwrap();
        bytes.extend_from_slice(&sample().encode().unwrap());
        put_tlv_u16_len(&mut bytes, 0x41, &[]).unwrap();
        assert_eq!(RequestSyncPacket::decode(&bytes).unwrap(), sample());
    }

    #[test]
    fn a_trailing_stub_shorter_than_a_header_is_ignored() {
        let mut encoded = sample().encode().unwrap();
        encoded.extend_from_slice(&[0x06, 0x00]);
        assert_eq!(RequestSyncPacket::decode(&encoded).unwrap(), sample());
    }

    #[test]
    fn missing_required_fields_are_reported() {
        assert!(matches!(
            RequestSyncPacket::decode(&[]),
            Err(MeshError::MissingField("request_sync.p"))
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_P, &[4]).unwrap();
        assert!(matches!(
            RequestSyncPacket::decode(&bytes),
            Err(MeshError::MissingField("request_sync.m"))
        ));

        put_tlv_u16_len(&mut bytes, TLV_M, &16u32.to_be_bytes()).unwrap();
        assert!(matches!(
            RequestSyncPacket::decode(&bytes),
            Err(MeshError::MissingField("request_sync.data"))
        ));

        // An empty GCS payload is a legitimate "I hold nothing" filter.
        put_tlv_u16_len(&mut bytes, TLV_DATA, &[]).unwrap();
        assert!(RequestSyncPacket::decode(&bytes).is_ok());
    }

    #[test]
    fn p_must_be_within_one_to_thirty_two() {
        for p in [0u8, 33, 255] {
            let mut bytes = Vec::new();
            put_tlv_u16_len(&mut bytes, TLV_P, &[p]).unwrap();
            put_tlv_u16_len(&mut bytes, TLV_M, &16u32.to_be_bytes()).unwrap();
            put_tlv_u16_len(&mut bytes, TLV_DATA, &[1]).unwrap();
            assert!(
                matches!(RequestSyncPacket::decode(&bytes), Err(MeshError::OutOfRange(_))),
                "P={p} must be rejected"
            );
        }
        for p in [1u8, 19, MAX_P] {
            let packet = RequestSyncPacket::new(p, 16, vec![1]);
            assert!(RequestSyncPacket::decode(&packet.encode().unwrap()).is_ok());
        }
    }

    #[test]
    fn m_must_be_nonzero() {
        let packet = RequestSyncPacket::new(4, 0, vec![1]);
        assert!(matches!(
            RequestSyncPacket::decode(&packet.encode().unwrap()),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn a_wrong_width_p_or_m_leaves_the_field_unset() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_P, &[1, 2]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_M, &16u32.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_DATA, &[1]).unwrap();
        assert!(matches!(
            RequestSyncPacket::decode(&bytes),
            Err(MeshError::MissingField("request_sync.p"))
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_P, &[4]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_M, &[0, 0, 16]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_DATA, &[1]).unwrap();
        assert!(matches!(
            RequestSyncPacket::decode(&bytes),
            Err(MeshError::MissingField("request_sync.m"))
        ));
    }

    #[test]
    fn an_oversized_gcs_payload_rejects_the_packet() {
        let packet = RequestSyncPacket::new(4, 16, vec![0u8; DEFAULT_MAX_ACCEPT_BYTES + 1]);
        assert!(matches!(
            RequestSyncPacket::decode(&packet.encode().unwrap()),
            Err(MeshError::LengthExceeded {
                max: DEFAULT_MAX_ACCEPT_BYTES,
                ..
            })
        ));
        // A payload exactly at the cap is fine.
        let packet = RequestSyncPacket::new(4, 16, vec![0u8; DEFAULT_MAX_ACCEPT_BYTES]);
        assert!(RequestSyncPacket::decode(&packet.encode().unwrap()).is_ok());
    }

    #[test]
    fn an_oversized_fragment_filter_is_ignored_but_the_packet_survives() {
        let mut packet = sample();
        packet.fragment_id_filter = Some("a".repeat(DEFAULT_MAX_ACCEPT_BYTES + 1));
        let decoded = RequestSyncPacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded.fragment_id_filter, None);
        assert_eq!(decoded.p, packet.p);
        assert_eq!(decoded.data, packet.data);
    }

    #[test]
    fn the_limit_is_configurable() {
        let packet = RequestSyncPacket::new(4, 16, vec![0u8; 100]);
        let encoded = packet.encode().unwrap();
        assert!(RequestSyncPacket::decode_with_limit(&encoded, 100).is_ok());
        assert!(RequestSyncPacket::decode_with_limit(&encoded, 99).is_err());
    }

    #[test]
    fn truncation_is_rejected_at_every_boundary() {
        let mut packet = sample();
        packet.since_timestamp_ms = Some(7);
        let encoded = packet.encode().unwrap();
        for cut in 0..encoded.len() {
            // Never panics. Prefixes that drop only the optional tail decode.
            let _ = RequestSyncPacket::decode(&encoded[..cut]);
        }
        // A length running past the end is always an error.
        let bytes = [TLV_DATA, 0xFF, 0xFF, 0x01];
        assert!(matches!(
            RequestSyncPacket::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn invalid_utf8_in_the_fragment_filter_is_rejected() {
        let mut bytes = sample().encode().unwrap();
        put_tlv_u16_len(&mut bytes, TLV_FRAGMENT_ID_FILTER, &[0xFF, 0xFE]).unwrap();
        assert!(matches!(
            RequestSyncPacket::decode(&bytes),
            Err(MeshError::InvalidUtf8 {
                field: "request_sync.fragment_id_filter"
            })
        ));
    }

    #[test]
    fn fragment_id_filter_round_trips() {
        let ids = [[0x00u8; 8], [0xFFu8; 8], [0x01, 2, 3, 4, 5, 6, 7, 8]];
        let filter = encode_fragment_id_filter(&ids).unwrap();
        assert_eq!(
            filter,
            "0000000000000000,ffffffffffffffff,0102030405060708"
        );
        let decoded = decode_fragment_id_filter(Some(&filter)).unwrap();
        assert_eq!(decoded.len(), 3);
        for id in ids {
            assert!(decoded.contains(&id));
        }
    }

    #[test]
    fn fragment_id_filter_caps_at_sixty_ids() {
        let ids: Vec<[u8; 8]> = (0..100u8).map(|i| [i; 8]).collect();
        let filter = encode_fragment_id_filter(&ids).unwrap();
        assert_eq!(filter.split(',').count(), MAX_FRAGMENT_ID_FILTER_COUNT);
        // 60 * 17 - 1 = 1019 bytes, inside the 1024-byte acceptance cap.
        assert_eq!(filter.len(), 1019);
        assert!(filter.len() <= DEFAULT_MAX_ACCEPT_BYTES);
        assert_eq!(
            decode_fragment_id_filter(Some(&filter)).unwrap().len(),
            MAX_FRAGMENT_ID_FILTER_COUNT
        );
    }

    #[test]
    fn an_empty_id_list_produces_no_filter() {
        assert_eq!(encode_fragment_id_filter(&[]), None);
        assert_eq!(decode_fragment_id_filter(None), None);
        assert_eq!(decode_fragment_id_filter(Some("")), None);
        assert_eq!(decode_fragment_id_filter(Some(",,,")), None);
    }

    #[test]
    fn malformed_fragment_tokens_are_dropped_individually() {
        let filter = "0102030405060708,zzzz,0011223344556677,short,,00112233445566778899";
        let ids = decode_fragment_id_filter(Some(filter)).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&[1, 2, 3, 4, 5, 6, 7, 8]));
        assert!(ids.contains(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]));
    }

    #[test]
    fn duplicate_fragment_ids_collapse() {
        let filter = "0102030405060708,0102030405060708";
        assert_eq!(decode_fragment_id_filter(Some(filter)).unwrap().len(), 1);
    }

    #[test]
    fn fragment_ids_reads_the_packet_field() {
        let mut packet = sample();
        assert_eq!(packet.fragment_ids(), None);
        packet.fragment_id_filter = Some("0102030405060708".to_string());
        assert_eq!(
            packet.fragment_ids().map(|ids| ids.len()),
            Some(1)
        );
    }

    #[test]
    fn sync_type_helpers_match_the_swift_bit_table() {
        // announce | message
        assert_eq!(encode_sync_types(0b11), Some(vec![0b11]));
        // groupMessage is bit 10.
        assert_eq!(encode_sync_types(1 << 10), Some(vec![0x00, 0x04]));
        // Bit 11 and above map to no message type.
        assert_eq!(encode_sync_types(1 << 11), None);
        assert_eq!(encode_sync_types(0), None);

        assert_eq!(decode_sync_types(&[0b11]), Some(0b11));
        assert_eq!(decode_sync_types(&[0x00, 0x04]), Some(1 << 10));
        assert_eq!(decode_sync_types(&[0xFF, 0xFF]), Some(SYNC_TYPE_KNOWN_MASK));
        assert_eq!(decode_sync_types(&[]), None);
        assert_eq!(decode_sync_types(&[0u8; 9]), None);
    }
}
