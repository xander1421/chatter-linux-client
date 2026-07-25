//! Binary file, image, or voice-note payload (`MessageType::FileTransfer`, 0x22).
//!
//! Ported from `Protocols/BitchatFilePacket.swift`, whose TLV tags come from the
//! Android client — this is the one format in the crate defined by a third
//! implementation, so the "canonical" and "legacy" length widths below are not
//! historical debt we may clean up.
//!
//! | Type | Field | Length width |
//! |---|---|---|
//! | 0x01 | fileName (UTF-8) | 2 bytes BE |
//! | 0x02 | fileSize (u32 BE) | 2 bytes BE, value 4 |
//! | 0x03 | mimeType (UTF-8) | 2 bytes BE |
//! | 0x04 | content | **4 bytes BE** |
//!
//! The content TLV is the only one with a four-byte length, because a 2-byte one
//! could not describe a payload above 64 KiB. Decoding tries four bytes first
//! and falls back to two when the four-byte read yields a length that could not
//! possibly fit in the remaining buffer — that is how a legacy encoder's 2-byte
//! content TLV is still understood.
//!
//! Multiple content TLVs concatenate, which is how the Android encoder chunks
//! large payloads.

use crate::error::MeshError;
use crate::tlv::{Reader, put_tlv_u16_len, put_tlv_u32_len, utf8};

const TLV_FILE_NAME: u8 = 0x01;
const TLV_FILE_SIZE: u8 = 0x02;
const TLV_MIME_TYPE: u8 = 0x03;
const TLV_CONTENT: u8 = 0x04;

/// Absolute ceiling for any file payload, from `FileTransferLimits`.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitchatFilePacket {
    pub file_name: Option<String>,
    /// Declared size. `None` means "use the content length", which is what
    /// `encode` writes and what `decode` fills in when the TLV is absent.
    pub file_size: Option<u64>,
    pub mime_type: Option<String>,
    pub content: Vec<u8>,
}

impl BitchatFilePacket {
    pub fn new(
        file_name: Option<String>,
        file_size: Option<u64>,
        mime_type: Option<String>,
        content: Vec<u8>,
    ) -> Self {
        Self {
            file_name,
            file_size,
            mime_type,
            content,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        let resolved_size = self.file_size.unwrap_or(self.content.len() as u64);
        if resolved_size > u32::MAX as u64 {
            return Err(MeshError::LengthExceeded {
                declared: resolved_size.min(usize::MAX as u64) as usize,
                max: u32::MAX as usize,
            });
        }
        if resolved_size > MAX_PAYLOAD_BYTES as u64 {
            return Err(MeshError::LengthExceeded {
                declared: resolved_size as usize,
                max: MAX_PAYLOAD_BYTES,
            });
        }
        if self.content.len() > MAX_PAYLOAD_BYTES {
            return Err(MeshError::LengthExceeded {
                declared: self.content.len(),
                max: MAX_PAYLOAD_BYTES,
            });
        }

        let mut out = Vec::with_capacity(16 + self.content.len());

        // Swift drops a name or MIME type longer than 65535 bytes rather than
        // failing the encode; the file itself is still worth sending.
        if let Some(name) = &self.file_name {
            if name.len() <= u16::MAX as usize {
                put_tlv_u16_len(&mut out, TLV_FILE_NAME, name.as_bytes())?;
            }
        }

        put_tlv_u16_len(&mut out, TLV_FILE_SIZE, &(resolved_size as u32).to_be_bytes())?;

        if let Some(mime) = &self.mime_type {
            if mime.len() <= u16::MAX as usize {
                put_tlv_u16_len(&mut out, TLV_MIME_TYPE, mime.as_bytes())?;
            }
        }

        put_tlv_u32_len(&mut out, TLV_CONTENT, &self.content)?;

        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MeshError> {
        let mut reader = Reader::new(bytes);

        let mut file_name: Option<String> = None;
        let mut file_size: Option<u64> = None;
        let mut mime_type: Option<String> = None;
        let mut content: Vec<u8> = Vec::new();

        while !reader.is_empty() {
            let tlv_type = reader.u8()?;

            let length = if tlv_type == TLV_CONTENT {
                // Canonical four-byte length, with a two-byte fallback for the
                // legacy encoder. The four-byte read is accepted only if it
                // describes a value that fits in what is left, which is what
                // makes the two encodings distinguishable in practice.
                let snapshot = reader.position();
                let canonical = reader.u32_be().ok().map(|value| value as usize);
                match canonical {
                    Some(value) if value <= reader.remaining() => value,
                    _ => {
                        reader.seek(snapshot);
                        reader.u16_be()? as usize
                    }
                }
            } else {
                reader.u16_be()? as usize
            };

            let value = reader.take(length)?;

            match tlv_type {
                TLV_FILE_NAME => file_name = Some(utf8(value, "file.file_name")?),
                TLV_FILE_SIZE => {
                    // Any other width is ignored outright, including the
                    // 8-byte legacy form's siblings.
                    if length == 4 || length == 8 {
                        let mut size: u64 = 0;
                        for byte in value {
                            size = (size << 8) | *byte as u64;
                        }
                        if size > MAX_PAYLOAD_BYTES as u64 {
                            return Err(MeshError::LengthExceeded {
                                declared: size.min(usize::MAX as u64) as usize,
                                max: MAX_PAYLOAD_BYTES,
                            });
                        }
                        file_size = Some(size);
                    }
                }
                TLV_MIME_TYPE => mime_type = Some(utf8(value, "file.mime_type")?),
                TLV_CONTENT => {
                    if content.len() + value.len() > MAX_PAYLOAD_BYTES {
                        return Err(MeshError::LengthExceeded {
                            declared: content.len() + value.len(),
                            max: MAX_PAYLOAD_BYTES,
                        });
                    }
                    content.extend_from_slice(value);
                }
                // Forward compatibility: unknown types are skipped, their
                // length read as the two-byte form.
                _ => {}
            }
        }

        if content.is_empty() {
            return Err(MeshError::MissingField("file.content"));
        }

        Ok(Self {
            file_name,
            file_size: Some(file_size.unwrap_or(content.len() as u64)),
            mime_type,
            content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BitchatFilePacket {
        BitchatFilePacket::new(
            Some("note.m4a".to_string()),
            None,
            Some("audio/mp4".to_string()),
            vec![0x11, 0x22, 0x33],
        )
    }

    #[test]
    fn round_trips_and_fills_in_the_size() {
        let encoded = sample().encode().unwrap();
        let decoded = BitchatFilePacket::decode(&encoded).unwrap();
        assert_eq!(decoded.file_name.as_deref(), Some("note.m4a"));
        assert_eq!(decoded.mime_type.as_deref(), Some("audio/mp4"));
        assert_eq!(decoded.content, vec![0x11, 0x22, 0x33]);
        assert_eq!(decoded.file_size, Some(3));
    }

    #[test]
    fn content_length_is_four_bytes_and_metadata_length_is_two() {
        let packet = BitchatFilePacket::new(Some("a".into()), None, None, vec![0xFF]);
        assert_eq!(
            packet.encode().unwrap(),
            vec![
                TLV_FILE_NAME, 0x00, 0x01, b'a',
                TLV_FILE_SIZE, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01,
                TLV_CONTENT, 0x00, 0x00, 0x00, 0x01, 0xFF,
            ]
        );
    }

    #[test]
    fn tlvs_are_emitted_in_the_android_order() {
        let encoded = sample().encode().unwrap();
        assert_eq!(encoded.first().copied(), Some(TLV_FILE_NAME));
        let after_name = 3 + "note.m4a".len();
        assert_eq!(encoded.get(after_name).copied(), Some(TLV_FILE_SIZE));
    }

    #[test]
    fn an_explicit_size_overrides_the_content_length() {
        let packet = BitchatFilePacket::new(None, Some(999), None, vec![0u8; 4]);
        let decoded = BitchatFilePacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded.file_size, Some(999));
        assert_eq!(decoded.content.len(), 4);
    }

    #[test]
    fn metadata_is_optional() {
        let packet = BitchatFilePacket::new(None, None, None, vec![1, 2]);
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded.first().copied(), Some(TLV_FILE_SIZE));
        let decoded = BitchatFilePacket::decode(&encoded).unwrap();
        assert_eq!(decoded.file_name, None);
        assert_eq!(decoded.mime_type, None);
    }

    #[test]
    fn a_legacy_two_byte_content_length_still_decodes() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_FILE_SIZE, &2u32.to_be_bytes()).unwrap();
        // Legacy encoder: content with a *two* byte length.
        put_tlv_u16_len(&mut bytes, TLV_CONTENT, &[0xAB, 0xCD]).unwrap();
        let decoded = BitchatFilePacket::decode(&bytes).unwrap();
        assert_eq!(decoded.content, vec![0xAB, 0xCD]);
        assert_eq!(decoded.file_size, Some(2));
    }

    #[test]
    fn a_legacy_eight_byte_file_size_still_decodes() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_FILE_SIZE, &7u64.to_be_bytes()).unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[0u8; 7]).unwrap();
        assert_eq!(
            BitchatFilePacket::decode(&bytes).unwrap().file_size,
            Some(7)
        );
    }

    #[test]
    fn an_odd_width_file_size_is_ignored() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_FILE_SIZE, &[0x01, 0x02]).unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[0u8; 3]).unwrap();
        // Falls back to the content length rather than failing.
        assert_eq!(
            BitchatFilePacket::decode(&bytes).unwrap().file_size,
            Some(3)
        );
    }

    #[test]
    fn multiple_content_tlvs_concatenate() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_FILE_SIZE, &4u32.to_be_bytes()).unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[1, 2]).unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[3, 4]).unwrap();
        assert_eq!(
            BitchatFilePacket::decode(&bytes).unwrap().content,
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn unknown_tlvs_are_skipped() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, 0x7A, &[0xDE, 0xAD]).unwrap();
        put_tlv_u16_len(&mut bytes, TLV_FILE_SIZE, &1u32.to_be_bytes()).unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[0x01]).unwrap();
        put_tlv_u16_len(&mut bytes, 0x7B, &[]).unwrap();
        assert_eq!(BitchatFilePacket::decode(&bytes).unwrap().content, vec![1]);
    }

    #[test]
    fn empty_content_is_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_FILE_SIZE, &0u32.to_be_bytes()).unwrap();
        assert!(matches!(
            BitchatFilePacket::decode(&bytes),
            Err(MeshError::MissingField("file.content"))
        ));

        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[]).unwrap();
        assert!(matches!(
            BitchatFilePacket::decode(&bytes),
            Err(MeshError::MissingField("file.content"))
        ));
    }

    #[test]
    fn a_declared_size_above_the_cap_is_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(
            &mut bytes,
            TLV_FILE_SIZE,
            &((MAX_PAYLOAD_BYTES + 1) as u32).to_be_bytes(),
        )
        .unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[1]).unwrap();
        assert!(matches!(
            BitchatFilePacket::decode(&bytes),
            Err(MeshError::LengthExceeded {
                max: MAX_PAYLOAD_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn oversized_payloads_are_rejected_on_encode() {
        let packet = BitchatFilePacket::new(None, None, None, vec![0u8; MAX_PAYLOAD_BYTES + 1]);
        assert!(matches!(
            packet.encode(),
            Err(MeshError::LengthExceeded {
                max: MAX_PAYLOAD_BYTES,
                ..
            })
        ));

        let packet = BitchatFilePacket::new(None, Some(u64::MAX), None, vec![1]);
        assert!(matches!(
            packet.encode(),
            Err(MeshError::LengthExceeded { .. })
        ));
    }

    #[test]
    fn a_maximum_size_payload_round_trips() {
        let packet = BitchatFilePacket::new(None, None, None, vec![0x5A; MAX_PAYLOAD_BYTES]);
        let decoded = BitchatFilePacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded.content.len(), MAX_PAYLOAD_BYTES);
        assert_eq!(decoded.file_size, Some(MAX_PAYLOAD_BYTES as u64));
    }

    #[test]
    fn truncation_at_every_boundary_is_total() {
        let encoded = sample().encode().unwrap();
        for cut in 0..encoded.len() {
            // Prefixes that happen to end on a TLV boundary decode only if a
            // content TLV was already complete; nothing may panic.
            if let Ok(decoded) = BitchatFilePacket::decode(&encoded[..cut]) {
                assert!(!decoded.content.is_empty());
            }
        }
        assert!(BitchatFilePacket::decode(&encoded).is_ok());
    }

    #[test]
    fn a_length_running_past_the_end_is_rejected() {
        let bytes = [TLV_FILE_NAME, 0xFF, 0xFF, b'a'];
        assert!(matches!(
            BitchatFilePacket::decode(&bytes),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn a_dangling_type_byte_is_truncation() {
        let mut encoded = sample().encode().unwrap();
        encoded.push(TLV_MIME_TYPE);
        assert!(matches!(
            BitchatFilePacket::decode(&encoded),
            Err(MeshError::Truncated { .. })
        ));
    }

    #[test]
    fn invalid_utf8_metadata_is_rejected() {
        let mut bytes = Vec::new();
        put_tlv_u16_len(&mut bytes, TLV_FILE_NAME, &[0xFF, 0xFE]).unwrap();
        put_tlv_u32_len(&mut bytes, TLV_CONTENT, &[1]).unwrap();
        assert!(matches!(
            BitchatFilePacket::decode(&bytes),
            Err(MeshError::InvalidUtf8 {
                field: "file.file_name"
            })
        ));
    }

    #[test]
    fn empty_metadata_strings_round_trip() {
        let packet =
            BitchatFilePacket::new(Some(String::new()), None, Some(String::new()), vec![1]);
        let decoded = BitchatFilePacket::decode(&packet.encode().unwrap()).unwrap();
        assert_eq!(decoded.file_name.as_deref(), Some(""));
        assert_eq!(decoded.mime_type.as_deref(), Some(""));
    }
}
