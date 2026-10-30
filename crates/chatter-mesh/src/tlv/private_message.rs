//! A direct message body, carried inside a Noise session.
//!
//! Ported from `Protocols/Packets.swift` → `PrivateMessagePacket`. This is the
//! body of a `NoisePayloadType.privateMessage` (0x01), so it is never seen by a
//! relay — only by the two endpoints of the session.
//!
//! Wire format: `type:u8 ‖ length:u8 ‖ value` TLVs, one-byte lengths.
//!
//! | Type | Field |
//! |---|---|
//! | 0x00 | messageID (UTF-8) |
//! | 0x01 | content (UTF-8) |
//!
//! Note the type numbering: `messageID` is **0x00**, not 0x01. It does not line
//! up with `AnnouncementPacket`'s table, and there is no reason it should — the
//! two namespaces are unrelated.
//!
//! Unlike every other TLV format in this crate, this decoder is **strict**: an
//! unrecognised type byte fails the whole packet rather than being skipped
//! (`guard let type = TLVType(rawValue:) else { return nil }`). That is a real
//! asymmetry in bitchat's source, not an oversight in this port — mirroring it
//! matters, because a future bitchat that adds TLV 0x02 here would break old
//! clients, and we must fail the same way they do rather than half-decode.

use crate::error::MeshError;
use crate::tlv::{Reader, put_tlv_u8_len, utf8};

const TLV_MESSAGE_ID: u8 = 0x00;
const TLV_CONTENT: u8 = 0x01;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateMessagePacket {
    pub message_id: String,
    pub content: String,
}

impl PrivateMessagePacket {
    pub fn new(message_id: String, content: String) -> Self {
        Self {
            message_id,
            content,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let mut out = Vec::with_capacity(4 + self.message_id.len() + self.content.len());
        put_tlv_u8_len(&mut out, TLV_MESSAGE_ID, self.message_id.as_bytes())?;
        put_tlv_u8_len(&mut out, TLV_CONTENT, self.content.as_bytes())?;
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);
        let mut message_id: Option<String> = None;
        let mut content: Option<String> = None;

        // `while offset + 2 <= data.count`: a lone trailing byte is ignored.
        while reader.remaining() >= 2 {
            let offset = reader.position();
            let tlv_type = reader.u8()?;
            let length = reader.u8()? as usize;
            let value = reader.take(length)?;

            match tlv_type {
                TLV_MESSAGE_ID => message_id = Some(utf8(value, "private_message.message_id")?),
                TLV_CONTENT => content = Some(utf8(value, "private_message.content")?),
                // Strict, per Swift: unknown types are rejected here.
                _ => return Err(MeshError::MalformedTlv { offset }),
            }
        }

        Ok(Self {
            message_id: message_id.ok_or(MeshError::MissingField("private_message.message_id"))?,
            content: content.ok_or(MeshError::MissingField("private_message.content"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let packet = PrivateMessagePacket::new(
            "3F2504E0-4F89-11D3-9A0C-0305E82C3301".to_string(),
            "hey there".to_string(),
        );
        let encoded = packet.encode().unwrap();
        assert_eq!(PrivateMessagePacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn message_id_uses_type_zero() {
        let packet = PrivateMessagePacket::new("ab".to_string(), "c".to_string());
        assert_eq!(
            packet.encode().unwrap(),
            vec![0x00, 0x02, b'a', b'b', 0x01, 0x01, b'c']
        );
    }

    #[test]
    fn empty_strings_round_trip() {
        let packet = PrivateMessagePacket::new(String::new(), String::new());
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded, vec![0x00, 0x00, 0x01, 0x00]);
        assert_eq!(PrivateMessagePacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn maximum_length_fields_round_trip() {
        let packet = PrivateMessagePacket::new("i".repeat(255), "c".repeat(255));
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded.len(), 2 + 255 + 2 + 255);
        assert_eq!(PrivateMessagePacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn oversize_field_fails_the_encode() {
        let packet = PrivateMessagePacket::new("i".repeat(256), String::new());
        assert!(matches!(
            packet.encode(),
            Err(MeshError::LengthExceeded { declared: 256, .. })
        ));
    }

    #[test]
    fn unknown_tlv_type_is_rejected() {
        let bytes = [0x00, 0x01, b'a', 0x01, 0x01, b'b', 0x02, 0x00];
        assert!(matches!(
            PrivateMessagePacket::decode(&bytes),
            Err(MeshError::MalformedTlv { offset: 6 })
        ));
    }

    #[test]
    fn missing_fields_are_reported() {
        assert!(matches!(
            PrivateMessagePacket::decode(&[]),
            Err(MeshError::MissingField("private_message.message_id"))
        ));
        assert!(matches!(
            PrivateMessagePacket::decode(&[0x00, 0x01, b'a']),
            Err(MeshError::MissingField("private_message.content"))
        ));
    }

    #[test]
    fn truncated_value_is_rejected_at_every_boundary() {
        let packet = PrivateMessagePacket::new("abcd".to_string(), "xyz".to_string());
        let encoded = packet.encode().unwrap();
        for cut in 0..encoded.len() {
            assert!(PrivateMessagePacket::decode(&encoded[..cut]).is_err());
        }
        assert!(PrivateMessagePacket::decode(&encoded).is_ok());
    }

    #[test]
    fn a_lone_trailing_byte_is_ignored() {
        let packet = PrivateMessagePacket::new("a".to_string(), "b".to_string());
        let mut encoded = packet.encode().unwrap();
        encoded.push(0x99);
        assert_eq!(PrivateMessagePacket::decode(&encoded).unwrap(), packet);
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let bytes = [0x00, 0x01, 0xFF, 0x01, 0x00];
        assert!(matches!(
            PrivateMessagePacket::decode(&bytes),
            Err(MeshError::InvalidUtf8 {
                field: "private_message.message_id"
            })
        ));
    }
}
