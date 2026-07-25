//! Signed bulletin-board posts and their deletion markers
//! (`MessageType::BoardPost`, 0x23).
//!
//! Ported from `Protocols/BoardPackets.swift`.
//!
//! Wire format: `type:u8 ‖ length:u16be ‖ value`, the same shape as
//! `REQUEST_SYNC`.
//!
//! | Type | Field | Applies to |
//! |---|---|---|
//! | 0x01 | kind (u8: 0x01 post, 0x02 tombstone) | both |
//! | 0x02 | postID (16 bytes) | both |
//! | 0x03 | geohash (UTF-8, empty = mesh-local, max 12) | post |
//! | 0x04 | content (UTF-8, 1..=512 bytes) | post |
//! | 0x05 | authorSigningKey (32-byte Ed25519 public key) | both |
//! | 0x06 | authorNickname (UTF-8, max 64 bytes) | post |
//! | 0x07 | createdAt (u64 BE ms) | post |
//! | 0x08 | expiresAt (u64 BE ms, ≤ 7 days after createdAt) | post |
//! | 0x09 | flags (u8, bit 0 urgent) | post |
//! | 0x0A | signature (64-byte Ed25519) | both |
//! | 0x0B | deletedAt (u64 BE ms) | tombstone |
//!
//! Unknown types are skipped.
//!
//! # Signatures
//!
//! [`BoardWire::decode`] is a **structural** decode only. The caller must verify
//! the Ed25519 signature over [`BoardPostPacket::signing_bytes`] or
//! [`BoardTombstonePacket::signing_bytes`] before ingesting a post — this crate
//! has no signature dependency, by design, so the bytes are produced here and
//! verified by the crypto layer.

use crate::error::MeshError;
use crate::tlv::{Reader, fixed, put_tlv_u16_len, utf8};

const TLV_KIND: u8 = 0x01;
const TLV_POST_ID: u8 = 0x02;
const TLV_GEOHASH: u8 = 0x03;
const TLV_CONTENT: u8 = 0x04;
const TLV_AUTHOR_SIGNING_KEY: u8 = 0x05;
const TLV_AUTHOR_NICKNAME: u8 = 0x06;
const TLV_CREATED_AT: u8 = 0x07;
const TLV_EXPIRES_AT: u8 = 0x08;
const TLV_FLAGS: u8 = 0x09;
const TLV_SIGNATURE: u8 = 0x0A;
const TLV_DELETED_AT: u8 = 0x0B;

const KIND_POST: u8 = 0x01;
const KIND_TOMBSTONE: u8 = 0x02;

pub const POST_ID_LEN: usize = 16;
pub const SIGNING_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;
pub const CONTENT_MAX_BYTES: usize = 512;
pub const NICKNAME_MAX_BYTES: usize = 64;
pub const GEOHASH_MAX_LEN: usize = 12;
/// Posts may live at most 7 days past their creation timestamp.
pub const MAX_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1000;

pub const POST_SIGNING_CONTEXT: &str = "bitchat-board-v1";
pub const TOMBSTONE_SIGNING_CONTEXT: &str = "bitchat-board-del-v1";

/// Geohash base32: no `a`, `i`, `l`, or `o`.
const GEOHASH_ALPHABET: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Bit 0 of the flags byte.
pub const URGENT_FLAG: u8 = 0x01;

/// A signed, persistent bulletin-board notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardPostPacket {
    pub post_id: [u8; POST_ID_LEN],
    /// Empty scopes the post to the mesh-local board.
    pub geohash: String,
    pub content: String,
    pub author_signing_key: [u8; SIGNING_KEY_LEN],
    pub author_nickname: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub flags: u8,
    pub signature: [u8; SIGNATURE_LEN],
}

impl BoardPostPacket {
    pub fn is_urgent(&self) -> bool {
        self.flags & URGENT_FLAG != 0
    }

    /// The exact bytes the Ed25519 signature covers.
    ///
    /// Variable-length fields are length-prefixed so no two field combinations
    /// can collide — the reason a signature over the raw concatenation would be
    /// forgeable by shifting bytes between adjacent fields.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128 + self.content.len());
        append_context(&mut out, POST_SIGNING_CONTEXT);
        out.extend_from_slice(&self.post_id);
        append_length_prefixed(&mut out, self.geohash.as_bytes());
        append_length_prefixed(&mut out, self.content.as_bytes());
        out.extend_from_slice(&self.author_signing_key);
        append_length_prefixed(&mut out, self.author_nickname.as_bytes());
        out.extend_from_slice(&self.created_at_ms.to_be_bytes());
        out.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        out.push(self.flags);
        out
    }

    /// The constraints [`BoardWire::decode`] enforces, checked before encoding.
    ///
    /// Swift's encoder skips these — it would rather trap on an oversized field
    /// than reject one. Checking here means we cannot emit a post that every
    /// bitchat client on the mesh would drop.
    fn validate(&self) -> Result<(), MeshError> {
        if self.content.is_empty() {
            return Err(MeshError::OutOfRange("board post content is empty"));
        }
        if self.content.len() > CONTENT_MAX_BYTES {
            return Err(MeshError::LengthExceeded {
                declared: self.content.len(),
                max: CONTENT_MAX_BYTES,
            });
        }
        if self.author_nickname.len() > NICKNAME_MAX_BYTES {
            return Err(MeshError::LengthExceeded {
                declared: self.author_nickname.len(),
                max: NICKNAME_MAX_BYTES,
            });
        }
        if self.geohash.len() > GEOHASH_MAX_LEN {
            return Err(MeshError::LengthExceeded {
                declared: self.geohash.len(),
                max: GEOHASH_MAX_LEN,
            });
        }
        if !is_valid_geohash_field(&self.geohash) {
            return Err(MeshError::OutOfRange(
                "board post geohash is not base32 geohash",
            ));
        }
        if self.expires_at_ms <= self.created_at_ms {
            return Err(MeshError::OutOfRange(
                "board post expires at or before its creation",
            ));
        }
        if self.expires_at_ms - self.created_at_ms > MAX_LIFETIME_MS {
            return Err(MeshError::OutOfRange("board post lifetime exceeds 7 days"));
        }
        Ok(())
    }
}

/// A signed deletion marker. Only the author's key can produce one; receivers
/// keep it until the post's original expiry, so the delete outruns the post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardTombstonePacket {
    pub post_id: [u8; POST_ID_LEN],
    pub author_signing_key: [u8; SIGNING_KEY_LEN],
    pub deleted_at_ms: u64,
    pub signature: [u8; SIGNATURE_LEN],
}

impl BoardTombstonePacket {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        append_context(&mut out, TOMBSTONE_SIGNING_CONTEXT);
        out.extend_from_slice(&self.post_id);
        out.extend_from_slice(&self.deleted_at_ms.to_be_bytes());
        out
    }
}

/// A decoded board payload: either a live post or a tombstone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardWire {
    Post(BoardPostPacket),
    Tombstone(BoardTombstonePacket),
}

impl BoardWire {
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut out = Vec::new();
        match self {
            Self::Post(post) => {
                post.validate()?;
                put_tlv_u16_len(&mut out, TLV_KIND, &[KIND_POST])?;
                put_tlv_u16_len(&mut out, TLV_POST_ID, &post.post_id)?;
                put_tlv_u16_len(&mut out, TLV_GEOHASH, post.geohash.as_bytes())?;
                put_tlv_u16_len(&mut out, TLV_CONTENT, post.content.as_bytes())?;
                put_tlv_u16_len(&mut out, TLV_AUTHOR_SIGNING_KEY, &post.author_signing_key)?;
                put_tlv_u16_len(&mut out, TLV_AUTHOR_NICKNAME, post.author_nickname.as_bytes())?;
                put_tlv_u16_len(&mut out, TLV_CREATED_AT, &post.created_at_ms.to_be_bytes())?;
                put_tlv_u16_len(&mut out, TLV_EXPIRES_AT, &post.expires_at_ms.to_be_bytes())?;
                put_tlv_u16_len(&mut out, TLV_FLAGS, &[post.flags])?;
                put_tlv_u16_len(&mut out, TLV_SIGNATURE, &post.signature)?;
            }
            Self::Tombstone(tombstone) => {
                put_tlv_u16_len(&mut out, TLV_KIND, &[KIND_TOMBSTONE])?;
                put_tlv_u16_len(&mut out, TLV_POST_ID, &tombstone.post_id)?;
                put_tlv_u16_len(
                    &mut out,
                    TLV_AUTHOR_SIGNING_KEY,
                    &tombstone.author_signing_key,
                )?;
                put_tlv_u16_len(&mut out, TLV_DELETED_AT, &tombstone.deleted_at_ms.to_be_bytes())?;
                put_tlv_u16_len(&mut out, TLV_SIGNATURE, &tombstone.signature)?;
            }
        }
        Ok(out)
    }

    /// Structural decode. The signature is **not** checked here.
    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);

        let mut kind: Option<u8> = None;
        let mut post_id: Option<[u8; POST_ID_LEN]> = None;
        let mut geohash: Option<String> = None;
        let mut content: Option<String> = None;
        let mut content_bytes = 0usize;
        let mut author_signing_key: Option<[u8; SIGNING_KEY_LEN]> = None;
        let mut author_nickname: Option<String> = None;
        let mut created_at_ms: Option<u64> = None;
        let mut expires_at_ms: Option<u64> = None;
        let mut flags: Option<u8> = None;
        let mut signature: Option<[u8; SIGNATURE_LEN]> = None;
        let mut deleted_at_ms: Option<u64> = None;

        // `while off + 3 <= endIndex`: a trailing stub shorter than a TLV header
        // is ignored rather than treated as truncation.
        while reader.remaining() >= 3 {
            let tlv_type = reader.u8()?;
            let length = reader.u16_be()? as usize;
            let value = reader.take(length)?;

            match tlv_type {
                TLV_KIND => {
                    let raw = fixed::<1>(value, "board.kind")?;
                    // An unrecognised kind byte leaves `kind` unset, which the
                    // final match rejects — matching Swift's `BoardWireKind(rawValue:)`.
                    kind = raw.first().copied();
                }
                TLV_POST_ID => post_id = Some(fixed::<POST_ID_LEN>(value, "board.post_id")?),
                TLV_GEOHASH => {
                    if length > GEOHASH_MAX_LEN {
                        return Err(MeshError::LengthExceeded {
                            declared: length,
                            max: GEOHASH_MAX_LEN,
                        });
                    }
                    geohash = Some(utf8(value, "board.geohash")?);
                }
                TLV_CONTENT => {
                    if length > CONTENT_MAX_BYTES {
                        return Err(MeshError::LengthExceeded {
                            declared: length,
                            max: CONTENT_MAX_BYTES,
                        });
                    }
                    content_bytes = length;
                    content = Some(utf8(value, "board.content")?);
                }
                TLV_AUTHOR_SIGNING_KEY => {
                    author_signing_key =
                        Some(fixed::<SIGNING_KEY_LEN>(value, "board.author_signing_key")?);
                }
                TLV_AUTHOR_NICKNAME => {
                    if length > NICKNAME_MAX_BYTES {
                        return Err(MeshError::LengthExceeded {
                            declared: length,
                            max: NICKNAME_MAX_BYTES,
                        });
                    }
                    author_nickname = Some(utf8(value, "board.author_nickname")?);
                }
                // A wrong-width timestamp clears the field rather than failing
                // the packet, exactly as `uint64(from:)` returning nil does.
                TLV_CREATED_AT => created_at_ms = read_u64(value),
                TLV_EXPIRES_AT => expires_at_ms = read_u64(value),
                TLV_DELETED_AT => deleted_at_ms = read_u64(value),
                TLV_FLAGS => {
                    let raw = fixed::<1>(value, "board.flags")?;
                    flags = raw.first().copied();
                }
                TLV_SIGNATURE => {
                    signature = Some(fixed::<SIGNATURE_LEN>(value, "board.signature")?);
                }
                // Forward compatible: ignore unknown TLVs.
                _ => {}
            }
        }

        let post_id = post_id.ok_or(MeshError::MissingField("board.post_id"))?;
        let author_signing_key =
            author_signing_key.ok_or(MeshError::MissingField("board.author_signing_key"))?;
        let signature = signature.ok_or(MeshError::MissingField("board.signature"))?;

        match kind {
            Some(KIND_POST) => {
                let geohash = geohash.ok_or(MeshError::MissingField("board.geohash"))?;
                let content = content.ok_or(MeshError::MissingField("board.content"))?;
                let author_nickname =
                    author_nickname.ok_or(MeshError::MissingField("board.author_nickname"))?;
                let created_at_ms =
                    created_at_ms.ok_or(MeshError::MissingField("board.created_at"))?;
                let expires_at_ms =
                    expires_at_ms.ok_or(MeshError::MissingField("board.expires_at"))?;
                let flags = flags.ok_or(MeshError::MissingField("board.flags"))?;

                if content_bytes < 1 {
                    return Err(MeshError::OutOfRange("board post content is empty"));
                }
                if !is_valid_geohash_field(&geohash) {
                    return Err(MeshError::OutOfRange(
                        "board post geohash is not base32 geohash",
                    ));
                }
                if expires_at_ms <= created_at_ms {
                    return Err(MeshError::OutOfRange(
                        "board post expires at or before its creation",
                    ));
                }
                if expires_at_ms - created_at_ms > MAX_LIFETIME_MS {
                    return Err(MeshError::OutOfRange("board post lifetime exceeds 7 days"));
                }

                Ok(Self::Post(BoardPostPacket {
                    post_id,
                    geohash,
                    content,
                    author_signing_key,
                    author_nickname,
                    created_at_ms,
                    expires_at_ms,
                    flags,
                    signature,
                }))
            }
            Some(KIND_TOMBSTONE) => {
                let deleted_at_ms =
                    deleted_at_ms.ok_or(MeshError::MissingField("board.deleted_at"))?;
                Ok(Self::Tombstone(BoardTombstonePacket {
                    post_id,
                    author_signing_key,
                    deleted_at_ms,
                    signature,
                }))
            }
            _ => Err(MeshError::MissingField("board.kind")),
        }
    }

    /// The bytes whose signature must verify before this payload is ingested.
    pub fn signing_bytes(&self) -> Vec<u8> {
        match self {
            Self::Post(post) => post.signing_bytes(),
            Self::Tombstone(tombstone) => tombstone.signing_bytes(),
        }
    }

    /// The signature carried by this payload.
    pub fn signature(&self) -> [u8; SIGNATURE_LEN] {
        match self {
            Self::Post(post) => post.signature,
            Self::Tombstone(tombstone) => tombstone.signature,
        }
    }

    /// The author key that must verify the signature.
    pub fn author_signing_key(&self) -> [u8; SIGNING_KEY_LEN] {
        match self {
            Self::Post(post) => post.author_signing_key,
            Self::Tombstone(tombstone) => tombstone.author_signing_key,
        }
    }

    /// Cheap TLV peek for the relay hot path: is this payload an urgent post?
    /// Avoids a full decode just to make a forwarding decision. Never fails —
    /// a malformed payload simply is not urgent.
    pub fn urgent_flag(bytes: &[u8]) -> bool {
        let mut reader = Reader::new(bytes);
        while reader.remaining() >= 3 {
            let Ok(tlv_type) = reader.u8() else {
                return false;
            };
            let Ok(length) = reader.u16_be() else {
                return false;
            };
            let Ok(value) = reader.take(length as usize) else {
                return false;
            };
            if tlv_type == TLV_FLAGS && length == 1 {
                return value.first().is_some_and(|byte| byte & URGENT_FLAG != 0);
            }
        }
        false
    }
}

/// `[len: u8][context bytes]`, truncated at 255.
fn append_context(out: &mut Vec<u8>, context: &str) {
    let bytes = context.as_bytes();
    let len = bytes.len().min(u8::MAX as usize);
    out.push(len as u8);
    out.extend_from_slice(bytes.get(..len).unwrap_or(bytes));
}

/// `[len: u16 BE][bytes]`, truncated at 65535.
fn append_length_prefixed(out: &mut Vec<u8>, value: &[u8]) {
    let len = value.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(value.get(..len).unwrap_or(value));
}

fn read_u64(value: &[u8]) -> Option<u64> {
    let raw: [u8; 8] = value.try_into().ok()?;
    Some(u64::from_be_bytes(raw))
}

/// Empty means the mesh-local board; otherwise 1-12 geohash base32 characters.
fn is_valid_geohash_field(geohash: &str) -> bool {
    geohash
        .bytes()
        .all(|byte| GEOHASH_ALPHABET.contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    fn post() -> BoardPostPacket {
        BoardPostPacket {
            post_id: [0x11; POST_ID_LEN],
            geohash: "u4pruyd".to_string(),
            content: "meet at the fountain".to_string(),
            author_signing_key: [0x22; SIGNING_KEY_LEN],
            author_nickname: "alice".to_string(),
            created_at_ms: 1_700_000_000_000,
            expires_at_ms: 1_700_000_000_000 + DAY_MS,
            flags: 0,
            signature: [0x33; SIGNATURE_LEN],
        }
    }

    fn tombstone() -> BoardTombstonePacket {
        BoardTombstonePacket {
            post_id: [0x44; POST_ID_LEN],
            author_signing_key: [0x55; SIGNING_KEY_LEN],
            deleted_at_ms: 1_700_000_100_000,
            signature: [0x66; SIGNATURE_LEN],
        }
    }

    #[test]
    fn post_round_trips() {
        let wire = BoardWire::Post(post());
        let encoded = wire.encode().unwrap();
        assert_eq!(BoardWire::decode(&encoded).unwrap(), wire);
    }

    #[test]
    fn tombstone_round_trips() {
        let wire = BoardWire::Tombstone(tombstone());
        let encoded = wire.encode().unwrap();
        assert_eq!(BoardWire::decode(&encoded).unwrap(), wire);
    }

    #[test]
    fn lengths_are_two_byte_big_endian_and_kind_comes_first() {
        let encoded = BoardWire::Post(post()).encode().unwrap();
        assert_eq!(
            encoded.get(..4),
            Some(&[TLV_KIND, 0x00, 0x01, KIND_POST][..])
        );
        assert_eq!(
            encoded.get(4..7),
            Some(&[TLV_POST_ID, 0x00, 0x10][..]),
            "post ID is 16 bytes"
        );
    }

    #[test]
    fn tombstone_omits_post_only_fields() {
        let encoded = BoardWire::Tombstone(tombstone()).encode().unwrap();
        // kind + postID + key + deletedAt + signature, headers included.
        assert_eq!(encoded.len(), (3 + 1) + (3 + 16) + (3 + 32) + (3 + 8) + (3 + 64));
    }

    #[test]
    fn an_empty_geohash_is_the_mesh_local_board() {
        let mut post = post();
        post.geohash = String::new();
        let wire = BoardWire::Post(post);
        assert_eq!(BoardWire::decode(&wire.encode().unwrap()).unwrap(), wire);
    }

    #[test]
    fn boundary_values_round_trip() {
        let mut post = post();
        post.content = "c".repeat(CONTENT_MAX_BYTES);
        post.author_nickname = "n".repeat(NICKNAME_MAX_BYTES);
        post.geohash = "0123456789bc".to_string();
        post.expires_at_ms = post.created_at_ms + MAX_LIFETIME_MS;
        post.flags = URGENT_FLAG;
        let wire = BoardWire::Post(post);
        assert_eq!(BoardWire::decode(&wire.encode().unwrap()).unwrap(), wire);
    }

    #[test]
    fn unknown_tlvs_are_skipped() {
        let mut encoded = BoardWire::Post(post()).encode().unwrap();
        put_tlv_u16_len(&mut encoded, 0x7F, &[1, 2, 3]).unwrap();
        assert_eq!(
            BoardWire::decode(&encoded).unwrap(),
            BoardWire::Post(post())
        );
    }

    #[test]
    fn a_trailing_stub_shorter_than_a_header_is_ignored() {
        let mut encoded = BoardWire::Post(post()).encode().unwrap();
        encoded.extend_from_slice(&[0x7F, 0x00]);
        assert_eq!(
            BoardWire::decode(&encoded).unwrap(),
            BoardWire::Post(post())
        );
    }

    #[test]
    fn missing_kind_is_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_POST_ID, &[0u8; POST_ID_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_AUTHOR_SIGNING_KEY, &[0u8; SIGNING_KEY_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_SIGNATURE, &[0u8; SIGNATURE_LEN]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::MissingField("board.kind"))
        ));
    }

    #[test]
    fn an_unknown_kind_value_is_rejected() {
        let mut encoded = BoardWire::Post(post()).encode().unwrap();
        if let Some(slot) = encoded.get_mut(3) {
            *slot = 0x09;
        }
        assert!(matches!(
            BoardWire::decode(&encoded),
            Err(MeshError::MissingField("board.kind"))
        ));
    }

    #[test]
    fn shared_required_fields_are_reported() {
        for (missing, expected) in [
            (TLV_POST_ID, "board.post_id"),
            (TLV_AUTHOR_SIGNING_KEY, "board.author_signing_key"),
            (TLV_SIGNATURE, "board.signature"),
        ] {
            let mut bytes = Vec::new();
            put_tlv_u16_len(&mut bytes, TLV_KIND, &[KIND_TOMBSTONE]).unwrap();
            if missing != TLV_POST_ID {
                put_tlv_u16_len(&mut bytes, TLV_POST_ID, &[0u8; POST_ID_LEN]).unwrap();
            }
            if missing != TLV_AUTHOR_SIGNING_KEY {
                put_tlv_u16_len(&mut bytes, TLV_AUTHOR_SIGNING_KEY, &[0u8; SIGNING_KEY_LEN])
                    .unwrap();
            }
            if missing != TLV_SIGNATURE {
                put_tlv_u16_len(&mut bytes, TLV_SIGNATURE, &[0u8; SIGNATURE_LEN]).unwrap();
            }
            put_tlv_u16_len(&mut bytes, TLV_DELETED_AT, &1u64.to_be_bytes()).unwrap();
            match BoardWire::decode(&bytes) {
                Err(MeshError::MissingField(field)) => assert_eq!(field, expected),
                other => panic!("expected {expected} to be reported, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_tombstone_without_a_deleted_at_is_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_KIND, &[KIND_TOMBSTONE]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_POST_ID, &[0u8; POST_ID_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_AUTHOR_SIGNING_KEY, &[0u8; SIGNING_KEY_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_SIGNATURE, &[0u8; SIGNATURE_LEN]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::MissingField("board.deleted_at"))
        ));
    }

    #[test]
    fn a_wrong_width_timestamp_clears_the_field() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_KIND, &[KIND_TOMBSTONE]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_POST_ID, &[0u8; POST_ID_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_AUTHOR_SIGNING_KEY, &[0u8; SIGNING_KEY_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_SIGNATURE, &[0u8; SIGNATURE_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_DELETED_AT, &1u64.to_be_bytes()).unwrap();
        // A second, malformed deletedAt overwrites the good one with nothing.
        put_tlv_u16_len(&mut bytes, TLV_DELETED_AT, &[0, 0, 0, 1]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::MissingField("board.deleted_at"))
        ));
    }

    #[test]
    fn wrong_width_fixed_fields_are_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_KIND, &[KIND_POST]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_POST_ID, &[0u8; 15]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "board.post_id",
                actual: 15,
                expected: 16
            })
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_SIGNATURE, &[0u8; 63]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "board.signature",
                ..
            })
        ));

        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_KIND, &[KIND_POST, KIND_POST]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::BadFieldLength {
                field: "board.kind",
                ..
            })
        ));
    }

    #[test]
    fn oversized_variable_fields_are_rejected_on_both_sides() {
        let mut too_long = post();
        too_long.content = "c".repeat(CONTENT_MAX_BYTES + 1);
        assert!(matches!(
            BoardWire::Post(too_long).encode(),
            Err(MeshError::LengthExceeded {
                max: CONTENT_MAX_BYTES,
                ..
            })
        ));

        let mut too_long = post();
        too_long.author_nickname = "n".repeat(NICKNAME_MAX_BYTES + 1);
        assert!(matches!(
            BoardWire::Post(too_long).encode(),
            Err(MeshError::LengthExceeded {
                max: NICKNAME_MAX_BYTES,
                ..
            })
        ));

        let mut too_long = post();
        too_long.geohash = "0".repeat(GEOHASH_MAX_LEN + 1);
        assert!(matches!(
            BoardWire::Post(too_long).encode(),
            Err(MeshError::LengthExceeded {
                max: GEOHASH_MAX_LEN,
                ..
            })
        ));

        // And on the wire.
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_CONTENT, &vec![b'c'; CONTENT_MAX_BYTES + 1]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::LengthExceeded {
                max: CONTENT_MAX_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn empty_content_is_rejected() {
        let mut empty = post();
        empty.content = String::new();
        assert!(matches!(
            BoardWire::Post(empty).encode(),
            Err(MeshError::OutOfRange(_))
        ));

        // On the wire: a present-but-empty content TLV fails the same check.
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_KIND, &[KIND_POST]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_POST_ID, &[0u8; POST_ID_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_GEOHASH, b"").unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CONTENT, b"").unwrap();
        put_tlv_u16_len(&mut bytes, TLV_AUTHOR_SIGNING_KEY, &[0u8; SIGNING_KEY_LEN]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_AUTHOR_NICKNAME, b"n").unwrap();
        put_tlv_u16_len(&mut bytes, TLV_CREATED_AT, &1u64.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_EXPIRES_AT, &2u64.to_be_bytes()).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_FLAGS, &[0]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_SIGNATURE, &[0u8; SIGNATURE_LEN]).unwrap();
        assert!(matches!(
            BoardWire::decode(&bytes),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn an_invalid_geohash_alphabet_is_rejected() {
        let mut bad = post();
        bad.geohash = "ailo".to_string(); // the four excluded characters
        assert!(matches!(
            BoardWire::Post(bad).encode(),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn expiry_must_follow_creation_and_stay_within_seven_days() {
        let mut same = post();
        same.expires_at_ms = same.created_at_ms;
        assert!(matches!(
            BoardWire::Post(same).encode(),
            Err(MeshError::OutOfRange(_))
        ));

        let mut long = post();
        long.expires_at_ms = long.created_at_ms + MAX_LIFETIME_MS + 1;
        assert!(matches!(
            BoardWire::Post(long).encode(),
            Err(MeshError::OutOfRange(_))
        ));
    }

    #[test]
    fn truncation_is_rejected_at_every_boundary() {
        let encoded = BoardWire::Post(post()).encode().unwrap();
        for cut in 0..encoded.len() {
            assert!(
                BoardWire::decode(&encoded[..cut]).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
    }

    #[test]
    fn urgent_peek_matches_a_full_decode() {
        let mut urgent = post();
        urgent.flags = URGENT_FLAG;
        let encoded = BoardWire::Post(urgent).encode().unwrap();
        assert!(BoardWire::urgent_flag(&encoded));

        let calm = BoardWire::Post(post()).encode().unwrap();
        assert!(!BoardWire::urgent_flag(&calm));

        // A tombstone has no flags TLV.
        let tombstone = BoardWire::Tombstone(tombstone()).encode().unwrap();
        assert!(!BoardWire::urgent_flag(&tombstone));
    }

    #[test]
    fn urgent_peek_never_panics_on_garbage() {
        assert!(!BoardWire::urgent_flag(&[]));
        assert!(!BoardWire::urgent_flag(&[TLV_FLAGS, 0x00]));
        assert!(!BoardWire::urgent_flag(&[TLV_FLAGS, 0xFF, 0xFF, 0x01]));
        assert!(!BoardWire::urgent_flag(&[TLV_FLAGS, 0x00, 0x02, 0x01, 0x00]));
        assert!(BoardWire::urgent_flag(&[TLV_FLAGS, 0x00, 0x01, 0x01]));
    }

    #[test]
    fn post_signing_bytes_are_length_prefixed_and_stable() {
        let bytes = post().signing_bytes();
        let context = POST_SIGNING_CONTEXT.as_bytes();
        assert_eq!(bytes.first().copied(), Some(context.len() as u8));
        assert_eq!(bytes.get(1..1 + context.len()), Some(context));

        let expected_len = 1
            + context.len()
            + POST_ID_LEN
            + 2
            + "u4pruyd".len()
            + 2
            + "meet at the fountain".len()
            + SIGNING_KEY_LEN
            + 2
            + "alice".len()
            + 8
            + 8
            + 1;
        assert_eq!(bytes.len(), expected_len);
    }

    #[test]
    fn signing_bytes_separate_adjacent_fields() {
        // Moving a character between geohash and content must change the
        // preimage — that is what the length prefixes are for.
        let mut a = post();
        a.geohash = "bc".to_string();
        a.content = "de".to_string();
        let mut b = post();
        b.geohash = "b".to_string();
        b.content = "cde".to_string();
        assert_ne!(a.signing_bytes(), b.signing_bytes());
    }

    #[test]
    fn tombstone_signing_bytes_use_their_own_context() {
        let bytes = tombstone().signing_bytes();
        let context = TOMBSTONE_SIGNING_CONTEXT.as_bytes();
        assert_eq!(bytes.first().copied(), Some(context.len() as u8));
        assert_eq!(bytes.get(1..1 + context.len()), Some(context));
        assert_eq!(bytes.len(), 1 + context.len() + POST_ID_LEN + 8);
        assert_ne!(bytes, post().signing_bytes());
    }

    #[test]
    fn accessors_expose_what_the_verifier_needs() {
        let wire = BoardWire::Post(post());
        assert_eq!(wire.signature(), [0x33; SIGNATURE_LEN]);
        assert_eq!(wire.author_signing_key(), [0x22; SIGNING_KEY_LEN]);
        assert_eq!(wire.signing_bytes(), post().signing_bytes());

        let wire = BoardWire::Tombstone(tombstone());
        assert_eq!(wire.signature(), [0x66; SIGNATURE_LEN]);
        assert_eq!(wire.signing_bytes(), tombstone().signing_bytes());
    }
}
