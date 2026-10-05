//! Store-and-forward envelope carried on behalf of an offline third party.
//!
//! Ported from `BitFoundation/CourierEnvelope.swift`.
//!
//! A courier envelope lets a mutual favourite physically carry an encrypted
//! message to a peer who is offline. It is opaque to the courier: the only
//! routing information is a rotating tag derived from the recipient's Noise
//! static public key and the UTC day, so two envelopes for the same peer on
//! different days do not correlate for an observer who does not already know
//! that peer's public key.
//!
//! Wire format: `type:u8 ‖ length:u16be ‖ value`. **Two-byte lengths** — unlike
//! [`crate::tlv::announce`], which uses one.
//!
//! | Type | Field | Required |
//! |---|---|---|
//! | 0x01 | recipientTag (exactly 16 bytes) | yes |
//! | 0x02 | expiry (u64 BE, ms) | yes |
//! | 0x03 | ciphertext (1..=16 KiB) | yes |
//! | 0x04 | copies (u8) | omitted when 1 |
//! | 0x05 | prekeyID (u32 BE) | omitted for v1 seals |
//!
//! Both optional TLVs are omitted at their default so envelopes stay
//! byte-identical to the pre-spray and pre-prekey wire formats. A v1 decoder
//! that never learned about 0x05 skips it as unknown, keeps carrying the
//! envelope, and — if it happens to be the recipient — simply fails to open it
//! and drops it quietly.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::MeshError;
use crate::tlv::{Reader, fixed, put_tlv_u16_len};

const TLV_RECIPIENT_TAG: u8 = 0x01;
const TLV_EXPIRY: u8 = 0x02;
const TLV_CIPHERTEXT: u8 = 0x03;
const TLV_COPIES: u8 = 0x04;
const TLV_PREKEY_ID: u8 = 0x05;

/// Width of the rotating recipient tag.
pub const TAG_LEN: usize = 16;
/// Couriered messages are text-sized; media transfers are out of scope.
pub const MAX_CIPHERTEXT_BYTES: usize = 16 * 1024;
/// Matches the outbox retention policy in bitchat's `MessageRouter`.
pub const MAX_LIFETIME_SECONDS: u64 = 24 * 60 * 60;
/// Cap on the copy budget a depositor may claim, so a malicious envelope cannot
/// turn the courier network into an amplifier.
pub const MAX_COPIES: u8 = 8;
/// Seconds in a UTC day, the tag rotation period.
pub const SECONDS_PER_DAY: u64 = 86_400;

/// Domain separator for the recipient tag HMAC.
const TAG_CONTEXT: &[u8] = b"bitchat-courier-tag-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CourierEnvelope {
    /// Rotating recipient hint: `HMAC-SHA256(recipient static key, context ‖ epoch day)`,
    /// truncated to 16 bytes.
    pub recipient_tag: [u8; TAG_LEN],
    /// Milliseconds since the epoch after which the envelope must be discarded.
    pub expiry_ms: u64,
    /// Opaque one-way Noise X ciphertext; the sender's identity rides inside.
    pub ciphertext: Vec<u8>,
    /// Spray-and-wait copy budget, always clamped to `1..=8`. 1 means
    /// carry-only: deliver to the recipient, never re-spray.
    pub copies: u8,
    /// `None` is a v1 seal (Noise X to the recipient's *static* key). `Some` is
    /// a v2 seal to the recipient's one-time prekey with this ID, which is
    /// forward secret.
    pub prekey_id: Option<u32>,
}

impl CourierEnvelope {
    /// Clamps `copies` into `1..=8`, as the Swift initialiser does. Every path
    /// that produces an envelope — including [`CourierEnvelope::decode`] — goes
    /// through this clamp.
    pub fn new(
        recipient_tag: [u8; TAG_LEN],
        expiry_ms: u64,
        ciphertext: Vec<u8>,
        copies: u8,
        prekey_id: Option<u32>,
    ) -> Self {
        Self {
            recipient_tag,
            expiry_ms,
            ciphertext,
            copies: clamp_copies(copies),
            prekey_id,
        }
    }

    /// The same envelope with a different remaining copy budget.
    pub fn with_copies(&self, copies: u8) -> Self {
        Self {
            copies: clamp_copies(copies),
            ..self.clone()
        }
    }

    /// `now_ms` is milliseconds since the epoch — this crate never reads a clock
    /// it was not handed.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expiry_ms
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        if self.ciphertext.is_empty() {
            return Err(MeshError::OutOfRange(
                "courier envelope ciphertext is empty",
            ));
        }
        if self.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
            return Err(MeshError::LengthExceeded {
                declared: self.ciphertext.len(),
                max: MAX_CIPHERTEXT_BYTES,
            });
        }

        let mut out = Vec::with_capacity(9 + TAG_LEN + 8 + self.ciphertext.len());

        put_tlv_u16_len(&mut out, TLV_RECIPIENT_TAG, &self.recipient_tag)?;
        put_tlv_u16_len(&mut out, TLV_EXPIRY, &self.expiry_ms.to_be_bytes())?;
        put_tlv_u16_len(&mut out, TLV_CIPHERTEXT, &self.ciphertext)?;

        // Omitted at 1 so carry-only envelopes stay byte-identical to the
        // pre-spray wire format. Clamped here too, in case the field was set
        // directly rather than through `new`.
        let copies = clamp_copies(self.copies);
        if copies > 1 {
            put_tlv_u16_len(&mut out, TLV_COPIES, &[copies])?;
        }

        if let Some(prekey_id) = self.prekey_id {
            put_tlv_u16_len(&mut out, TLV_PREKEY_ID, &prekey_id.to_be_bytes())?;
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);

        let mut recipient_tag: Option<[u8; TAG_LEN]> = None;
        let mut expiry_ms: Option<u64> = None;
        let mut ciphertext: Option<Vec<u8>> = None;
        let mut copies: u8 = 1;
        let mut prekey_id: Option<u32> = None;

        // `while cursor < end`: a trailing byte with no length after it is an
        // error here, unlike in the announce decoder.
        while !reader.is_empty() {
            let tlv_type = reader.u8()?;
            if reader.remaining() < 2 {
                return Err(MeshError::Truncated {
                    offset: reader.position(),
                    needed: 2 - reader.remaining(),
                });
            }
            let length = reader.u16_be()? as usize;
            let value = reader.take(length)?;

            match tlv_type {
                TLV_RECIPIENT_TAG => {
                    recipient_tag = Some(fixed::<TAG_LEN>(value, "courier.recipient_tag")?);
                }
                TLV_EXPIRY => {
                    let raw = fixed::<8>(value, "courier.expiry")?;
                    expiry_ms = Some(u64::from_be_bytes(raw));
                }
                TLV_CIPHERTEXT => {
                    if length == 0 {
                        return Err(MeshError::OutOfRange(
                            "courier envelope ciphertext is empty",
                        ));
                    }
                    if length > MAX_CIPHERTEXT_BYTES {
                        return Err(MeshError::LengthExceeded {
                            declared: length,
                            max: MAX_CIPHERTEXT_BYTES,
                        });
                    }
                    ciphertext = Some(value.to_vec());
                }
                TLV_COPIES => {
                    let raw = fixed::<1>(value, "courier.copies")?;
                    copies = raw.first().copied().unwrap_or(1);
                }
                TLV_PREKEY_ID => {
                    let raw = fixed::<4>(value, "courier.prekey_id")?;
                    prekey_id = Some(u32::from_be_bytes(raw));
                }
                // Forward compatibility: a v1 decoder reaches this arm for the
                // prekey TLV and carries the envelope anyway.
                _ => {}
            }
        }

        Ok(Self::new(
            recipient_tag.ok_or(MeshError::MissingField("courier.recipient_tag"))?,
            expiry_ms.ok_or(MeshError::MissingField("courier.expiry"))?,
            ciphertext.ok_or(MeshError::MissingField("courier.ciphertext"))?,
            copies,
            prekey_id,
        ))
    }
}

/// Swift: `min(max(copies, 1), maxCopies)`.
fn clamp_copies(copies: u8) -> u8 {
    copies.clamp(1, MAX_COPIES)
}

/// UTC day number used to rotate recipient tags.
pub fn epoch_day(unix_seconds: u64) -> u32 {
    (unix_seconds / SECONDS_PER_DAY).min(u32::MAX as u64) as u32
}

/// The rotating recipient hint for one day, computable only by parties that
/// already know the recipient's Noise static public key.
///
/// `HMAC-SHA256(key: noise_static_key, message: "bitchat-courier-tag-v1" ‖ epoch_day_be)`,
/// truncated to 16 bytes.
pub fn recipient_tag(noise_static_key: &[u8], epoch_day: u32) -> [u8; TAG_LEN] {
    let mut message = Vec::with_capacity(TAG_CONTEXT.len() + 4);
    message.extend_from_slice(TAG_CONTEXT);
    message.extend_from_slice(&epoch_day.to_be_bytes());

    // HMAC accepts a key of any length, so this cannot fail; the closure keeps
    // the error path away from `unwrap`.
    let mut tag = [0u8; TAG_LEN];
    if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(noise_static_key) {
        mac.update(&message);
        let digest = mac.finalize().into_bytes();
        if let Some(prefix) = digest.get(..TAG_LEN) {
            tag.copy_from_slice(prefix);
        }
    }
    tag
}

/// The tags worth testing when checking whether an envelope is addressed to a
/// peer. Covers the adjacent days, so envelopes sealed near midnight — or under
/// modest clock skew — still match while being carried.
pub fn candidate_tags(noise_static_key: &[u8], unix_seconds: u64) -> [[u8; TAG_LEN]; 3] {
    let day = epoch_day(unix_seconds);
    let previous = if day == 0 { 0 } else { day - 1 };
    [
        recipient_tag(noise_static_key, previous),
        recipient_tag(noise_static_key, day),
        recipient_tag(noise_static_key, day.saturating_add(1)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CourierEnvelope {
        CourierEnvelope::new(
            [0xAB; TAG_LEN],
            1_700_000_000_000,
            vec![1, 2, 3, 4],
            1,
            None,
        )
    }

    #[test]
    fn round_trips_a_v1_envelope() {
        let envelope = sample();
        let encoded = envelope.encode().unwrap();
        assert_eq!(CourierEnvelope::decode(&encoded).unwrap(), envelope);
    }

    #[test]
    fn lengths_are_two_byte_big_endian() {
        let encoded = sample().encode().unwrap();
        assert_eq!(encoded.get(..3), Some(&[TLV_RECIPIENT_TAG, 0x00, 0x10][..]));
        let expiry_at = 3 + TAG_LEN;
        assert_eq!(
            encoded.get(expiry_at..expiry_at + 3),
            Some(&[TLV_EXPIRY, 0x00, 0x08][..])
        );
    }

    #[test]
    fn a_carry_only_v1_envelope_carries_exactly_three_tlvs() {
        let encoded = sample().encode().unwrap();
        // 37 bytes: three TLV headers (3 each) plus a 16-byte tag, an 8-byte
        // expiry and a 4-byte body. No copies TLV, no prekey TLV.
        assert_eq!(encoded.len(), 3 + TAG_LEN + 3 + 8 + 3 + 4);
        assert_eq!(encoded.len(), 37);

        // Re-decoding proves the omitted TLVs took their defaults.
        let decoded = CourierEnvelope::decode(&encoded).unwrap();
        assert_eq!(decoded.copies, 1);
        assert_eq!(decoded.prekey_id, None);
    }

    #[test]
    fn round_trips_a_v2_envelope_with_copies_and_prekey() {
        let envelope = CourierEnvelope::new([7; TAG_LEN], 42, vec![9; 100], 4, Some(0xDEAD_BEEF));
        let encoded = envelope.encode().unwrap();
        let decoded = CourierEnvelope::decode(&encoded).unwrap();
        assert_eq!(decoded, envelope);
        assert_eq!(decoded.copies, 4);
        assert_eq!(decoded.prekey_id, Some(0xDEAD_BEEF));
    }

    #[test]
    fn copies_are_clamped_on_construction_and_on_the_wire() {
        assert_eq!(sample().with_copies(0).copies, 1);
        assert_eq!(sample().with_copies(200).copies, MAX_COPIES);

        // A wire value above the cap is clamped, not rejected.
        let mut envelope = sample();
        envelope.copies = 8;
        let mut encoded = envelope.encode().unwrap();
        let last = encoded.len() - 1;
        if let Some(slot) = encoded.get_mut(last) {
            *slot = 200;
        }
        assert_eq!(
            CourierEnvelope::decode(&encoded).unwrap().copies,
            MAX_COPIES
        );

        // And zero becomes one.
        if let Some(slot) = encoded.get_mut(last) {
            *slot = 0;
        }
        assert_eq!(CourierEnvelope::decode(&encoded).unwrap().copies, 1);
    }

    #[test]
    fn a_v1_decoder_skips_the_prekey_tlv_as_unknown() {
        // Simulate an old decoder by feeding a v2 envelope through a decoder
        // that only knows 0x01..0x04: the unknown-type arm must skip it.
        let envelope = CourierEnvelope::new([3; TAG_LEN], 1, vec![1], 1, Some(77));
        let mut encoded = envelope.encode().unwrap();
        // Rewrite the prekey tag to an unassigned type; the rest must survive.
        let prekey_tag_index = encoded.len() - 7;
        if let Some(slot) = encoded.get_mut(prekey_tag_index) {
            assert_eq!(*slot, TLV_PREKEY_ID);
            *slot = 0x7E;
        }
        let decoded = CourierEnvelope::decode(&encoded).unwrap();
        assert_eq!(decoded.prekey_id, None);
        assert_eq!(decoded.ciphertext, vec![1]);
    }

    #[test]
    fn unknown_tlvs_at_the_front_are_skipped() {
        let mut bytes = vec![0x40, 0x00, 0x03, 0xAA, 0xBB, 0xCC];
        bytes.extend_from_slice(&sample().encode().unwrap());
        assert_eq!(CourierEnvelope::decode(&bytes).unwrap(), sample());
    }

    #[test]
    fn missing_required_fields_are_reported() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_EXPIRY, &1u64.to_be_bytes()).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::MissingField("courier.recipient_tag"))
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_RECIPIENT_TAG, &[0u8; TAG_LEN]).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::MissingField("courier.expiry"))
        ));

        put_tlv_u16_len(&mut bytes, TLV_EXPIRY, &1u64.to_be_bytes()).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::MissingField("courier.ciphertext"))
        ));
    }

    #[test]
    fn wrong_width_fixed_fields_are_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_RECIPIENT_TAG, &[0u8; 15]).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "courier.recipient_tag",
                actual: 15,
                expected: 16
            })
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_EXPIRY, &[0u8; 4]).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "courier.expiry",
                ..
            })
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_PREKEY_ID, &[0u8; 3]).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "courier.prekey_id",
                ..
            })
        ));
    }

    #[test]
    fn empty_and_oversized_ciphertexts_are_rejected() {
        let mut envelope = sample();
        envelope.ciphertext = Vec::new();
        assert!(matches!(envelope.encode(), Err(MeshError::OutOfRange(_))));

        envelope.ciphertext = vec![0u8; MAX_CIPHERTEXT_BYTES + 1];
        assert!(matches!(
            envelope.encode(),
            Err(MeshError::LengthExceeded {
                max: MAX_CIPHERTEXT_BYTES,
                ..
            })
        ));

        // On the wire: an empty ciphertext TLV fails the decode.
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_RECIPIENT_TAG, &[0u8; TAG_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_EXPIRY, &1u64.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CIPHERTEXT, &[]).unwrap();
        assert!(matches!(
            CourierEnvelope::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn maximum_ciphertext_round_trips() {
        let envelope = CourierEnvelope::new(
            [1; TAG_LEN],
            u64::MAX,
            vec![0x5A; MAX_CIPHERTEXT_BYTES],
            MAX_COPIES,
            Some(u32::MAX),
        );
        assert_eq!(
            CourierEnvelope::decode(&envelope.encode().unwrap()).unwrap(),
            envelope
        );
    }

    #[test]
    fn truncation_at_every_boundary_is_an_error() {
        let encoded = sample().encode().unwrap();
        for cut in 1..encoded.len() {
            assert!(
                CourierEnvelope::decode(&encoded[..cut]).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
        // Zero bytes is not truncation, it is an envelope with no fields.
        assert!(matches!(
            CourierEnvelope::decode(&[]),
            Err(MeshError::MissingField(_))
        ));
    }

    #[test]
    fn a_dangling_type_byte_is_truncation() {
        let mut encoded = sample().encode().unwrap();
        encoded.push(0x09);
        assert!(matches!(
            CourierEnvelope::decode(&encoded),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn expiry_is_compared_against_the_supplied_clock() {
        let envelope = CourierEnvelope::new([0; TAG_LEN], 1_000, vec![1], 1, None);
        assert!(!envelope.is_expired(999));
        assert!(envelope.is_expired(1_000));
        assert!(envelope.is_expired(1_001));
    }

    #[test]
    fn epoch_day_rotates_at_utc_midnight() {
        assert_eq!(epoch_day(0), 0);
        assert_eq!(epoch_day(SECONDS_PER_DAY - 1), 0);
        assert_eq!(epoch_day(SECONDS_PER_DAY), 1);
        assert_eq!(epoch_day(1_700_000_000), 19_675);
    }

    #[test]
    fn recipient_tag_is_deterministic_and_key_bound() {
        let key = [0x11u8; 32];
        let a = recipient_tag(&key, 19_675);
        assert_eq!(a, recipient_tag(&key, 19_675));
        assert_ne!(a, recipient_tag(&key, 19_676));
        assert_ne!(a, recipient_tag(&[0x12u8; 32], 19_675));
    }

    #[test]
    fn candidate_tags_cover_the_adjacent_days() {
        let key = [0x22u8; 32];
        let now = 1_700_000_000;
        let day = epoch_day(now);
        let tags = candidate_tags(&key, now);
        assert_eq!(tags.first().copied(), Some(recipient_tag(&key, day - 1)));
        assert_eq!(tags.get(1).copied(), Some(recipient_tag(&key, day)));
        assert_eq!(tags.get(2).copied(), Some(recipient_tag(&key, day + 1)));

        // Day zero must not underflow.
        let tags = candidate_tags(&key, 0);
        assert_eq!(tags.first().copied(), Some(recipient_tag(&key, 0)));
    }
}
