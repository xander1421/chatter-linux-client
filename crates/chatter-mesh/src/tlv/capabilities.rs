//! Feature bits a peer advertises inside its announce packet.
//!
//! Ported from `BitFoundation/PeerCapabilities.swift`.
//!
//! The wire form is a **little-endian** bitfield with trailing zero bytes
//! dropped — the one field in this crate that is not big-endian, because Swift
//! builds it by repeatedly shifting the low byte off. It is always at least one
//! byte, so an advertised-but-empty capability set stays distinguishable from an
//! absent TLV (an old client that never learned about capabilities at all).
//!
//! Decoding keeps the low 64 bits and ignores anything beyond, and unknown bits
//! are preserved verbatim so a re-encode of a newer peer's announcement is
//! byte-identical to what they sent.

use crate::error::MeshError;

/// A peer's advertised capability bits.
///
/// Deliberately not an enum-set: bit 43 meaning nothing to us today is not a
/// reason to drop it, and preserving it is what keeps re-encoding lossless.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PeerCapabilities {
    bits: u64,
}

impl PeerCapabilities {
    /// Accepts and serves one-time prekeys for forward-secret courier seals.
    pub const PREKEYS: u64 = 1 << 0;
    /// Can move bulk payloads over the Wi-Fi transport.
    pub const WIFI_BULK: u64 = 1 << 1;
    /// Bridges to the internet as a Nostr gateway.
    pub const GATEWAY: u64 = 1 << 2;
    /// Understands private groups.
    pub const GROUPS: u64 = 1 << 3;
    /// Carries bulletin-board posts.
    pub const BOARD: u64 = 1 << 4;
    /// Handles vouch attestations.
    pub const VOUCH: u64 = 1 << 5;
    /// Answers mesh diagnostic probes.
    pub const MESH_DIAGNOSTICS: u64 = 1 << 6;
    /// Bridges the local mesh channel to a geohash-cell rendezvous on Nostr.
    /// Advertised alongside a `bridgeGeohash` TLV naming the cell.
    pub const BRIDGE: u64 = 1 << 7;

    pub const fn from_bits(bits: u64) -> Self {
        Self { bits }
    }

    pub const fn bits(self) -> u64 {
        self.bits
    }

    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// True when every bit in `mask` is set. `contains(0)` is vacuously true,
    /// matching `OptionSet.contains([])`.
    pub const fn contains(self, mask: u64) -> bool {
        self.bits & mask == mask
    }

    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self {
            bits: self.bits & other.bits,
        }
    }

    pub fn insert(&mut self, mask: u64) {
        self.bits |= mask;
    }

    pub fn remove(&mut self, mask: u64) {
        self.bits &= !mask;
    }

    /// Minimal little-endian encoding, never shorter than one byte.
    ///
    /// Infallible in practice — the `Result` exists only so every TLV type in
    /// this module shares one signature.
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut value = self.bits;
        let mut out = Vec::with_capacity(8);
        // `repeat { } while value != 0` in Swift: one byte always gets written.
        loop {
            out.push((value & 0xFF) as u8);
            value >>= 8;
            if value == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// Accepts any length, including zero. Bytes past the low 64 bits are
    /// ignored for forward compatibility, exactly as `init(encoded:)` does with
    /// its `data.prefix(8)`.
    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut bits: u64 = 0;
        for (index, byte) in bytes.iter().take(8).enumerate() {
            bits |= (*byte as u64) << (8 * index as u32);
        }
        Ok(Self { bits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_still_occupies_one_byte() {
        let encoded = PeerCapabilities::default().encode().unwrap();
        assert_eq!(encoded, vec![0x00]);
        assert!(PeerCapabilities::decode(&encoded).unwrap().is_empty());
    }

    #[test]
    fn low_bits_pack_into_a_single_little_endian_byte() {
        let caps =
            PeerCapabilities::from_bits(PeerCapabilities::PREKEYS | PeerCapabilities::BRIDGE);
        assert_eq!(caps.encode().unwrap(), vec![0x81]);
    }

    #[test]
    fn high_bits_widen_the_encoding_and_trailing_zeros_are_dropped() {
        let caps = PeerCapabilities::from_bits(1 << 8);
        assert_eq!(caps.encode().unwrap(), vec![0x00, 0x01]);

        let caps = PeerCapabilities::from_bits(1 << 63);
        assert_eq!(
            caps.encode().unwrap(),
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80]
        );
    }

    #[test]
    fn round_trips_every_named_bit() {
        for mask in [
            PeerCapabilities::PREKEYS,
            PeerCapabilities::WIFI_BULK,
            PeerCapabilities::GATEWAY,
            PeerCapabilities::GROUPS,
            PeerCapabilities::BOARD,
            PeerCapabilities::VOUCH,
            PeerCapabilities::MESH_DIAGNOSTICS,
            PeerCapabilities::BRIDGE,
        ] {
            let caps = PeerCapabilities::from_bits(mask);
            let encoded = caps.encode().unwrap();
            assert_eq!(PeerCapabilities::decode(&encoded).unwrap(), caps);
            assert!(caps.contains(mask));
        }
    }

    #[test]
    fn unknown_bits_survive_a_round_trip() {
        let caps = PeerCapabilities::from_bits(1 << 43 | PeerCapabilities::BOARD);
        let encoded = caps.encode().unwrap();
        assert_eq!(PeerCapabilities::decode(&encoded).unwrap(), caps);
    }

    #[test]
    fn decode_ignores_bytes_beyond_the_low_64_bits() {
        let mut bytes = vec![0xFFu8; 8];
        bytes.push(0xAA);
        bytes.push(0xBB);
        assert_eq!(
            PeerCapabilities::decode(&bytes).unwrap(),
            PeerCapabilities::from_bits(u64::MAX)
        );
    }

    #[test]
    fn decode_accepts_an_empty_value() {
        assert_eq!(
            PeerCapabilities::decode(&[]).unwrap(),
            PeerCapabilities::default()
        );
    }

    #[test]
    fn set_algebra_behaves() {
        let a = PeerCapabilities::from_bits(PeerCapabilities::BOARD);
        let b = PeerCapabilities::from_bits(PeerCapabilities::VOUCH);
        let both = a.union(b);
        assert!(both.contains(PeerCapabilities::BOARD | PeerCapabilities::VOUCH));
        assert_eq!(both.intersection(a), a);
        assert!(!a.contains(PeerCapabilities::VOUCH));

        let mut c = PeerCapabilities::default();
        c.insert(PeerCapabilities::GATEWAY);
        assert!(c.contains(PeerCapabilities::GATEWAY));
        c.remove(PeerCapabilities::GATEWAY);
        assert!(c.is_empty());
    }
}
