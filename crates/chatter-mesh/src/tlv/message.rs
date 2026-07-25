//! The public chat message body (`MessageType::Message`, 0x02).
//!
//! Ported from `BitFoundation/BitchatMessage.swift` →
//! `toBinaryPayload()` / `init?(_ data: Data)`.
//!
//! **This format is not TLV.** It is a flags byte followed by strictly ordered
//! positional fields, and the flags decide which of the trailing optional fields
//! are present at all. Get the order wrong and the whole tail shifts.
//!
//! ```text
//! [flags: 1][timestamp: 8 BE ms]
//! [idLen: 1][id][senderLen: 1][sender][contentLen: 2 BE][content]
//! (flag 0x04) [originalSenderLen: 1][originalSender]
//! (flag 0x08) [recipientNicknameLen: 1][recipientNickname]
//! (flag 0x10) [senderPeerIDLen: 1][senderPeerID]
//! (flag 0x20) [mentionCount: 1] then mentionCount × ([len: 1][nickname])
//! ```
//!
//! Flags: bit 0 `isRelay`, 1 `isPrivate`, 2 `hasOriginalSender`,
//! 3 `hasRecipientNickname`, 4 `hasSenderPeerID`, 5 `hasMentions`,
//! 6 `isBridged`. Bit 7 is unassigned and preserved by neither side.
//!
//! # Two quirks of the Swift that are reproduced deliberately
//!
//! 1. **The mentions flag and the mentions field disagree.** The flag is set
//!    only for a non-empty list, but the count byte is written whenever the
//!    list is non-`nil`. `Some(vec![])` therefore encodes a trailing `0x00`
//!    that no decoder reads, and decodes back as `None`. Round-tripping is
//!    lossy for exactly that one input.
//! 2. **A truncated optional field consumes its length byte and stops there.**
//!    Swift reads the length, finds the value runs past the end, leaves the
//!    field `nil`, and does *not* rewind — so the next field starts one byte
//!    after the bad length. The mention loop then walks forward a byte at a
//!    time. It is strange, but it is what shipping clients do, and a decoder
//!    that "fixed" it would disagree with them about where fields begin.
//!
//! Long strings are truncated to their byte prefix, not rejected — `prefix(255)`
//! on UTF-8 `Data` can split a multi-byte character, so bitchat can emit a
//! payload its own decoder cannot fully read (it substitutes `"unknown"` or a
//! fresh UUID). We truncate identically, byte for byte, and report
//! [`MeshError::InvalidUtf8`] rather than inventing a substitute.

use crate::error::MeshError;
use crate::tlv::{Reader, utf8};

const FLAG_IS_RELAY: u8 = 0x01;
const FLAG_IS_PRIVATE: u8 = 0x02;
const FLAG_HAS_ORIGINAL_SENDER: u8 = 0x04;
const FLAG_HAS_RECIPIENT_NICKNAME: u8 = 0x08;
const FLAG_HAS_SENDER_PEER_ID: u8 = 0x10;
const FLAG_HAS_MENTIONS: u8 = 0x20;
const FLAG_IS_BRIDGED: u8 = 0x40;

/// Shortest possible payload: flags + timestamp + three empty length fields.
pub const MIN_ENCODED_LEN: usize = 13;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitchatMessagePayload {
    pub id: String,
    pub sender: String,
    pub content: String,
    /// Milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    pub is_relay: bool,
    pub is_private: bool,
    /// True when the message arrived across a mesh bridge rather than over
    /// local radio.
    pub is_bridged: bool,
    pub original_sender: Option<String>,
    pub recipient_nickname: Option<String>,
    /// bitchat's `PeerID` string form, e.g. 16 lowercase hex chars, or a
    /// prefixed form like `nostr:…`. Kept as the raw string: their `PeerID`
    /// lowercases the part after the prefix on parse, and re-encoding a
    /// normalised copy would not be byte-identical to what the peer sent.
    pub sender_peer_id: Option<String>,
    /// `Some(vec![])` is representable but encodes to a stray zero byte with
    /// the mentions flag clear — see the module docs.
    pub mentions: Option<Vec<String>>,
}

impl BitchatMessagePayload {
    /// A plain broadcast message with no optional fields set.
    pub fn new(id: String, sender: String, content: String, timestamp_ms: u64) -> Self {
        Self {
            id,
            sender,
            content,
            timestamp_ms,
            is_relay: false,
            is_private: false,
            is_bridged: false,
            original_sender: None,
            recipient_nickname: None,
            sender_peer_id: None,
            mentions: None,
        }
    }

    fn flags(&self) -> u8 {
        let mut flags = 0u8;
        if self.is_relay {
            flags |= FLAG_IS_RELAY;
        }
        if self.is_private {
            flags |= FLAG_IS_PRIVATE;
        }
        if self.original_sender.is_some() {
            flags |= FLAG_HAS_ORIGINAL_SENDER;
        }
        if self.recipient_nickname.is_some() {
            flags |= FLAG_HAS_RECIPIENT_NICKNAME;
        }
        if self.sender_peer_id.is_some() {
            flags |= FLAG_HAS_SENDER_PEER_ID;
        }
        // Set only for a *non-empty* list, while the field below is written for
        // any non-nil list. Quirk 1 in the module docs.
        if self.mentions.as_ref().is_some_and(|m| !m.is_empty()) {
            flags |= FLAG_HAS_MENTIONS;
        }
        if self.is_bridged {
            flags |= FLAG_IS_BRIDGED;
        }
        flags
    }

    /// Never fails — over-long fields are truncated, matching Swift. The
    /// `Result` is there for the uniform TLV surface.
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut out = Vec::with_capacity(MIN_ENCODED_LEN + self.content.len());

        out.push(self.flags());
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());

        push_u8_prefixed(&mut out, self.id.as_bytes());
        push_u8_prefixed(&mut out, self.sender.as_bytes());

        let content = self.content.as_bytes();
        let content_len = content.len().min(u16::MAX as usize);
        out.extend_from_slice(&(content_len as u16).to_be_bytes());
        out.extend_from_slice(content.get(..content_len).unwrap_or(content));

        if let Some(original_sender) = &self.original_sender {
            push_u8_prefixed(&mut out, original_sender.as_bytes());
        }
        if let Some(recipient_nickname) = &self.recipient_nickname {
            push_u8_prefixed(&mut out, recipient_nickname.as_bytes());
        }
        if let Some(sender_peer_id) = &self.sender_peer_id {
            push_u8_prefixed(&mut out, sender_peer_id.as_bytes());
        }
        if let Some(mentions) = &self.mentions {
            let count = mentions.len().min(u8::MAX as usize);
            out.push(count as u8);
            for mention in mentions.iter().take(u8::MAX as usize) {
                push_u8_prefixed(&mut out, mention.as_bytes());
            }
        }

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        if bytes.len() < MIN_ENCODED_LEN {
            return Err(MeshError::Truncated {
                offset: bytes.len(),
                needed: MIN_ENCODED_LEN - bytes.len(),
            });
        }

        let mut reader = Reader::new(bytes);
        let flags = reader.u8()?;
        let is_relay = flags & FLAG_IS_RELAY != 0;
        let is_private = flags & FLAG_IS_PRIVATE != 0;
        let has_original_sender = flags & FLAG_HAS_ORIGINAL_SENDER != 0;
        let has_recipient_nickname = flags & FLAG_HAS_RECIPIENT_NICKNAME != 0;
        let has_sender_peer_id = flags & FLAG_HAS_SENDER_PEER_ID != 0;
        let has_mentions = flags & FLAG_HAS_MENTIONS != 0;
        let is_bridged = flags & FLAG_IS_BRIDGED != 0;

        let timestamp_ms = reader.u64_be()?;

        let id_len = reader.u8()? as usize;
        let id = utf8(reader.take(id_len)?, "message.id")?;

        let sender_len = reader.u8()? as usize;
        let sender = utf8(reader.take(sender_len)?, "message.sender")?;

        let content_len = reader.u16_be()? as usize;
        let content = utf8(reader.take(content_len)?, "message.content")?;

        let original_sender =
            read_optional(&mut reader, has_original_sender, "message.original_sender")?;
        let recipient_nickname = read_optional(
            &mut reader,
            has_recipient_nickname,
            "message.recipient_nickname",
        )?;
        let sender_peer_id =
            read_optional(&mut reader, has_sender_peer_id, "message.sender_peer_id")?;

        let mut mentions: Option<Vec<String>> = None;
        if has_mentions && !reader.is_empty() {
            let count = reader.u8()? as usize;
            if count > 0 {
                // Swift seeds the array before the loop, so a mention block
                // that is entirely unreadable still yields an empty list rather
                // than `nil`.
                let mut list = Vec::new();
                for _ in 0..count {
                    if reader.is_empty() {
                        continue;
                    }
                    let len = reader.u8()? as usize;
                    if reader.remaining() >= len {
                        let value = reader.take(len)?;
                        list.push(utf8(value, "message.mention")?);
                    }
                    // Otherwise: field dropped, cursor left just past the bad
                    // length byte. Quirk 2 in the module docs.
                }
                mentions = Some(list);
            }
        }

        Ok(Self {
            id,
            sender,
            content,
            timestamp_ms,
            is_relay,
            is_private,
            is_bridged,
            original_sender,
            recipient_nickname,
            sender_peer_id,
            mentions,
        })
    }
}

/// `[len: u8][bytes]`, truncating the value to 255 bytes as Swift does.
fn push_u8_prefixed(out: &mut Vec<u8>, value: &[u8]) {
    let len = value.len().min(u8::MAX as usize);
    out.push(len as u8);
    out.extend_from_slice(value.get(..len).unwrap_or(value));
}

/// One optional trailing string, with Swift's no-rewind truncation behaviour.
fn read_optional(
    reader: &mut Reader<'_>,
    present: bool,
    field: &'static str,
) -> Result<Option<String>, MeshError> {
    if !present || reader.is_empty() {
        return Ok(None);
    }
    let len = reader.u8()? as usize;
    if reader.remaining() < len {
        // Swift leaves the field nil and does not rewind past the length byte.
        return Ok(None);
    }
    let value = reader.take(len)?;
    Ok(Some(utf8(value, field)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> BitchatMessagePayload {
        BitchatMessagePayload {
            id: "msg-1".to_string(),
            sender: "alice".to_string(),
            content: "hello mesh".to_string(),
            timestamp_ms: 1_700_000_000_123,
            is_relay: true,
            is_private: true,
            is_bridged: true,
            original_sender: Some("bob".to_string()),
            recipient_nickname: Some("carol".to_string()),
            sender_peer_id: Some("00112233445566aa".to_string()),
            mentions: Some(vec!["dave".to_string(), "erin".to_string()]),
        }
    }

    #[test]
    fn round_trips_every_field() {
        let payload = full();
        let encoded = payload.encode().unwrap();
        assert_eq!(BitchatMessagePayload::decode(&encoded).unwrap(), payload);
    }

    #[test]
    fn round_trips_the_minimal_message() {
        let payload =
            BitchatMessagePayload::new(String::new(), String::new(), String::new(), 0);
        let encoded = payload.encode().unwrap();
        assert_eq!(encoded.len(), MIN_ENCODED_LEN);
        assert_eq!(encoded, vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(BitchatMessagePayload::decode(&encoded).unwrap(), payload);
    }

    #[test]
    fn header_layout_is_exact() {
        let payload = BitchatMessagePayload::new(
            "ab".to_string(),
            "c".to_string(),
            "de".to_string(),
            0x0102_0304_0506_0708,
        );
        assert_eq!(
            payload.encode().unwrap(),
            vec![
                0x00, // flags
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // timestamp BE
                0x02, b'a', b'b', // id
                0x01, b'c', // sender
                0x00, 0x02, b'd', b'e', // content, 2-byte length
            ]
        );
    }

    #[test]
    fn every_flag_bit_maps_to_the_right_field() {
        let mut payload =
            BitchatMessagePayload::new("i".into(), "s".into(), "c".into(), 1);
        assert_eq!(payload.flags(), 0x00);

        payload.is_relay = true;
        assert_eq!(payload.flags() & FLAG_IS_RELAY, FLAG_IS_RELAY);
        payload.is_private = true;
        assert_eq!(payload.flags() & FLAG_IS_PRIVATE, FLAG_IS_PRIVATE);
        payload.original_sender = Some("o".into());
        assert_eq!(
            payload.flags() & FLAG_HAS_ORIGINAL_SENDER,
            FLAG_HAS_ORIGINAL_SENDER
        );
        payload.recipient_nickname = Some("r".into());
        assert_eq!(
            payload.flags() & FLAG_HAS_RECIPIENT_NICKNAME,
            FLAG_HAS_RECIPIENT_NICKNAME
        );
        payload.sender_peer_id = Some("p".into());
        assert_eq!(
            payload.flags() & FLAG_HAS_SENDER_PEER_ID,
            FLAG_HAS_SENDER_PEER_ID
        );
        payload.mentions = Some(vec!["m".into()]);
        assert_eq!(payload.flags() & FLAG_HAS_MENTIONS, FLAG_HAS_MENTIONS);
        payload.is_bridged = true;
        assert_eq!(payload.flags(), 0x7F);
    }

    #[test]
    fn optional_fields_appear_in_declaration_order() {
        let mut payload =
            BitchatMessagePayload::new("i".into(), "s".into(), "c".into(), 0);
        payload.original_sender = Some("O".into());
        payload.recipient_nickname = Some("R".into());
        payload.sender_peer_id = Some("P".into());
        payload.mentions = Some(vec!["M".into()]);

        let encoded = payload.encode().unwrap();
        let tail = encoded.get(MIN_ENCODED_LEN + 3..).unwrap();
        assert_eq!(
            tail,
            &[1, b'O', 1, b'R', 1, b'P', 1, 1, b'M'],
            "original sender, recipient nickname, peer id, then mentions"
        );
        assert_eq!(BitchatMessagePayload::decode(&encoded).unwrap(), payload);
    }

    #[test]
    fn empty_mentions_list_encodes_a_stray_zero_and_decodes_as_absent() {
        let mut payload =
            BitchatMessagePayload::new("i".into(), "s".into(), "c".into(), 0);
        payload.mentions = Some(Vec::new());

        let encoded = payload.encode().unwrap();
        assert_eq!(encoded.last().copied(), Some(0x00));
        assert_eq!(encoded.first().copied(), Some(0x00), "mentions flag is clear");

        // Lossy by design — this is quirk 1, mirrored from Swift.
        let decoded = BitchatMessagePayload::decode(&encoded).unwrap();
        assert_eq!(decoded.mentions, None);
    }

    #[test]
    fn mention_count_is_capped_at_255() {
        let mut payload =
            BitchatMessagePayload::new("i".into(), "s".into(), "c".into(), 0);
        payload.mentions = Some((0..300).map(|i| format!("m{i}")).collect());
        let encoded = payload.encode().unwrap();
        let decoded = BitchatMessagePayload::decode(&encoded).unwrap();
        assert_eq!(decoded.mentions.map(|m| m.len()), Some(255));
    }

    #[test]
    fn oversized_fields_are_truncated_not_rejected() {
        let mut payload = BitchatMessagePayload::new(
            "i".repeat(300),
            "s".repeat(300),
            "c".repeat(70_000),
            0,
        );
        payload.original_sender = Some("o".repeat(300));

        let encoded = payload.encode().unwrap();
        let decoded = BitchatMessagePayload::decode(&encoded).unwrap();
        assert_eq!(decoded.id.len(), 255);
        assert_eq!(decoded.sender.len(), 255);
        assert_eq!(decoded.content.len(), 65_535);
        assert_eq!(decoded.original_sender.map(|s| s.len()), Some(255));
    }

    #[test]
    fn maximum_length_fields_round_trip() {
        let mut payload = BitchatMessagePayload::new(
            "i".repeat(255),
            "s".repeat(255),
            "c".repeat(65_535),
            u64::MAX,
        );
        payload.mentions = Some(vec!["m".repeat(255)]);
        let decoded =
            BitchatMessagePayload::decode(&payload.encode().unwrap()).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn short_input_is_rejected_before_any_field_is_read() {
        for len in 0..MIN_ENCODED_LEN {
            assert!(matches!(
                BitchatMessagePayload::decode(&vec![0u8; len]),
                Err(MeshError::Truncated { .. })
            ));
        }
        assert!(BitchatMessagePayload::decode(&[0u8; MIN_ENCODED_LEN]).is_ok());
    }

    #[test]
    fn truncation_at_every_boundary_is_total() {
        let encoded = full().encode().unwrap();
        for cut in 0..encoded.len() {
            // Must never panic. Some prefixes legitimately decode, because the
            // optional tail is allowed to be missing.
            let _ = BitchatMessagePayload::decode(&encoded[..cut]);
        }
        // A declared id length past the end is a hard error.
        let mut bad = encoded.clone();
        if let Some(slot) = bad.get_mut(9) {
            *slot = 0xFF;
        }
        assert!(matches!(
            BitchatMessagePayload::decode(&bad),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn a_truncated_optional_leaves_the_field_absent() {
        let mut payload =
            BitchatMessagePayload::new("i".into(), "s".into(), "c".into(), 0);
        payload.recipient_nickname = Some("carol".into());
        let mut encoded = payload.encode().unwrap();
        // Claim 200 bytes of nickname but supply 5.
        let len_index = encoded.len() - 6;
        if let Some(slot) = encoded.get_mut(len_index) {
            *slot = 200;
        }
        let decoded = BitchatMessagePayload::decode(&encoded).unwrap();
        assert_eq!(decoded.recipient_nickname, None);
        assert_eq!(decoded.content, "c");
    }

    #[test]
    fn a_missing_optional_tail_is_tolerated() {
        // Flags claim every optional field, but the payload ends after content.
        let bytes = [FLAG_HAS_ORIGINAL_SENDER
            | FLAG_HAS_RECIPIENT_NICKNAME
            | FLAG_HAS_SENDER_PEER_ID
            | FLAG_HAS_MENTIONS, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let decoded = BitchatMessagePayload::decode(&bytes).unwrap();
        assert_eq!(decoded.original_sender, None);
        assert_eq!(decoded.recipient_nickname, None);
        assert_eq!(decoded.sender_peer_id, None);
        assert_eq!(decoded.mentions, None);
    }

    #[test]
    fn unreadable_mention_block_yields_an_empty_list() {
        // hasMentions, count = 3, then a single length byte claiming 9 bytes.
        let mut bytes = vec![FLAG_HAS_MENTIONS, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        bytes.push(3);
        bytes.push(9);
        let decoded = BitchatMessagePayload::decode(&bytes).unwrap();
        assert_eq!(decoded.mentions, Some(Vec::new()));
    }

    #[test]
    fn invalid_utf8_is_rejected_rather_than_substituted() {
        let mut bytes = vec![0u8; MIN_ENCODED_LEN];
        if let Some(slot) = bytes.get_mut(9) {
            *slot = 1;
        }
        bytes.insert(10, 0xFF);
        assert!(matches!(
            BitchatMessagePayload::decode(&bytes),
            Err(MeshError::InvalidUtf8 { field: "message.id" })
        ));
    }

    #[test]
    fn unassigned_flag_bit_seven_does_not_break_decoding() {
        let mut encoded = full().encode().unwrap();
        if let Some(slot) = encoded.first_mut() {
            *slot |= 0x80;
        }
        let decoded = BitchatMessagePayload::decode(&encoded).unwrap();
        assert_eq!(decoded.content, "hello mesh");
    }
}
