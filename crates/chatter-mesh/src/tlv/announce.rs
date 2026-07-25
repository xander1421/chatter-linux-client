//! "I'm here", with a nickname, keys, and what this peer can do.
//!
//! Ported from `Protocols/Packets.swift` → `AnnouncementPacket`.
//!
//! Wire format: a flat sequence of `type:u8 ‖ length:u8 ‖ value` TLVs. One-byte
//! lengths, so no field may exceed 255 bytes — unlike the courier, board, and
//! request-sync payloads, which all use two.
//!
//! | Type | Field | Notes |
//! |---|---|---|
//! | 0x01 | nickname | UTF-8, required |
//! | 0x02 | noisePublicKey | required |
//! | 0x03 | signingPublicKey | required |
//! | 0x04 | directNeighbors | bare concatenation of 8-byte IDs, no count |
//! | 0x05 | capabilities | little-endian bitfield, minimal width |
//! | 0x06 | bridgeGeohash | UTF-8, at most 12 bytes |
//!
//! Unknown types are **skipped**, not rejected. That is the whole forward-
//! compatibility story for announcements: a newer peer can add TLV 0x07 and we
//! keep talking to them.

use crate::error::MeshError;
use crate::packet::SENDER_ID_LEN;
use crate::tlv::capabilities::PeerCapabilities;
use crate::tlv::{Reader, put_tlv_u8_len, utf8};

const TLV_NICKNAME: u8 = 0x01;
const TLV_NOISE_PUBLIC_KEY: u8 = 0x02;
const TLV_SIGNING_PUBLIC_KEY: u8 = 0x03;
const TLV_DIRECT_NEIGHBORS: u8 = 0x04;
const TLV_CAPABILITIES: u8 = 0x05;
const TLV_BRIDGE_GEOHASH: u8 = 0x06;

/// Most neighbours one announcement may advertise. Swift takes `prefix(10)`.
pub const MAX_DIRECT_NEIGHBORS: usize = 10;
/// Longest bridge rendezvous cell, in bytes.
pub const MAX_BRIDGE_GEOHASH_BYTES: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnouncementPacket {
    pub nickname: String,
    /// Noise static public key (Curve25519 key agreement). Not length-validated
    /// here: Swift accepts any value up to 255 bytes and lets the Noise layer
    /// reject a wrong-sized key, and rejecting earlier would drop announcements
    /// a real bitchat peer accepts.
    pub noise_public_key: Vec<u8>,
    /// Ed25519 public key used to sign packets.
    pub signing_public_key: Vec<u8>,
    /// Directly reachable peers, 8 bytes each. `None` when the TLV is absent;
    /// note that Swift also leaves it `None` for a present-but-malformed value.
    pub direct_neighbors: Option<Vec<[u8; SENDER_ID_LEN]>>,
    /// `None` from clients predating the capability bitfield.
    pub capabilities: Option<PeerCapabilities>,
    /// Rendezvous cell this peer bridges, when advertising [`PeerCapabilities::BRIDGE`].
    pub bridge_geohash: Option<String>,
}

impl AnnouncementPacket {
    pub fn new(
        nickname: String,
        noise_public_key: Vec<u8>,
        signing_public_key: Vec<u8>,
        direct_neighbors: Option<Vec<[u8; SENDER_ID_LEN]>>,
        capabilities: Option<PeerCapabilities>,
        bridge_geohash: Option<String>,
    ) -> Self {
        Self {
            nickname,
            noise_public_key,
            signing_public_key,
            direct_neighbors,
            capabilities,
            bridge_geohash,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut out = Vec::with_capacity(
            6 + self.nickname.len() + self.noise_public_key.len() + self.signing_public_key.len(),
        );

        put_tlv_u8_len(&mut out, TLV_NICKNAME, self.nickname.as_bytes())?;
        put_tlv_u8_len(&mut out, TLV_NOISE_PUBLIC_KEY, &self.noise_public_key)?;
        put_tlv_u8_len(&mut out, TLV_SIGNING_PUBLIC_KEY, &self.signing_public_key)?;

        // The neighbour list is a bare concatenation — there is no count field,
        // because the count is recoverable as `length / 8`.
        if let Some(neighbors) = &self.direct_neighbors {
            if !neighbors.is_empty() {
                let mut packed = Vec::with_capacity(neighbors.len().min(MAX_DIRECT_NEIGHBORS) * 8);
                for id in neighbors.iter().take(MAX_DIRECT_NEIGHBORS) {
                    packed.extend_from_slice(id);
                }
                put_tlv_u8_len(&mut out, TLV_DIRECT_NEIGHBORS, &packed)?;
            }
        }

        if let Some(capabilities) = &self.capabilities {
            put_tlv_u8_len(&mut out, TLV_CAPABILITIES, &capabilities.encode()?)?;
        }

        // Swift silently omits an empty or over-long cell rather than failing
        // the whole encode; mirrored, because an announcement without the
        // bridge hint is still a valid announcement.
        if let Some(geohash) = &self.bridge_geohash {
            let bytes = geohash.as_bytes();
            if !bytes.is_empty() && bytes.len() <= MAX_BRIDGE_GEOHASH_BYTES {
                put_tlv_u8_len(&mut out, TLV_BRIDGE_GEOHASH, bytes)?;
            }
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);

        let mut nickname: Option<String> = None;
        let mut noise_public_key: Option<Vec<u8>> = None;
        let mut signing_public_key: Option<Vec<u8>> = None;
        let mut direct_neighbors: Option<Vec<[u8; SENDER_ID_LEN]>> = None;
        let mut capabilities: Option<PeerCapabilities> = None;
        let mut bridge_geohash: Option<String> = None;

        // `while offset + 2 <= data.count`: a lone trailing byte is ignored, not
        // an error. Duplicate TLVs are last-wins, as in Swift.
        while reader.remaining() >= 2 {
            let tlv_type = reader.u8()?;
            let length = reader.u8()? as usize;
            let value = reader.take(length)?;

            match tlv_type {
                TLV_NICKNAME => nickname = Some(utf8(value, "announce.nickname")?),
                TLV_NOISE_PUBLIC_KEY => noise_public_key = Some(value.to_vec()),
                TLV_SIGNING_PUBLIC_KEY => signing_public_key = Some(value.to_vec()),
                TLV_DIRECT_NEIGHBORS => {
                    // A value that is not a whole number of IDs is ignored
                    // outright — it does not fail the announcement.
                    if length > 0 && length % SENDER_ID_LEN == 0 {
                        let mut ids = Vec::with_capacity(length / SENDER_ID_LEN);
                        for chunk in value.chunks_exact(SENDER_ID_LEN) {
                            let mut id = [0u8; SENDER_ID_LEN];
                            id.copy_from_slice(chunk);
                            ids.push(id);
                        }
                        direct_neighbors = Some(ids);
                    }
                }
                TLV_CAPABILITIES => capabilities = Some(PeerCapabilities::decode(value)?),
                TLV_BRIDGE_GEOHASH => {
                    if length <= MAX_BRIDGE_GEOHASH_BYTES {
                        bridge_geohash = Some(utf8(value, "announce.bridge_geohash")?);
                    }
                }
                // Forward compatibility: skip what we do not recognise.
                _ => {}
            }
        }

        Ok(Self {
            nickname: nickname.ok_or(MeshError::MissingField("announce.nickname"))?,
            noise_public_key: noise_public_key
                .ok_or(MeshError::MissingField("announce.noise_public_key"))?,
            signing_public_key: signing_public_key
                .ok_or(MeshError::MissingField("announce.signing_public_key"))?,
            direct_neighbors,
            capabilities,
            bridge_geohash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AnnouncementPacket {
        AnnouncementPacket::new(
            "alice".to_string(),
            vec![0xAA; 32],
            vec![0xBB; 32],
            Some(vec![[1u8; 8], [2u8; 8]]),
            Some(PeerCapabilities::from_bits(
                PeerCapabilities::PREKEYS | PeerCapabilities::BRIDGE,
            )),
            Some("u4pruyd".to_string()),
        )
    }

    #[test]
    fn round_trips_a_full_announcement() {
        let packet = sample();
        let encoded = packet.encode().unwrap();
        assert_eq!(AnnouncementPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn emits_tlvs_in_the_swift_order_with_one_byte_lengths() {
        let packet = AnnouncementPacket::new(
            "ab".to_string(),
            vec![0x01, 0x02],
            vec![0x03],
            None,
            None,
            None,
        );
        assert_eq!(
            packet.encode().unwrap(),
            vec![
                0x01, 0x02, b'a', b'b', // nickname
                0x02, 0x02, 0x01, 0x02, // noise key
                0x03, 0x01, 0x03, // signing key
            ]
        );
    }

    #[test]
    fn round_trips_the_minimal_announcement() {
        let packet =
            AnnouncementPacket::new(String::new(), Vec::new(), Vec::new(), None, None, None);
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded, vec![0x01, 0x00, 0x02, 0x00, 0x03, 0x00]);
        assert_eq!(AnnouncementPacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn neighbors_carry_no_count_field() {
        let packet = AnnouncementPacket::new(
            "n".to_string(),
            vec![],
            vec![],
            Some(vec![[0xAB; 8], [0xCD; 8]]),
            None,
            None,
        );
        let encoded = packet.encode().unwrap();
        // ... nickname(3) + noise(2) + signing(2) = 7 bytes, then the neighbours.
        assert_eq!(encoded.get(7).copied(), Some(TLV_DIRECT_NEIGHBORS));
        assert_eq!(encoded.get(8).copied(), Some(16));
        assert_eq!(encoded.get(9).copied(), Some(0xAB));
    }

    #[test]
    fn neighbors_are_capped_at_ten_on_encode() {
        let neighbors: Vec<[u8; 8]> = (0..20u8).map(|i| [i; 8]).collect();
        let packet = AnnouncementPacket::new(
            "n".to_string(),
            vec![],
            vec![],
            Some(neighbors),
            None,
            None,
        );
        let decoded = AnnouncementPacket::decode(&packet.encode().unwrap()).unwrap();
        let got = decoded.direct_neighbors.unwrap();
        assert_eq!(got.len(), MAX_DIRECT_NEIGHBORS);
        assert_eq!(got.first().copied(), Some([0u8; 8]));
        assert_eq!(got.last().copied(), Some([9u8; 8]));
    }

    #[test]
    fn empty_neighbor_list_emits_no_tlv() {
        let packet =
            AnnouncementPacket::new("n".to_string(), vec![], vec![], Some(vec![]), None, None);
        let encoded = packet.encode().unwrap();
        // The three mandatory TLVs are always emitted, even with empty keys:
        // nickname (2 + 1) + noise key (2 + 0) + signing key (2 + 0) = 7.
        assert_eq!(encoded.len(), 7);
        assert_eq!(
            AnnouncementPacket::decode(&encoded).unwrap().direct_neighbors,
            None
        );
    }

    #[test]
    fn misaligned_neighbor_value_is_ignored_not_rejected() {
        let mut bytes = vec![0x01, 0x01, b'n', 0x02, 0x00, 0x03, 0x00];
        bytes.extend_from_slice(&[TLV_DIRECT_NEIGHBORS, 0x05, 1, 2, 3, 4, 5]);
        let decoded = AnnouncementPacket::decode(&bytes).unwrap();
        assert_eq!(decoded.direct_neighbors, None);
        assert_eq!(decoded.nickname, "n");
    }

    #[test]
    fn unknown_tlv_types_are_skipped() {
        let mut bytes = vec![0x7F, 0x03, 0xDE, 0xAD, 0xBE];
        bytes.extend_from_slice(&[0x01, 0x01, b'z']);
        bytes.extend_from_slice(&[0x02, 0x01, 0xAA]);
        bytes.extend_from_slice(&[0x03, 0x01, 0xBB]);
        bytes.extend_from_slice(&[0xF0, 0x00]);
        let decoded = AnnouncementPacket::decode(&bytes).unwrap();
        assert_eq!(decoded.nickname, "z");
        assert_eq!(decoded.noise_public_key, vec![0xAA]);
        assert_eq!(decoded.signing_public_key, vec![0xBB]);
    }

    #[test]
    fn a_lone_trailing_byte_is_ignored() {
        let mut encoded = sample().encode().unwrap();
        encoded.push(0x42);
        assert_eq!(AnnouncementPacket::decode(&encoded).unwrap(), sample());
    }

    #[test]
    fn truncated_value_is_rejected() {
        let encoded = sample().encode().unwrap();
        // Every prefix must be handled totally: an error, or a decode whose
        // fields are all fully present. Never a panic, never a short key.
        for cut in 0..encoded.len() {
            if let Ok(decoded) = AnnouncementPacket::decode(&encoded[..cut]) {
                assert_eq!(decoded.noise_public_key.len(), 32);
                assert_eq!(decoded.signing_public_key.len(), 32);
            }
        }
        // A declared length that runs past the end is always an error.
        let bytes = [0x01, 0x10, b'a'];
        assert!(matches!(
            AnnouncementPacket::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn missing_required_fields_are_reported() {
        assert!(matches!(
            AnnouncementPacket::decode(&[]),
            Err(MeshError::MissingField("announce.nickname"))
        ));
        assert!(matches!(
            AnnouncementPacket::decode(&[0x01, 0x01, b'a']),
            Err(MeshError::MissingField("announce.noise_public_key"))
        ));
        assert!(matches!(
            AnnouncementPacket::decode(&[0x01, 0x01, b'a', 0x02, 0x00]),
            Err(MeshError::MissingField("announce.signing_public_key"))
        ));
    }

    #[test]
    fn invalid_utf8_nickname_is_rejected() {
        let bytes = [0x01, 0x02, 0xFF, 0xFE, 0x02, 0x00, 0x03, 0x00];
        assert!(matches!(
            AnnouncementPacket::decode(&bytes),
            Err(MeshError::InvalidUtf8 {
                field: "announce.nickname"
            })
        ));
    }

    #[test]
    fn oversize_bridge_geohash_is_ignored_on_both_sides() {
        // Encode side: silently omitted.
        let packet = AnnouncementPacket::new(
            "n".to_string(),
            vec![],
            vec![],
            None,
            None,
            Some("0123456789abc".to_string()),
        );
        // 7 = the three mandatory TLVs with a 1-byte nickname and empty keys.
        assert_eq!(packet.encode().unwrap().len(), 7);

        // Decode side: a 13-byte value is skipped.
        let mut bytes = vec![0x01, 0x01, b'n', 0x02, 0x00, 0x03, 0x00, TLV_BRIDGE_GEOHASH, 13];
        bytes.extend_from_slice(b"0123456789abc");
        assert_eq!(
            AnnouncementPacket::decode(&bytes).unwrap().bridge_geohash,
            None
        );
    }

    #[test]
    fn empty_bridge_geohash_is_omitted() {
        let packet = AnnouncementPacket::new(
            "n".to_string(),
            vec![],
            vec![],
            None,
            None,
            Some(String::new()),
        );
        // 7 = the three mandatory TLVs with a 1-byte nickname and empty keys.
        assert_eq!(packet.encode().unwrap().len(), 7);
    }

    #[test]
    fn absent_capabilities_stay_absent() {
        let packet =
            AnnouncementPacket::new("n".to_string(), vec![], vec![], None, None, None);
        let decoded = AnnouncementPacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded.capabilities, None);
    }

    #[test]
    fn empty_capability_set_is_distinguishable_from_an_absent_one() {
        let packet = AnnouncementPacket::new(
            "n".to_string(),
            vec![],
            vec![],
            None,
            Some(PeerCapabilities::default()),
            None,
        );
        let decoded = AnnouncementPacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded.capabilities, Some(PeerCapabilities::default()));
    }

    #[test]
    fn oversize_field_fails_the_encode() {
        let packet = AnnouncementPacket::new(
            "x".repeat(256),
            vec![],
            vec![],
            None,
            None,
            None,
        );
        assert!(matches!(
            packet.encode(),
            Err(MeshError::LengthExceeded {
                declared: 256,
                max: 255
            })
        ));
    }

    #[test]
    fn maximum_length_nickname_is_accepted() {
        let packet = AnnouncementPacket::new(
            "x".repeat(255),
            vec![0u8; 255],
            vec![0u8; 255],
            None,
            None,
            None,
        );
        let decoded = AnnouncementPacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded, packet);
    }
}
