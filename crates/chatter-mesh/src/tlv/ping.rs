//! The payload shared by `Ping` (0x26) and `Pong` (0x27).
//!
//! Ported from `BitFoundation/MeshPingPayload.swift`.
//!
//! Nine fixed bytes, no TLV framing:
//!
//! ```text
//! [nonce: 8][originTTL: 1]
//! ```
//!
//! A pong echoes the nonce of the ping it answers, and `originTTL` is the TTL
//! the packet was launched with, so the receiver can derive the hop count from
//! how far the TTL fell in transit. Both directions travel unencrypted and
//! unsigned: there is nothing private in nine bytes of noise, and the
//! unguessable nonce already binds a pong to a probe we actually sent.

use crate::error::MeshError;
use crate::tlv::Reader;

/// Width of the echo nonce.
pub const NONCE_LEN: usize = 8;
/// Total encoded width.
pub const ENCODED_LEN: usize = NONCE_LEN + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MeshPingPayload {
    pub nonce: [u8; NONCE_LEN],
    pub origin_ttl: u8,
}

impl MeshPingPayload {
    pub fn new(nonce: [u8; NONCE_LEN], origin_ttl: u8) -> Self {
        Self { nonce, origin_ttl }
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut out = Vec::with_capacity(ENCODED_LEN);
        out.extend_from_slice(&self.nonce);
        out.push(self.origin_ttl);
        Ok(out)
    }

    /// Trailing bytes are accepted and ignored, so a future revision can extend
    /// the payload without breaking older clients.
    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);
        let nonce = reader.array::<NONCE_LEN>()?;
        let origin_ttl = reader.u8()?;
        Ok(Self { nonce, origin_ttl })
    }

    /// Links crossed, counting the final delivery hop: a directly connected
    /// peer is 1 hop away. `None` when the TTLs are inconsistent, which means
    /// either a broken peer or a forged reply.
    pub fn hop_count(origin_ttl: u8, received_ttl: u8) -> Option<u32> {
        if origin_ttl < received_ttl {
            return None;
        }
        Some((origin_ttl - received_ttl) as u32 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let payload = MeshPingPayload::new([1, 2, 3, 4, 5, 6, 7, 8], 7);
        let encoded = payload.encode().unwrap();
        assert_eq!(encoded, vec![1, 2, 3, 4, 5, 6, 7, 8, 7]);
        assert_eq!(MeshPingPayload::decode(&encoded).unwrap(), payload);
    }

    #[test]
    fn boundary_values_round_trip() {
        for payload in [
            MeshPingPayload::new([0; NONCE_LEN], 0),
            MeshPingPayload::new([0xFF; NONCE_LEN], 0xFF),
        ] {
            assert_eq!(
                MeshPingPayload::decode(&payload.encode().unwrap()).unwrap(),
                payload
            );
        }
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let payload = MeshPingPayload::new([9; NONCE_LEN], 3);
        let mut encoded = payload.encode().unwrap();
        encoded.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(MeshPingPayload::decode(&encoded).unwrap(), payload);
    }

    #[test]
    fn every_short_prefix_is_rejected() {
        let encoded = MeshPingPayload::new([9; NONCE_LEN], 3).encode().unwrap();
        for cut in 0..ENCODED_LEN {
            assert!(matches!(
                MeshPingPayload::decode(&encoded[..cut]),
                Err(MeshError::Truncated { .. })
            ));
        }
        assert!(MeshPingPayload::decode(&encoded).is_ok());
    }

    #[test]
    fn hop_count_counts_the_delivery_link() {
        assert_eq!(MeshPingPayload::hop_count(7, 7), Some(1));
        assert_eq!(MeshPingPayload::hop_count(7, 5), Some(3));
        assert_eq!(MeshPingPayload::hop_count(7, 0), Some(8));
        assert_eq!(MeshPingPayload::hop_count(0xFF, 0), Some(256));
    }

    #[test]
    fn hop_count_rejects_a_ttl_above_the_origin() {
        assert_eq!(MeshPingPayload::hop_count(3, 4), None);
        assert_eq!(MeshPingPayload::hop_count(0, 0xFF), None);
    }
}
